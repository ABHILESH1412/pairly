package dev.pairly.android

import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.net.Uri
import android.os.Build
import android.provider.OpenableColumns
import android.provider.Settings
import android.util.Log
import dev.pairly.core.ffi.BatteryData
import dev.pairly.core.ffi.Device
import dev.pairly.core.ffi.DeviceKind
import dev.pairly.core.ffi.Event
import dev.pairly.core.ffi.EventListener
import dev.pairly.core.ffi.Node
import dev.pairly.core.ffi.MediaActionData
import dev.pairly.core.ffi.NodeOptions
import dev.pairly.core.ffi.LaserActionData
import dev.pairly.core.ffi.NodeSetup
import dev.pairly.core.ffi.PowerActionData
import dev.pairly.android.device.PhonePower
import dev.pairly.core.ffi.NotificationData
import dev.pairly.core.ffi.PairlyException
import dev.pairly.core.ffi.TransferData
import dev.pairly.core.ffi.TransferStatus
import dev.pairly.android.bluetooth.AndroidBluetooth
import dev.pairly.android.bluetooth.PairlyBluetooth
import dev.pairly.android.device.DeviceFeatures
import dev.pairly.android.media.MediaFeatures
import dev.pairly.android.phone.CallControl
import dev.pairly.android.phone.Calls
import dev.pairly.android.phone.PhoneContacts
import dev.pairly.android.phone.SharedFiles
import dev.pairly.android.phone.PcCommands
import dev.pairly.android.phone.Sms
import dev.pairly.core.ffi.ButtonActionData
import dev.pairly.core.ffi.CallStateData
import dev.pairly.core.ffi.CommandData
import dev.pairly.core.ffi.KeyData
import dev.pairly.core.ffi.MessageData
import dev.pairly.core.ffi.ModifiersData
import dev.pairly.core.ffi.MouseButtonData
import dev.pairly.android.media.PcPlayers
import dev.pairly.android.media.PhoneMedia
import dev.pairly.android.notifications.MirroredNotifications
import dev.pairly.android.notifications.PhoneNotifications
import dev.pairly.android.share.Downloads
import dev.pairly.android.share.ShareEvent
import dev.pairly.android.share.ShareFeatures
import dev.pairly.android.share.ShareNotifications
import dev.pairly.android.share.SharePrefs
import java.io.File
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.SharedFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asSharedFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext

data class SelfInfo(val name: String, val id: String)

data class PairingPrompt(val id: String, val name: String, val code: String, val incoming: Boolean)

data class UiState(
    val self: SelfInfo? = null,
    val starting: Boolean = false,
    val error: String? = null,
    val devices: List<Device> = emptyList(),
)

/**
 * Process-wide owner of the Rust node. [PairlyService] starts and stops it; the UI observes
 * [state], [pairing] and [messages].
 */
object Pairly {
    private const val TAG = "Pairly"
    private const val LAN_PORT: UShort = 47100u
    private const val NETWORK_DEBOUNCE_MS = 1_000L

    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
    private val lifecycle = Mutex()
    private val events = Channel<Event>(Channel.UNLIMITED)
    private val shareEvents = Channel<ShareEvent>(Channel.UNLIMITED)
    private const val NOTIFY_INTERVAL_MS = 1_000L
    private lateinit var appContext: Context

    @Volatile
    private var node: Node? = null
    private var networkRefresh: Job? = null

    private val _state = MutableStateFlow(UiState())
    val state: StateFlow<UiState> = _state.asStateFlow()

    private val _pairing = MutableStateFlow<PairingPrompt?>(null)
    val pairing: StateFlow<PairingPrompt?> = _pairing.asStateFlow()

    private val _transfers = MutableStateFlow<Map<ULong, TransferData>>(emptyMap())
    /** Unfinished transfers in either direction. */
    val transfers: StateFlow<Map<ULong, TransferData>> = _transfers.asStateFlow()

    /** Incoming transfers → where they are being written. Touched only on [scope]'s share loop. */
    private val incoming = mutableMapOf<ULong, Uri>()
    private val lastNotified = mutableMapOf<ULong, Long>()

    private val _messages = MutableSharedFlow<String>(extraBufferCapacity = 16)
    val messages: SharedFlow<String> = _messages.asSharedFlow()

    /** Called on Rust threads; events are handled in order on [scope]. */
    private val listener = object : EventListener {
        override fun onEvent(event: Event) {
            events.trySend(event)
        }
    }

