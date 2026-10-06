package dev.pairly.android.ui

import androidx.activity.compose.BackHandler
import androidx.compose.foundation.background
import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.outlined.ContentPaste
import androidx.compose.material.icons.outlined.LinkOff
import androidx.compose.material.icons.outlined.Lock
import androidx.compose.material.icons.outlined.Mouse
import androidx.compose.material.icons.outlined.Notifications
import androidx.compose.material.icons.outlined.NotificationsActive
import androidx.compose.material.icons.outlined.NotificationsOff
import androidx.compose.material.icons.outlined.PowerSettingsNew
import androidx.compose.material.icons.outlined.Slideshow
import androidx.compose.material.icons.outlined.Terminal
import androidx.compose.material.icons.outlined.UploadFile
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.ElevatedCard
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import dev.pairly.android.device.LaserPointer
import androidx.compose.material.icons.outlined.Adjust
import androidx.compose.runtime.DisposableEffect
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.foundation.gestures.awaitFirstDown
import androidx.compose.foundation.gestures.awaitEachGesture
import androidx.compose.foundation.layout.ColumnScope
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.res.pluralStringResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import dev.pairly.android.R
import dev.pairly.core.ffi.Device
import dev.pairly.core.ffi.DeviceKind
import dev.pairly.core.ffi.PowerActionData
import dev.pairly.core.ffi.TransferData

