package dev.pairly.android.ui

import androidx.annotation.DrawableRes
import android.net.Uri
import android.text.format.Formatter
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.outlined.PowerSettingsNew
import androidx.compose.material.icons.outlined.QrCodeScanner
import androidx.compose.material.icons.outlined.Settings
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.IconButton
import androidx.compose.material3.LargeTopAppBar
import androidx.compose.material3.ListItemDefaults
import androidx.compose.material3.TopAppBarDefaults
import androidx.compose.material3.SwitchDefaults
import androidx.compose.material3.pulltorefresh.PullToRefreshBox
import androidx.compose.foundation.layout.fillMaxSize
import kotlinx.coroutines.delay
import dev.pairly.android.AppSettings
import dev.pairly.android.PairlyService
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.input.nestedscroll.nestedScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import android.content.ClipboardManager
import androidx.compose.foundation.layout.Box
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Button
import androidx.compose.material3.ElevatedCard
import androidx.compose.material3.ExtendedFloatingActionButton
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.FilledTonalButton
import androidx.compose.material3.Icon
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.ListItem
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.tooling.preview.Preview
import androidx.compose.ui.unit.dp
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.compose.LifecycleEventEffect
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import dev.pairly.android.Pairly
import dev.pairly.android.PairingPrompt
import dev.pairly.android.R
import dev.pairly.android.SelfInfo
import dev.pairly.android.UiState
import dev.pairly.android.bluetooth.PairlyBluetooth
import dev.pairly.android.phone.Calls
import dev.pairly.android.phone.PcCommands
import dev.pairly.android.device.ClipboardSync
import dev.pairly.android.device.PairlyAccessibility
import dev.pairly.android.phone.SharedFiles
import dev.pairly.android.phone.Sms
import dev.pairly.core.ffi.CommandData
import dev.pairly.android.notifications.PairlyNotificationListener
import dev.pairly.android.share.SharePrefs
import dev.pairly.android.ui.theme.PairlyTheme
import dev.pairly.core.ffi.BatteryData
import dev.pairly.core.ffi.Device
import dev.pairly.core.ffi.DeviceKind
import dev.pairly.core.ffi.Link
import dev.pairly.core.ffi.TransferData
import dev.pairly.core.ffi.TransferStatus
import kotlinx.coroutines.launch