    init {
        scope.launch {
            for (event in events) handle(event)
        }
        scope.launch {
            for (event in shareEvents) {
                runCatching { handleShare(event) }.onFailure { Log.w(TAG, "share event failed", it) }
            }
        }
    }

    suspend fun start(context: Context) = lifecycle.withLock {
        if (node != null) return@withLock
        appContext = context.applicationContext
        _state.update { it.copy(starting = true, error = null) }
        try {
            val options = NodeOptions(
                name = deviceName(appContext),
                kind = deviceKind(appContext),
                dataDir = appContext.filesDir.absolutePath,
                lanPort = LAN_PORT,
                relay = null,
            )
            val started = withContext(Dispatchers.IO) {
                val setup = NodeSetup(options, KeystoreSecretStore(appContext), listener)
                setup.notifications(MirroredNotifications(appContext))
                setup.device(DeviceFeatures(appContext))
                setup.share(ShareFeatures())
                setup.bluetooth(AndroidBluetooth(appContext))
                setup.media(MediaFeatures(appContext))
                setup.sms(Sms(appContext))
                setup.commands(PcCommands(appContext))
                setup.telephony(CallControl(appContext))
                setup.contacts(PhoneContacts(appContext))
                setup.files(SharedFiles(appContext))
                setup.power(PhonePower())
                setup.start().also { setup.close() }
            }
            node = started
            PhoneNotifications.sink = object : PhoneNotifications.Sink {
                override fun posted(notification: NotificationData) = started.notificationPosted(notification)
                override fun removed(id: String) = started.notificationRemoved(id)
            }
            PhoneNotifications.resend(appContext)
            DeviceFeatures.current(appContext)?.let { started.batteryChanged(it) }
            bluetoothChanged()
            withContext(Dispatchers.Main) {
                PhoneMedia.start(appContext)
                phonePermissionsChanged()
            }
            _state.update { it.copy(self = SelfInfo(started.name(), started.deviceId()), starting = false) }
            refresh()
        } catch (e: Exception) {
            Log.e(TAG, "failed to start the node", e)
            _state.update { it.copy(starting = false, error = e.describe()) }
        }
    }

    /** Stop the node (the service is going away). */
    fun stop() {
        scope.launch {
            lifecycle.withLock {
                val stopping = node ?: return@withLock
                node = null
                PairlyBluetooth.stopServer()
                PhoneMedia.stop()
                withContext(Dispatchers.Main) {
                    Calls.stop()
                    Sms.stop(appContext)
                }
                PcPlayers.clear(appContext)
                PhoneNotifications.sink = null
                stopping.shutdown()
                stopping.close()
                _state.value = UiState()
                _pairing.value = null
                _transfers.value = emptyMap()
            }
        }
    }

    /**
     * The network may have changed (new Wi-Fi, tethering, app back in the foreground): re-run
     * discovery. Android fires bursts of callbacks, so this is debounced.
     */
    fun networkChanged() {
        val n = node ?: return
        synchronized(this) {
            networkRefresh?.cancel()
            networkRefresh = scope.launch {
                delay(NETWORK_DEBOUNCE_MS)
                n.networkChanged()
            }
        }
    }

    /** Call or SMS permissions were granted: start reporting calls and texts. Main thread. */
    fun phonePermissionsChanged() {
        if (node == null) return
        Calls.start(appContext)
        Sms.watch(appContext)
    }

    fun callChanged(state: CallStateData, number: String?, contact: String?) {
        runCatching { node?.callChanged(state, number, contact) }
    }

    fun smsNew(message: MessageData, name: String?) {
        node?.smsNew(message, name)
    }

    /** Tell the PCs how sending a text went. */
    fun smsStatus(ok: Boolean, detail: String) {
        node?.smsStatus(ok, detail)
    }

    fun runCommand(device: Device, command: CommandData) {
        runCatching { node?.runCommand(device.id, command.id) }
            .onSuccess { say(appContext.getString(R.string.command_started, command.name)) }
            .onFailure { say(it.describe()) }
    }

    fun inputPointer(device: String, dx: Float, dy: Float, scrollX: Float, scrollY: Float) {
        runCatching { node?.inputPointer(device, dx, dy, scrollX, scrollY) }
    }

    fun inputButton(device: String, button: MouseButtonData, action: ButtonActionData) {
        runCatching { node?.inputButton(device, button, action) }
    }

