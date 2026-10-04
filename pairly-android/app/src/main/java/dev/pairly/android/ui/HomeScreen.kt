package dev.pairly.android.ui

import androidx.annotation.DrawableRes
import android.net.Uri
import android.text.format.Formatter
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
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
    val context = LocalContext.current
    var askBeforeReceiving by remember { mutableStateOf(SharePrefs.askBeforeReceiving(context)) }
    var notificationAccess by remember { mutableStateOf(PairlyNotificationListener.isEnabled(context)) }
    var bluetoothAllowed by remember { mutableStateOf(PairlyBluetooth.permitted(context)) }
    val requestBluetooth = rememberLauncherForActivityResult(ActivityResultContracts.RequestPermission()) { granted ->
        bluetoothAllowed = granted
        if (granted) Pairly.bluetoothChanged()
    }
    // Re-check when coming back from the system settings screen.
    LifecycleEventEffect(Lifecycle.Event.ON_RESUME) {
        notificationAccess = PairlyNotificationListener.isEnabled(context)
        bluetoothAllowed = PairlyBluetooth.permitted(context)
    }
    LaunchedEffect(Unit) {
        Pairly.messages.collect { snackbar.showSnackbar(it) }
    }
    if (choosingApps) {
        AppsScreen(onBack = { choosingApps = false })
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
        onPing = Pairly::ping,
        onUnpair = Pairly::unpair,
        onScan = { scanning = true },
        onRing = Pairly::ring,
        onClipboard = { device ->
            // In the foreground, so Android lets us read the clipboard.
            val clip = context.getSystemService(ClipboardManager::class.java)?.primaryClip
            val text = clip?.takeIf { it.itemCount > 0 }?.getItemAt(0)?.coerceToText(context)?.toString()
            Pairly.sendClipboard(device, text)
        },
        notificationAccess = notificationAccess,
        onChooseApps = { choosingApps = true },
        transfers = transfers.values.toList(),
        onSendFiles = { device ->
            sendTo = device
            pickFiles.launch(arrayOf("*/*"))
        },
        onAcceptTransfer = Pairly::acceptTransfer,
        onCancelTransfer = Pairly::cancelTransfer,
        askBeforeReceiving = askBeforeReceiving,
        bluetoothAllowed = bluetoothAllowed,
        onAllowBluetooth = {
            if (android.os.Build.VERSION.SDK_INT >= android.os.Build.VERSION_CODES.S) {
                requestBluetooth.launch(android.Manifest.permission.BLUETOOTH_CONNECT)
            }
        },
        onAskBeforeReceiving = {
            askBeforeReceiving = it
            SharePrefs.setAskBeforeReceiving(context, it)
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
    onPing: (Device) -> Unit,
    onUnpair: (Device) -> Unit,
    onScan: () -> Unit,
    onRing: (Device, Boolean) -> Unit = { _, _ -> },
    onClipboard: (Device) -> Unit = {},
    notificationAccess: Boolean = true,
    onChooseApps: () -> Unit = {},
    transfers: List<TransferData> = emptyList(),
    onSendFiles: (Device) -> Unit = {},
    onAcceptTransfer: (ULong) -> Unit = {},
    onCancelTransfer: (ULong) -> Unit = {},
    askBeforeReceiving: Boolean = false,
    onAskBeforeReceiving: (Boolean) -> Unit = {},
    bluetoothAllowed: Boolean = true,
    onAllowBluetooth: () -> Unit = {},
) {
    val paired = state.devices.filter { it.paired }
    val available = state.devices.filterNot { it.paired }
    var confirmUnpair by remember { mutableStateOf<Device?>(null) }
    val ringing = remember { mutableStateListOf<String>() }

    Scaffold(
        topBar = {
            TopAppBar(
                title = {
                    Column {
                        Text(stringResource(R.string.app_name))
                        state.self?.let {
                            Text(it.name, style = MaterialTheme.typography.bodySmall)
                        }
                    }
                },
            )
        },
        snackbarHost = { SnackbarHost(snackbar) },
        floatingActionButton = {
            if (state.self != null) {
                ExtendedFloatingActionButton(onClick = onScan) { Text(stringResource(R.string.action_scan)) }
            }
        },
    ) { padding ->
        LazyColumn(
            contentPadding = PaddingValues(
                start = 16.dp,
                end = 16.dp,
                top = padding.calculateTopPadding() + 8.dp,
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
                item { SectionHeader(stringResource(R.string.section_paired)) }
                items(paired, key = { it.id }) { device ->
                    PairedDeviceCard(
                        device,
                        ringing = device.id in ringing,
                        onPing = { onPing(device) },
                        onRing = { on ->
                            if (on) ringing.add(device.id) else ringing.remove(device.id)
                            onRing(device, on)
                        },
                        onClipboard = { onClipboard(device) },
                        onUnpair = { confirmUnpair = device },
                        transfers = transfers.filter { it.deviceId == device.id },
                        onSendFiles = { onSendFiles(device) },
                        onAcceptTransfer = onAcceptTransfer,
                        onCancelTransfer = onCancelTransfer,
                    )
                }
                item {
                    ListItem(
                        headlineContent = { Text(stringResource(R.string.share_ask_setting)) },
                        supportingContent = { Text(stringResource(R.string.share_ask_setting_body)) },
                        trailingContent = { Switch(askBeforeReceiving, onCheckedChange = onAskBeforeReceiving) },
                    )
                }
            }
            if (available.isNotEmpty()) {
                item { SectionHeader(stringResource(R.string.section_available)) }
                items(available, key = { it.id }) { device ->
                    ListItem(
                        headlineContent = { Text(device.name) },
                        supportingContent = { Text(stringResource(R.string.available_to_pair)) },
                        leadingContent = { DeviceIcon(device.kind) },
                        trailingContent = {
                            Button(onClick = { onPair(device.id) }) { Text(stringResource(R.string.action_pair)) }
                        },
                    )
                }
            }
            if (state.self != null && state.devices.isEmpty()) {
                item { Searching() }
            }
        }
    }

    confirmUnpair?.let { device ->
        AlertDialog(
            onDismissRequest = { confirmUnpair = null },
            title = { Text(stringResource(R.string.unpair_title, device.name)) },
            text = { Text(stringResource(R.string.unpair_body)) },
            confirmButton = {
                TextButton(onClick = {
                    confirmUnpair = null
                    onUnpair(device)
                }) { Text(stringResource(R.string.action_unpair)) }
            },
            dismissButton = {
                TextButton(onClick = { confirmUnpair = null }) { Text(stringResource(R.string.action_cancel)) }
            },
        )
    }
}

@Composable
private fun SectionHeader(text: String) {
    Text(
        text,
        style = MaterialTheme.typography.titleSmall,
        color = MaterialTheme.colorScheme.primary,
        modifier = Modifier.padding(top = 8.dp),
    )
}

@Composable
private fun PairedDeviceCard(
    device: Device,
    ringing: Boolean,
    onPing: () -> Unit,
    onRing: (Boolean) -> Unit,
    onClipboard: () -> Unit,
    onUnpair: () -> Unit,
    transfers: List<TransferData> = emptyList(),
    onSendFiles: () -> Unit = {},
    onAcceptTransfer: (ULong) -> Unit = {},
    onCancelTransfer: (ULong) -> Unit = {},
) {
    val connected = device.link != null
    var menu by remember { mutableStateOf(false) }
    ElevatedCard(Modifier.fillMaxWidth()) {
        Column(Modifier.padding(16.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                DeviceIcon(device.kind, Modifier.size(40.dp))
                Spacer(Modifier.size(16.dp))
                Column(Modifier.weight(1f)) {
                    Text(device.name, style = MaterialTheme.typography.titleMedium)
                    Text(
                        status(device),
                        style = MaterialTheme.typography.bodyMedium,
                        color = if (connected) MaterialTheme.colorScheme.primary else MaterialTheme.colorScheme.outline,
                    )
                    device.battery?.let { b ->
                        Text(
                            stringResource(
                                if (b.charging) R.string.battery_charging else R.string.battery_level,
                                b.percent.toInt(),
                            ),
                            style = MaterialTheme.typography.bodySmall,
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                        )
                    }
                }
            }
            transfers.forEach { t ->
                TransferRow(t, onAccept = { onAcceptTransfer(t.id) }, onCancel = { onCancelTransfer(t.id) })
            }
            FlowRow(
                Modifier.fillMaxWidth().padding(top = 12.dp),
                horizontalArrangement = Arrangement.spacedBy(8.dp, Alignment.End),
                itemVerticalAlignment = Alignment.CenterVertically,
            ) {
                Box {
                    TextButton(onClick = { menu = true }) { Text(stringResource(R.string.action_more)) }
                    DropdownMenu(expanded = menu, onDismissRequest = { menu = false }) {
                        DropdownMenuItem(
                            text = { Text(stringResource(R.string.action_ping)) },
                            enabled = connected,
                            onClick = {
                                menu = false
                                onPing()
                            },
                        )
                        DropdownMenuItem(
                            text = { Text(stringResource(R.string.action_unpair)) },
                            onClick = {
                                menu = false
                                onUnpair()
                            },
                        )
                    }
                }
                OutlinedButton(onClick = { onRing(!ringing) }, enabled = connected) {
                    Text(stringResource(if (ringing) R.string.action_stop_ring else R.string.action_ring))
                }
                FilledTonalButton(onClick = onSendFiles, enabled = connected) {
                    Text(stringResource(R.string.action_send_files))
                }
                FilledTonalButton(onClick = onClipboard, enabled = connected) {
                    Text(stringResource(R.string.action_send_clipboard))
                }
            }
        }
    }
}

@Composable
private fun TransferRow(t: TransferData, onAccept: () -> Unit, onCancel: () -> Unit) {
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
private fun status(device: Device): String {
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
private fun DeviceIcon(kind: DeviceKind?, modifier: Modifier = Modifier) {
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
        Device("a", "three-desktop", DeviceKind.DESKTOP, paired = true, link = Link.LAN, rttMs = 3u, battery = BatteryData(87u, true)),
        Device("b", "Old laptop", DeviceKind.LAPTOP, paired = true, link = null, rttMs = null, battery = null),
        Device("c", "Living room PC", null, paired = false, link = null, rttMs = null, battery = null),
    )
    PairlyTheme {
        HomeScreen(
            state = UiState(self = SelfInfo("moto g85", "x"), devices = devices),
            snackbar = remember { SnackbarHostState() },
            onPair = {},
            onPing = {},
            onUnpair = {},
            onScan = {},
        )
    }
}