@Composable
fun HomeRoute() {
    val state by Pairly.state.collectAsStateWithLifecycle()
    val prompt by Pairly.pairing.collectAsStateWithLifecycle()
    val transfers by Pairly.transfers.collectAsStateWithLifecycle()
    val scope = rememberCoroutineScope()
    var sendTo by remember { mutableStateOf<Device?>(null) }
    val pickFiles = rememberLauncherForActivityResult(ActivityResultContracts.OpenMultipleDocuments()) { uris ->
        val device = sendTo ?: return@rememberLauncherForActivityResult
        sendTo = null
        if (uris.isNotEmpty()) scope.launch { Pairly.sendFiles(device, uris) }
    }
    val snackbar = remember { SnackbarHostState() }
    var scanning by rememberSaveable { mutableStateOf(false) }
    var choosingApps by rememberSaveable { mutableStateOf(false) }
    var settingsOpen by rememberSaveable { mutableStateOf(false) }
    var remoteId by rememberSaveable { mutableStateOf<String?>(null) }
    var presenterId by rememberSaveable { mutableStateOf<String?>(null) }
    var commandsFor by remember { mutableStateOf<Device?>(null) }
    var openId by rememberSaveable { mutableStateOf<String?>(null) }
    val ringing = remember { mutableStateListOf<String>() }
    val pcCommands by PcCommands.byDevice.collectAsStateWithLifecycle()
    val enabled by AppSettings.enabled.collectAsStateWithLifecycle()
    val context = LocalContext.current
    var notificationAccess by remember { mutableStateOf(PairlyNotificationListener.isEnabled(context)) }
    var bluetoothAllowed by remember { mutableStateOf(PairlyBluetooth.permitted(context)) }
    var callsAllowed by remember { mutableStateOf(Calls.permitted(context)) }
    var smsAllowed by remember { mutableStateOf(Sms.permitted(context)) }
    var filesAllowed by remember { mutableStateOf(SharedFiles.permitted(context)) }
    var controlAllowed by remember { mutableStateOf(PairlyAccessibility.enabled(context)) }
    val requestPhone = rememberLauncherForActivityResult(ActivityResultContracts.RequestMultiplePermissions()) {
        callsAllowed = Calls.permitted(context)
        smsAllowed = Sms.permitted(context)
        Pairly.phonePermissionsChanged()
    }
    val requestBluetooth = rememberLauncherForActivityResult(ActivityResultContracts.RequestPermission()) { granted ->
        bluetoothAllowed = granted
        if (granted) Pairly.bluetoothChanged()
    }
    // Re-check when coming back from the system settings screen.
    LifecycleEventEffect(Lifecycle.Event.ON_RESUME) {
        notificationAccess = PairlyNotificationListener.isEnabled(context)
        bluetoothAllowed = PairlyBluetooth.permitted(context)
        callsAllowed = Calls.permitted(context)
        smsAllowed = Sms.permitted(context)
        filesAllowed = SharedFiles.permitted(context)
        controlAllowed = PairlyAccessibility.enabled(context)
    }
    LaunchedEffect(Unit) {
        Pairly.messages.collect { snackbar.showSnackbar(it) }
    }
    if (choosingApps) {
        AppsScreen(onBack = { choosingApps = false })
        return
    }
    if (settingsOpen) {
        SettingsScreen(onBack = { settingsOpen = false }, onChooseApps = { choosingApps = true })
        return
    }
    state.devices.find { it.id == remoteId }?.let { device ->
        RemoteInputScreen(device, onBack = { remoteId = null })
        return
    }
    state.devices.find { it.id == presenterId }?.let { device ->
        PresenterScreen(device, onBack = { presenterId = null })
        return
    }
    commandsFor?.let { device ->
        CommandsDialog(device, pcCommands[device.id].orEmpty(), onDismiss = { commandsFor = null })
    }
    state.devices.find { it.id == openId && it.paired }?.let { device ->
        DeviceScreen(
            device = device,
            ringing = device.id in ringing,
            transfers = transfers.values.filter { it.deviceId == device.id },
            snackbar = snackbar,
            actions = DeviceActions(
                sendFiles = {
                    sendTo = device
                    pickFiles.launch(arrayOf("*/*"))
                },
                clipboard = {
                    // In the foreground, so Android lets us read the clipboard.
                    val clip = context.getSystemService(ClipboardManager::class.java)?.primaryClip
                    val text = clip?.takeIf { it.itemCount > 0 }?.getItemAt(0)?.coerceToText(context)?.toString()
                    Pairly.sendClipboard(device, text)
                },
                ring = { on ->
                    if (on) ringing.add(device.id) else ringing.remove(device.id)
                    Pairly.ring(device, on)
                },
                ping = { Pairly.ping(device) },
                remote = { remoteId = device.id },
                presenter = { presenterId = device.id },
                commands = pcCommands[device.id]?.takeIf { it.isNotEmpty() }?.let { { commandsFor = device } },
                power = { Pairly.power(device, it) },
                pause = { Pairly.setPaused(device, it) },
                unpair = {
                    openId = null
                    Pairly.unpair(device)
                },
                acceptTransfer = Pairly::acceptTransfer,
                cancelTransfer = Pairly::cancelTransfer,
            ),
            onBack = { openId = null },
        )
        return
    }
    if (scanning) {
        ScanScreen(
            onResult = { uri ->
                scanning = false
                Pairly.pairFromQr(uri)
            },
            onCancel = { scanning = false },
        )
        return
    }
    HomeScreen(
        state = state,
        snackbar = snackbar,
        onPair = Pairly::requestPair,
        onScan = { scanning = true },
        notificationAccess = notificationAccess,
        onChooseApps = { choosingApps = true },
        transfers = transfers.values.toList(),
        bluetoothAllowed = bluetoothAllowed,
        onSettings = { settingsOpen = true },
        enabled = enabled,
        onEnabled = { PairlyService.setEnabled(context, it) },
        onRefresh = { Pairly.networkChanged() },
        phoneFeatures = PhoneFeatures(
            calls = callsAllowed,
            texts = smsAllowed,
            onAllowCalls = { requestPhone.launch(Calls.PERMISSIONS) },
            onAllowTexts = { requestPhone.launch(Sms.PERMISSIONS) },
            files = filesAllowed,
            onAllowFiles = { context.startActivity(SharedFiles.settingsIntent(context)) },
            control = controlAllowed,
            onAllowControl = { context.startActivity(PairlyAccessibility.settingsIntent()) },
        ),
        onOpen = { openId = it.id },
        onAllowBluetooth = {
            if (android.os.Build.VERSION.SDK_INT >= android.os.Build.VERSION_CODES.S) {
                requestBluetooth.launch(android.Manifest.permission.BLUETOOTH_CONNECT)
            }
        },
    )
    prompt?.let { p ->
        PairingDialog(p, onAnswer = { accept -> Pairly.confirmPair(p.id, accept) })
    }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun HomeScreen(
    state: UiState,
    snackbar: SnackbarHostState,
    onPair: (String) -> Unit,
    onScan: () -> Unit,
    notificationAccess: Boolean = true,
    onChooseApps: () -> Unit = {},
    transfers: List<TransferData> = emptyList(),
    bluetoothAllowed: Boolean = true,
    onSettings: () -> Unit = {},
    onAllowBluetooth: () -> Unit = {},
    phoneFeatures: PhoneFeatures? = null,
    onOpen: (Device) -> Unit = {},
    enabled: Boolean = true,
    onEnabled: (Boolean) -> Unit = {},
    onRefresh: () -> Unit = {},
) {
    val paired = state.devices.filter { it.paired }
    var refreshing by remember { mutableStateOf(false) }
    val scope = rememberCoroutineScope()
    val available = state.devices.filterNot { it.paired }
    val scroll = TopAppBarDefaults.exitUntilCollapsedScrollBehavior()
    Scaffold(
        modifier = Modifier.nestedScroll(scroll.nestedScrollConnection),
        topBar = {
            LargeTopAppBar(
                title = {
                    Column {
                        Text(stringResource(R.string.app_name), fontWeight = FontWeight.SemiBold)
                        state.self?.let {
                            Text(
                                stringResource(R.string.home_this_phone, it.name),
                                style = MaterialTheme.typography.bodyMedium,
                                color = MaterialTheme.colorScheme.onSurfaceVariant,
                                maxLines = 1,
                                overflow = TextOverflow.Ellipsis,
                            )
                        }
                    }
                },
                actions = {
                    // The global switch: off stops Pairly until it's switched back on.
                    Switch(
                        checked = enabled,
                        onCheckedChange = onEnabled,
                        thumbContent = {
                            Icon(Icons.Outlined.PowerSettingsNew, contentDescription = null, modifier = Modifier.size(SwitchDefaults.IconSize))
                        },
                    )
                    IconButton(onClick = onSettings) {
                        Icon(Icons.Outlined.Settings, contentDescription = stringResource(R.string.settings_title))
                    }
                },
                scrollBehavior = scroll,
            )
        },
        snackbarHost = { SnackbarHost(snackbar) },
        floatingActionButton = {
            if (enabled && state.self != null) {
                ExtendedFloatingActionButton(
                    onClick = onScan,
                    icon = { Icon(Icons.Outlined.QrCodeScanner, contentDescription = null) },
                    text = { Text(stringResource(R.string.action_scan)) },
                )
            }
        },
    ) { padding ->
        if (!enabled) {
            PairlyOff(Modifier.padding(padding), onTurnOn = { onEnabled(true) })
            return@Scaffold
        }
        // Pull down to look for devices again.
        PullToRefreshBox(
            isRefreshing = refreshing,
            onRefresh = {
                refreshing = true
                onRefresh()
                scope.launch {
                    delay(1500)
                    refreshing = false
                }
            },
            modifier = Modifier.padding(top = padding.calculateTopPadding()),
        ) {
        LazyColumn(
            modifier = Modifier.fillMaxSize(),
            contentPadding = PaddingValues(
                start = 16.dp,
                end = 16.dp,
                top = 8.dp,
                bottom = padding.calculateBottomPadding() + 88.dp,
            ),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            if (state.starting) {
                item {
                    Column {
                        Text(stringResource(R.string.starting))
                        LinearProgressIndicator(Modifier.fillMaxWidth().padding(top = 8.dp))
                    }
                }
            }
            state.error?.let { error ->
                item {
                    Text(
                        stringResource(R.string.start_failed, error),
                        color = MaterialTheme.colorScheme.error,
                    )
                }
            }
            if (paired.isNotEmpty()) {
                item { NotificationAccessCard(notificationAccess, onChooseApps) }
                if (!bluetoothAllowed) {
                    item { BluetoothCard(onGrant = onAllowBluetooth) }
                }
                phoneFeatures?.takeIf { !it.calls || !it.texts || !it.files || !it.control }?.let { f ->
                    item { PhoneFeaturesCard(f) }
                }
                item { SectionHeader(stringResource(R.string.section_paired)) }
                items(paired, key = { it.id }) { device ->
                    DeviceSummaryCard(
                        device,
                        activeTransfers = transfers.count { it.deviceId == device.id },
                        onClick = { onOpen(device) },
                    )
                }
            }
            if (available.isNotEmpty()) {
                item { SectionHeader(stringResource(R.string.section_available)) }
                items(available, key = { it.id }) { device ->
                    Card(
                        shape = MaterialTheme.shapes.large,
                        colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surfaceContainer),
                        modifier = Modifier.fillMaxWidth(),
                    ) {
                        ListItem(
                            headlineContent = { Text(device.name, fontWeight = FontWeight.Medium) },
                            supportingContent = { Text(stringResource(R.string.available_to_pair)) },
                            leadingContent = { DeviceAvatar(device.kind, strong = false) },
                            trailingContent = {
                                FilledTonalButton(onClick = { onPair(device.id) }) { Text(stringResource(R.string.action_pair)) }
                            },
                            colors = ListItemDefaults.colors(containerColor = Color.Transparent),
                        )
                    }
                }
            }
            if (state.self != null && state.devices.isEmpty()) {
                item { Searching() }
            }
        }
        }
    }

}