    fun inputLaser(device: String, action: LaserActionData, dx: Float, dy: Float) {
        runCatching { node?.inputLaser(device, action, dx, dy) }
    }

    fun inputKey(device: String, text: String?, key: KeyData?, modifiers: ModifiersData) {
        runCatching { node?.inputKey(device, text, key, modifiers) }.onFailure { say(it.describe()) }
    }

    /** The phone's players changed: the core asks [MediaFeatures] and tells paired devices. */
    fun mediaChanged() {
        node?.mediaChanged()
    }

    fun mediaCommand(device: String, player: String, action: MediaActionData) {
        runCatching { node?.mediaCommand(device, player, action) }.onFailure { say(it.describe()) }
    }

    /** Bluetooth was switched on or off, or its permission granted: (re)start listening. */
    fun bluetoothChanged() {
        val n = node ?: return
        val adapterOn = PairlyBluetooth.adapter(appContext)?.isEnabled == true
        if (!adapterOn) {
            PairlyBluetooth.stopServer()
            return
        }
        PairlyBluetooth.startServer(appContext) { socket, address -> n.bluetoothIncoming(socket, address) }
    }

    fun requestPair(id: String) {
        val n = node ?: return
        scope.launch {
            try {
                n.requestPair(id)
            } catch (e: Exception) {
                say(e.describe())
            }
        }
    }

    /** Pair with the PC whose QR code was scanned; progress arrives as messages. */
    fun pairFromQr(uri: String) {
        val n = node ?: return
        say(appContext.getString(R.string.pairing_in_progress))
        scope.launch {
            try {
                n.pairFromQr(uri)
            } catch (e: Exception) {
                say(e.describe())
            }
        }
    }

    fun confirmPair(id: String, accept: Boolean) {
        _pairing.value = null
        runCatching { node?.confirmPair(id, accept) }.onFailure { say(it.describe()) }
    }

    fun ping(device: Device) {
        runCatching { node?.ping(device.id, null) }
            .onSuccess { say("Pinged ${device.name}") }
            .onFailure { say(it.describe()) }
    }

    fun mirrorDismissed(device: String, id: String) {
        runCatching { node?.mirrorDismissed(device, id) }
    }

    fun mirrorAction(device: String, id: String, action: String) {
        runCatching { node?.mirrorAction(device, id, action) }.onFailure { say(it.describe()) }
    }

    fun mirrorReply(device: String, id: String, text: String) {
        runCatching { node?.mirrorReply(device, id, text) }.onFailure { say(it.describe()) }
    }

    /** Send the phone's clipboard to one device (the app is in the foreground, so it's readable). */
    fun sendClipboard(device: Device, text: String?) {
        if (text.isNullOrEmpty()) {
            say(appContext.getString(R.string.clipboard_empty))
            return
        }
        runCatching { node?.sendClipboard(device.id, text) }
            .onSuccess { say(appContext.getString(R.string.clipboard_sent, device.name)) }
            .onFailure { say(it.describe()) }
    }

    /** From the Quick Settings tile or the notification action: every connected device. */
    fun sendClipboardToAll(text: String?) {
        val connected = _state.value.devices.filter { it.paired && it.link != null }
        if (connected.isEmpty()) {
            say(appContext.getString(R.string.clipboard_no_devices))
            return
        }
        connected.forEach { sendClipboard(it, text) }
    }

    /** Copied on the phone (noticed by the accessibility service): to every connected device. */
    fun sendClipboardQuietly(text: String) {
        _state.value.devices.filter { it.paired && it.link != null }.forEach { device ->
            runCatching { node?.sendClipboard(device.id, text) }.onFailure { Log.w(TAG, "clipboard to ${device.name} failed", it) }
        }
    }

    /** Lock, power off or restart a PC; says how it went. */
    fun power(device: Device, action: PowerActionData) {
        scope.launch {
            runCatching { node?.power(device.id, action) }
                .onSuccess {
                    val done = when (action) {
                        PowerActionData.LOCK -> R.string.power_locked
                        PowerActionData.POWER_OFF -> R.string.power_powering_off
                        PowerActionData.RESTART -> R.string.power_restarting
                    }
                    say(appContext.getString(done, device.name))
                }
                .onFailure { say(it.describe()) }
        }
    }

    fun ring(device: Device, on: Boolean) {
        runCatching { node?.ring(device.id, on) }.onFailure { say(it.describe()) }
    }