/** Green when connected, red when offline, the accent colour when it can be paired. */
@Composable
fun StatusPill(device: Device, modifier: Modifier = Modifier, short: Boolean = false) {
    val dark = isSystemInDarkTheme()
    val (color, text) = when {
        !device.paired -> MaterialTheme.colorScheme.primary to stringResource(R.string.available_to_pair)
        device.link == null -> MaterialTheme.colorScheme.error to stringResource(R.string.status_offline)
        else -> (if (dark) Color(0xFF6DD58C) else Color(0xFF1B873F)) to
            if (short) stringResource(R.string.status_connected_short) else status(device)
    }
    Row(
        modifier
            .background(color.copy(alpha = 0.15f), CircleShape)
            .padding(horizontal = 12.dp, vertical = 4.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Box(Modifier.size(8.dp).background(color, CircleShape))
        Spacer(Modifier.size(8.dp))
        Text(
            text,
            color = color,
            style = MaterialTheme.typography.labelMedium,
            fontWeight = FontWeight.Bold,
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
        )
    }
}

@Composable
private fun battery(device: Device): String? = device.battery?.let { b ->
    stringResource(if (b.charging) R.string.battery_charging else R.string.battery_level, b.percent.toInt())
}

/** A paired device on the home screen; tap for everything you can do with it. */
@Composable
fun DeviceSummaryCard(device: Device, activeTransfers: Int, onClick: () -> Unit) {
    ElevatedCard(onClick = onClick, modifier = Modifier.fillMaxWidth()) {
        Row(Modifier.padding(16.dp), verticalAlignment = Alignment.CenterVertically) {
            DeviceIcon(device.kind, Modifier.size(40.dp))
            Spacer(Modifier.size(16.dp))
            Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(6.dp)) {
                Text(device.name, style = MaterialTheme.typography.titleMedium)
                StatusPill(device, short = true)
                val extra = listOfNotNull(
                    battery(device),
                    activeTransfers.takeIf { it > 0 }?.let { pluralStringResource(R.plurals.transfers_active, it, it) },
                )
                if (extra.isNotEmpty()) {
                    Text(
                        extra.joinToString(" · "),
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
            }
        }
    }
}

/** What a device screen can do. */
class DeviceActions(
    val sendFiles: () -> Unit,
    val clipboard: () -> Unit,
    val ring: (Boolean) -> Unit,
    val ping: () -> Unit,
    val remote: () -> Unit,
    val presenter: () -> Unit,
    /** Null when the device offers no commands. */
    val commands: (() -> Unit)?,
    /** Lock, power off or restart a PC. */
    val power: (PowerActionData) -> Unit,
    val unpair: () -> Unit,
    val acceptTransfer: (ULong) -> Unit,
    val cancelTransfer: (ULong) -> Unit,
)

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun DeviceScreen(
    device: Device,
    ringing: Boolean,
    transfers: List<TransferData>,
    snackbar: SnackbarHostState,
    actions: DeviceActions,
    onBack: () -> Unit,
) {
    BackHandler(onBack = onBack)
    var confirmUnpair by remember { mutableStateOf(false) }
    var confirmPower by remember { mutableStateOf(false) }
    val isPc = device.kind == DeviceKind.DESKTOP || device.kind == DeviceKind.LAPTOP
    val context = LocalContext.current
    val laser = remember(device.id) { LaserPointer(context, device.id) }
    // Never leave the pointer showing (e.g. leaving the screen while holding it).
    DisposableEffect(laser) { onDispose { laser.stop() } }
    val on = device.link != null
    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text(device.name, maxLines = 1, overflow = TextOverflow.Ellipsis) },
                navigationIcon = {
                    IconButton(onClick = onBack) {
                        Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = stringResource(R.string.action_back))
                    }
                },
            )
        },
        snackbarHost = { SnackbarHost(snackbar) },
    ) { padding ->
        Column(
            Modifier
                .padding(padding)
                .verticalScroll(rememberScrollState())
                .padding(16.dp),
            verticalArrangement = Arrangement.spacedBy(16.dp),
        ) {
            Column(Modifier.fillMaxWidth(), horizontalAlignment = Alignment.CenterHorizontally) {
                DeviceIcon(device.kind, Modifier.size(64.dp))
                Spacer(Modifier.height(8.dp))
                Text(device.name, style = MaterialTheme.typography.headlineSmall)
                Spacer(Modifier.height(8.dp))
                StatusPill(device)
                battery(device)?.let {
                    Spacer(Modifier.height(6.dp))
                    Text(it, style = MaterialTheme.typography.bodyMedium, color = MaterialTheme.colorScheme.onSurfaceVariant)
                }
            }
            transfers.forEach { t ->
                TransferRow(t, onAccept = { actions.acceptTransfer(t.id) }, onCancel = { actions.cancelTransfer(t.id) })
            }
            val tiles = buildList {
                add(Tile(Icons.Outlined.UploadFile, stringResource(R.string.action_send_files), on, actions.sendFiles))
                add(Tile(Icons.Outlined.ContentPaste, stringResource(R.string.action_send_clipboard), on, actions.clipboard))
                if (ringing) {
                    add(Tile(Icons.Outlined.NotificationsOff, stringResource(R.string.action_stop_ring), on, highlight = true) { actions.ring(false) })
                } else {
                    add(Tile(Icons.Outlined.NotificationsActive, stringResource(R.string.action_ring), on) { actions.ring(true) })
                }
                add(Tile(Icons.Outlined.Notifications, stringResource(R.string.action_ping), on, actions.ping))
                add(Tile(Icons.Outlined.Mouse, stringResource(R.string.action_remote), on, actions.remote))
                add(Tile(Icons.Outlined.Slideshow, stringResource(R.string.action_presenter), on, actions.presenter))
                actions.commands?.let { add(Tile(Icons.Outlined.Terminal, stringResource(R.string.action_commands), on, it)) }
                if (isPc && laser.available) {
                    add(
                        Tile(
                            Icons.Outlined.Adjust,
                            stringResource(R.string.action_pointer),
                            on,
                            onClick = {},
                            onHold = { down -> if (down) laser.start() else laser.stop() },
                        ),
                    )
                }
                if (isPc) {
                    add(Tile(Icons.Outlined.Lock, stringResource(R.string.action_lock_pc), on) { actions.power(PowerActionData.LOCK) })
                    add(Tile(Icons.Outlined.PowerSettingsNew, stringResource(R.string.action_power_off), on) { confirmPower = true })
                }
            }
            tiles.chunked(3).forEach { row ->
                Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.spacedBy(12.dp)) {
                    row.forEach { ActionTile(it, Modifier.weight(1f)) }
                    repeat(3 - row.size) { Spacer(Modifier.weight(1f)) }
                }
            }
            TextButton(
                onClick = { confirmUnpair = true },
                modifier = Modifier.align(Alignment.CenterHorizontally),
            ) {
                Icon(Icons.Outlined.LinkOff, contentDescription = null, tint = MaterialTheme.colorScheme.error)
                Spacer(Modifier.size(8.dp))
                Text(stringResource(R.string.action_unpair), color = MaterialTheme.colorScheme.error)
            }
        }
    }
    if (confirmPower) {
        AlertDialog(
            onDismissRequest = { confirmPower = false },
            title = { Text(stringResource(R.string.power_title, device.name)) },
            text = { Text(stringResource(R.string.power_body)) },
            confirmButton = {
                TextButton(onClick = {
                    confirmPower = false
                    actions.power(PowerActionData.POWER_OFF)
                }) { Text(stringResource(R.string.action_power_off), color = MaterialTheme.colorScheme.error) }
            },
            dismissButton = {
                Row {
                    TextButton(onClick = { confirmPower = false }) { Text(stringResource(R.string.action_cancel)) }
                    TextButton(onClick = {
                        confirmPower = false
                        actions.power(PowerActionData.RESTART)
                    }) { Text(stringResource(R.string.action_restart)) }
                }
            },
        )
    }
    if (confirmUnpair) {
        AlertDialog(
            onDismissRequest = { confirmUnpair = false },
            title = { Text(stringResource(R.string.unpair_title, device.name)) },
            text = { Text(stringResource(R.string.unpair_body)) },
            confirmButton = {
                TextButton(onClick = {
                    confirmUnpair = false
                    actions.unpair()
                }) { Text(stringResource(R.string.action_unpair)) }
            },
            dismissButton = {
                TextButton(onClick = { confirmUnpair = false }) { Text(stringResource(R.string.action_cancel)) }
            },
        )
    }
}