/** Shown while Pairly is switched off. */
@Composable
private fun PairlyOff(modifier: Modifier, onTurnOn: () -> Unit) {
    Column(
        modifier.fillMaxSize().padding(32.dp),
        horizontalAlignment = Alignment.CenterHorizontally,
        verticalArrangement = Arrangement.Center,
    ) {
        val c = dev.pairly.android.ui.theme.Hue.GRAY.colors()
        Box(Modifier.size(96.dp).background(c.background, CircleShape), contentAlignment = Alignment.Center) {
            Icon(Icons.Outlined.PowerSettingsNew, contentDescription = null, tint = c.content, modifier = Modifier.size(44.dp))
        }
        Spacer(Modifier.height(20.dp))
        Text(stringResource(R.string.off_title), style = MaterialTheme.typography.headlineSmall, fontWeight = FontWeight.Bold)
        Spacer(Modifier.height(8.dp))
        Text(
            stringResource(R.string.off_body),
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            textAlign = TextAlign.Center,
        )
        Spacer(Modifier.height(24.dp))
        Button(onClick = onTurnOn) { Text(stringResource(R.string.off_turn_on)) }
    }
}

@Composable
private fun SectionHeader(text: String) {
    Text(
        text,
        style = MaterialTheme.typography.titleSmall,
        color = MaterialTheme.colorScheme.primary,
        modifier = Modifier.padding(start = 8.dp, top = 8.dp),
    )
}