    fun batteryChanged(state: BatteryData) {
        node?.batteryChanged(state)
    }

    /** Something about a device changed outside of node events (e.g. its battery). */
    fun devicesChanged() {
        scope.launch { refresh() }
    }

    // ----- sharing ---------------------------------------------------------------------------

    fun onShare(event: ShareEvent) {
        shareEvents.trySend(event)
    }

    /**
     * Offer files to [device]. Suspends until every file is open, so a share-sheet activity can
     * finish afterwards without losing its temporary permission to read them.
     */
    suspend fun sendFiles(device: Device, uris: List<Uri>) {
        val n = node ?: return
        var sent = 0
        withContext(Dispatchers.IO) {
            for (uri in uris) {
                try {
                    val (name, fd) = openForSending(uri)
                    n.sendFile(device.id, fd, name, appContext.contentResolver.getType(uri))
                    sent++
                } catch (e: Exception) {
                    Log.w(TAG, "can't send $uri", e)
                    say(appContext.getString(R.string.share_cant_send, e.describe()))
                }
            }
        }
        if (sent > 0) {
            say(appContext.resources.getQuantityString(R.plurals.share_sending, sent, sent, device.name))
        }
    }

    /** A readable, seekable descriptor for [uri] (streams are copied to a temporary file). */
    private fun openForSending(uri: Uri): Pair<String, Int> {
        val resolver = appContext.contentResolver
        val name = resolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)?.use { c ->
            if (c.moveToFirst()) c.getString(0) else null
        } ?: uri.lastPathSegment ?: "file"
        // Rust takes over the descriptor; closing the emptied wrapper is a no-op.
        val fd = resolver.openFileDescriptor(uri, "r")?.use { pfd ->
            if (pfd.statSize >= 0) pfd.detachFd() else null
        }
        if (fd != null) return name to fd
        // A pipe (e.g. a cloud file being downloaded): Rust needs random access, so copy it.
        val temp = File.createTempFile("send-", null, appContext.cacheDir)
        try {
            resolver.openInputStream(uri)?.use { input -> temp.outputStream().use { input.copyTo(it) } }
                ?: error("can't read $name")
            val copy = android.os.ParcelFileDescriptor.open(temp, android.os.ParcelFileDescriptor.MODE_READ_ONLY)
            return name to copy.use { it.detachFd() }
        } finally {
            // The open descriptor keeps the data alive until the transfer ends.
            temp.delete()
        }
    }

    fun sendText(device: Device, text: String) {
        val url = SharePrefs.isWebUrl(text)
        runCatching { node?.sendText(device.id, text.trim(), url) }
            .onSuccess {
                say(appContext.getString(if (url) R.string.share_link_sent else R.string.share_text_sent, device.name))
            }
            .onFailure { say(it.describe()) }
    }

    fun acceptTransfer(id: ULong) {
        scope.launch(Dispatchers.IO) {
            val n = node ?: return@launch
            val t = _transfers.value[id]?.takeIf { it.status == TransferStatus.Waiting } ?: return@launch
            try {
                val (uri, fd) = Downloads.create(appContext, t.name, t.mime)
                val first = synchronized(incoming) { incoming.putIfAbsent(id, uri) == null }
                if (!first) {
                    // Accepted twice (a double tap): keep the first file.
                    android.os.ParcelFileDescriptor.adoptFd(fd).close()
                    Downloads.discard(appContext, uri)
                    return@launch
                }
                n.acceptTransfer(id, fd)
            } catch (e: Exception) {
                Log.w(TAG, "can't accept ${t.name}", e)
                synchronized(incoming) { incoming.remove(id) }?.let { Downloads.discard(appContext, it) }
                say(appContext.getString(R.string.share_cant_receive, e.describe()))
                runCatching { n.cancelTransfer(id) }
            }
        }
    }

    fun cancelTransfer(id: ULong) {
        runCatching { node?.cancelTransfer(id) }.onFailure {
            // Already over: just drop the stale notification and row.
            _transfers.value[id]?.let { t -> ShareNotifications.clear(appContext, t) }
            _transfers.update { it - id }
        }
    }

    private suspend fun handleShare(event: ShareEvent) {
        when (event) {
            is ShareEvent.Offered -> {
                val t = event.transfer
                _transfers.update { it + (t.id to t) }
                if (SharePrefs.askBeforeReceiving(appContext)) {
                    ShareNotifications.offer(appContext, t)
                } else {
                    acceptTransfer(t.id)
                }
            }
            is ShareEvent.Changed -> transferChanged(event.transfer)
            is ShareEvent.Text -> withContext(Dispatchers.Main) {
                if (event.url) {
                    ShareNotifications.link(appContext, event.fromName, event.text)
                    say(appContext.getString(R.string.share_link_from, event.fromName))
                } else {
                    appContext.getSystemService(ClipboardManager::class.java)
                        ?.setPrimaryClip(ClipData.newPlainText(appContext.getString(R.string.app_name), event.text))
                    ShareNotifications.text(appContext, event.fromName, event.text)
                    say(appContext.getString(R.string.share_text_from, event.fromName))
                }
            }
        }
    }

    private suspend fun transferChanged(t: TransferData) {
        val finished = when (t.status) {
            TransferStatus.Done, TransferStatus.Cancelled, is TransferStatus.Failed -> true
            else -> false
        }
        if (!finished) {
            val statusChanged = _transfers.value[t.id]?.status != t.status
            _transfers.update { it + (t.id to t) }
            // Waiting offers keep their Accept/Decline notification.
            if (t.status == TransferStatus.Waiting && t.incoming) return
            val now = android.os.SystemClock.elapsedRealtime()
            val last = lastNotified[t.id]
            if (last == null || statusChanged || now - last >= NOTIFY_INTERVAL_MS) {
                lastNotified[t.id] = now
                ShareNotifications.progress(appContext, t)
            }
            return
        }
        _transfers.update { it - t.id }
        lastNotified.remove(t.id)
        val uri = if (t.incoming) synchronized(incoming) { incoming.remove(t.id) } else null
        when (val status = t.status) {
            TransferStatus.Done -> if (uri != null) {
                withContext(Dispatchers.IO) { Downloads.publish(appContext, uri) }
                ShareNotifications.received(appContext, t, uri)
                say(appContext.getString(R.string.share_received, t.name))
            } else if (!t.incoming) {
                ShareNotifications.sent(appContext, t)
            }
            is TransferStatus.Failed -> {
                uri?.let { withContext(Dispatchers.IO) { Downloads.discard(appContext, it) } }
                ShareNotifications.failed(appContext, t, status.reason)
            }
            else -> {
                uri?.let { withContext(Dispatchers.IO) { Downloads.discard(appContext, it) } }
                ShareNotifications.clear(appContext, t)
            }
        }
    }

    fun unpair(device: Device) {
        runCatching { node?.unpair(device.id) }.onFailure { say(it.describe()) }
    }

    private fun refresh() {
        val n = node ?: return
        val devices = runCatching { n.devices() }.getOrElse {
            Log.w(TAG, "listing devices failed", it)
            return
        }
        _state.update { it.copy(devices = devices) }
        val connected = devices.filter { it.paired && it.link != null }.map { it.name }
        Notifications.updateService(appContext, connected)
    }

    private fun handle(event: Event) {
        when (event) {
            is Event.PairingRequested ->
                _pairing.value = PairingPrompt(event.id, event.name, event.code, event.incoming)
            is Event.Paired -> {
                _pairing.value = null
                say("Paired with ${event.name}")
            }
            is Event.PairingFailed -> {
                if (_pairing.value?.id == event.id) _pairing.value = null
                say("Pairing failed: ${event.reason}")
            }
            is Event.PingReceived -> {
                Notifications.ping(appContext, event.name, event.message)
                say(event.message?.let { "Ping from ${event.name}: $it" } ?: "Ping from ${event.name}")
            }
            else -> {}
        }
        refresh()
    }

    fun say(message: String) {
        _messages.tryEmit(message)
    }

    private fun Throwable.describe(): String = when (this) {
        is PairlyException.Failed -> reason
        is PairlyException.InvalidDevice -> "Unknown device"
        else -> message ?: javaClass.simpleName
    }

    private fun deviceName(context: Context): String {
        val name = Settings.Global.getString(context.contentResolver, Settings.Global.DEVICE_NAME)
            ?.takeIf { it.isNotBlank() }
            ?: "${Build.MANUFACTURER} ${Build.MODEL}"
        return name.filterNot(Char::isISOControl).take(64)
    }

    private fun deviceKind(context: Context): DeviceKind =
        if (context.resources.configuration.smallestScreenWidthDp >= 600) DeviceKind.TABLET else DeviceKind.PHONE
}