private class Tile(
    val icon: ImageVector,
    val label: String,
    val enabled: Boolean,
    val onClick: () -> Unit,
    val highlight: Boolean = false,
    /** A tile you hold instead of tap: true when pressed, false when let go. */
    val onHold: ((Boolean) -> Unit)? = null,
) {
    constructor(icon: ImageVector, label: String, enabled: Boolean, highlight: Boolean = false, onClick: () -> Unit) :
        this(icon, label, enabled, onClick, highlight)
}

@Composable
private fun ActionTile(tile: Tile, modifier: Modifier) {
    val colors = if (tile.highlight) {
        CardDefaults.cardColors(
            containerColor = MaterialTheme.colorScheme.errorContainer,
            contentColor = MaterialTheme.colorScheme.onErrorContainer,
        )
    } else {
        CardDefaults.cardColors(
            containerColor = MaterialTheme.colorScheme.secondaryContainer,
            contentColor = MaterialTheme.colorScheme.onSecondaryContainer,
        )
    }
    val hold = tile.onHold
    var held by remember { mutableStateOf(false) }
    val content: @Composable ColumnScope.() -> Unit = {
        Column(
            Modifier.fillMaxWidth().padding(8.dp).weight(1f),
            horizontalAlignment = Alignment.CenterHorizontally,
            verticalArrangement = Arrangement.Center,
        ) {
            Icon(tile.icon, contentDescription = null, modifier = Modifier.size(28.dp))
            Spacer(Modifier.height(8.dp))
            Text(
                tile.label,
                style = MaterialTheme.typography.labelLarge,
                textAlign = TextAlign.Center,
                maxLines = 2,
                overflow = TextOverflow.Ellipsis,
            )
        }
    }
    if (hold == null) {
        Card(onClick = tile.onClick, enabled = tile.enabled, colors = colors, modifier = modifier.height(104.dp), content = content)
    } else {
        val pressedColors = CardDefaults.cardColors(
            containerColor = MaterialTheme.colorScheme.errorContainer,
            contentColor = MaterialTheme.colorScheme.onErrorContainer,
        )
        Card(
            colors = if (held) pressedColors else colors,
            content = content,
            modifier = modifier.height(104.dp).pointerInput(tile.enabled) {
                if (!tile.enabled) return@pointerInput
                awaitEachGesture {
                    awaitFirstDown()
                    held = true
                    hold(true)
                    while (awaitPointerEvent().changes.any { it.pressed }) Unit
                    held = false
                    hold(false)
                }
            },
        )
    }
}