/** A device's icon in a round badge: filled when it's connected, tinted otherwise. */
@Composable
internal fun DeviceAvatar(kind: DeviceKind?, strong: Boolean, size: androidx.compose.ui.unit.Dp = 48.dp) {
    val colors = MaterialTheme.colorScheme
    Box(
        Modifier.size(size).background(if (strong) colors.primary else colors.secondaryContainer, CircleShape),
        contentAlignment = Alignment.Center,
    ) {
        Icon(
            painterResource(iconFor(kind)),
            contentDescription = null,
            tint = if (strong) colors.onPrimary else colors.onSecondaryContainer,
            modifier = Modifier.size(size * 0.5f),
        )
    }
}

@Composable
internal fun TransferRow(t: TransferData, onAccept: () -> Unit, onCancel: () -> Unit) {
    val context = LocalContext.current
    fun size(bytes: ULong) = Formatter.formatShortFileSize(context, bytes.toLong())
    val waitingForMe = t.incoming && t.status == TransferStatus.Waiting
    val detail = when {
        waitingForMe -> stringResource(R.string.share_wants_to_send, size(t.size))
        t.status == TransferStatus.Waiting -> stringResource(R.string.share_waiting_short)
        else -> stringResource(
            if (t.incoming) R.string.share_progress_in else R.string.share_progress_out,
            size(t.bytes),
            size(t.size),
        )
    }
    Column(Modifier.fillMaxWidth().padding(top = 12.dp)) {
        Text(t.name, style = MaterialTheme.typography.bodyLarge, maxLines = 1, overflow = TextOverflow.MiddleEllipsis)
        Text(detail, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
        if (t.status == TransferStatus.Running) {
            val fraction = if (t.size == 0uL) 0f else (t.bytes.toDouble() / t.size.toDouble()).toFloat()
            LinearProgressIndicator(progress = { fraction }, modifier = Modifier.fillMaxWidth().padding(top = 6.dp))
        }
        Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.spacedBy(8.dp, Alignment.End)) {
            if (waitingForMe) {
                TextButton(onClick = onCancel) { Text(stringResource(R.string.share_decline)) }
                Button(onClick = onAccept) { Text(stringResource(R.string.share_accept)) }
            } else {
                TextButton(onClick = onCancel) { Text(stringResource(R.string.action_cancel)) }
            }
        }
    }
}

@Composable
internal fun status(device: Device): String {
    val link = device.link ?: return stringResource(R.string.status_offline)
    val linkName = stringResource(
        when (link) {
            Link.LAN -> R.string.link_lan
            Link.BLUETOOTH -> R.string.link_bluetooth
            Link.RELAY -> R.string.link_relay
        },
    )
    val rtt = device.rttMs
    return if (rtt != null) {
        stringResource(R.string.status_connected_rtt, linkName, rtt.toInt())
    } else {
        stringResource(R.string.status_connected, linkName)
    }
}

@DrawableRes
private fun iconFor(kind: DeviceKind?): Int = when (kind) {
    DeviceKind.PHONE, DeviceKind.TABLET -> R.drawable.ic_phone
    DeviceKind.DESKTOP, DeviceKind.LAPTOP, null -> R.drawable.ic_computer
}

@Composable
internal fun DeviceIcon(kind: DeviceKind?, modifier: Modifier = Modifier) {
    Icon(
        painterResource(iconFor(kind)),
        contentDescription = null,
        tint = MaterialTheme.colorScheme.onSurfaceVariant,
        modifier = modifier,
    )
}

@Composable
private fun Searching() {
    Column(
        Modifier.fillMaxWidth().padding(top = 48.dp),
        horizontalAlignment = Alignment.CenterHorizontally,
    ) {
        Text(stringResource(R.string.searching), style = MaterialTheme.typography.titleMedium)
        Spacer(Modifier.height(8.dp))
        Text(
            stringResource(R.string.searching_hint),
            style = MaterialTheme.typography.bodyMedium,
            textAlign = TextAlign.Center,
        )
        LinearProgressIndicator(Modifier.padding(top = 24.dp))
    }
}

@Composable
fun PairingDialog(prompt: PairingPrompt, onAnswer: (Boolean) -> Unit) {
    AlertDialog(
        // Only an explicit answer closes it: the code comparison is the security step.
        onDismissRequest = {},
        title = { Text(stringResource(R.string.pair_title, prompt.name)) },
        text = {
            Column(horizontalAlignment = Alignment.CenterHorizontally, modifier = Modifier.fillMaxWidth()) {
                if (prompt.incoming) {
                    Text(stringResource(R.string.pair_incoming, prompt.name))
                    Spacer(Modifier.height(8.dp))
                }
                Text(stringResource(R.string.pair_check, prompt.name))
                Text(
                    prompt.code,
                    style = MaterialTheme.typography.displaySmall,
                    fontFamily = FontFamily.Monospace,
                    fontWeight = FontWeight.SemiBold,
                    modifier = Modifier.padding(vertical = 16.dp),
                )
            }
        },
        confirmButton = {
            Button(onClick = { onAnswer(true) }) { Text(stringResource(R.string.pair_confirm)) }
        },
        dismissButton = {
            TextButton(onClick = { onAnswer(false) }) { Text(stringResource(R.string.action_cancel)) }
        },
    )
}

@Preview(showBackground = true)
@Composable
private fun HomeScreenPreview() {
    val devices = listOf(
        Device("a", "three-desktop", DeviceKind.DESKTOP, paired = true, paused = false, link = Link.LAN, rttMs = 3u, battery = BatteryData(87u, true)),
        Device("b", "Old laptop", DeviceKind.LAPTOP, paired = true, paused = false, link = null, rttMs = null, battery = null),
        Device("c", "Living room PC", null, paired = false, paused = false, link = null, rttMs = null, battery = null),
    )
    PairlyTheme {
        HomeScreen(
            state = UiState(self = SelfInfo("moto g85", "x"), devices = devices),
            snackbar = remember { SnackbarHostState() },
            onPair = {},
            onScan = {},
        )
    }
}

/** Which phone features have their permissions, and how to ask for the rest. */
data class PhoneFeatures(
    val calls: Boolean,
    val texts: Boolean,
    val onAllowCalls: () -> Unit,
    val onAllowTexts: () -> Unit,
    val files: Boolean = true,
    val onAllowFiles: () -> Unit = {},
    /** Pairly's accessibility service: lock / power off from a PC, and copy → PC. */
    val control: Boolean = true,
    val onAllowControl: () -> Unit = {},
)

@Composable
private fun PhoneFeaturesCard(f: PhoneFeatures) {
    ElevatedCard(Modifier.fillMaxWidth()) {
        Column(Modifier.padding(vertical = 8.dp)) {
            if (!f.calls) {
                ListItem(
                    headlineContent = { Text(stringResource(R.string.feature_calls)) },
                    supportingContent = { Text(stringResource(R.string.feature_calls_body)) },
                    trailingContent = { FilledTonalButton(onClick = f.onAllowCalls) { Text(stringResource(R.string.feature_allow)) } },
                )
            }
            if (!f.files) {
                ListItem(
                    headlineContent = { Text(stringResource(R.string.feature_files)) },
                    supportingContent = { Text(stringResource(R.string.feature_files_body)) },
                    trailingContent = { FilledTonalButton(onClick = f.onAllowFiles) { Text(stringResource(R.string.feature_allow)) } },
                )
            }
            if (!f.control) {
                ListItem(
                    headlineContent = { Text(stringResource(R.string.feature_control)) },
                    supportingContent = { Text(stringResource(R.string.feature_control_body)) },
                    trailingContent = { FilledTonalButton(onClick = f.onAllowControl) { Text(stringResource(R.string.feature_allow)) } },
                )
            }
            if (!f.texts) {
                ListItem(
                    headlineContent = { Text(stringResource(R.string.feature_texts)) },
                    supportingContent = { Text(stringResource(R.string.feature_texts_body)) },
                    trailingContent = { FilledTonalButton(onClick = f.onAllowTexts) { Text(stringResource(R.string.feature_allow)) } },
                )
            }
        }
    }
}

/** Pick one of a PC's commands, confirm, run. */
@Composable
private fun CommandsDialog(device: Device, commands: List<CommandData>, onDismiss: () -> Unit) {
    var confirm by remember { mutableStateOf<CommandData?>(null) }
    val pending = confirm
    if (pending != null) {
        AlertDialog(
            onDismissRequest = { confirm = null },
            title = { Text(stringResource(R.string.command_confirm_title, pending.name)) },
            text = { Text(stringResource(R.string.command_confirm_body, device.name)) },
            confirmButton = {
                TextButton(onClick = {
                    Pairly.runCommand(device, pending)
                    confirm = null
                    onDismiss()
                }) { Text(stringResource(R.string.command_run)) }
            },
            dismissButton = { TextButton(onClick = { confirm = null }) { Text(stringResource(R.string.action_cancel)) } },
        )
        return
    }
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(stringResource(R.string.commands_title, device.name)) },
        text = {
            Column {
                commands.forEach { c ->
                    ListItem(
                        headlineContent = { Text(c.name) },
                        modifier = Modifier.clickable { confirm = c },
                    )
                }
            }
        },
        confirmButton = {},
        dismissButton = { TextButton(onClick = onDismiss) { Text(stringResource(R.string.action_cancel)) } },
    )
}
