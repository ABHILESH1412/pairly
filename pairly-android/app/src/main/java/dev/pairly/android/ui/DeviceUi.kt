package dev.pairly.android.ui

import androidx.activity.compose.BackHandler
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.gestures.awaitEachGesture
import androidx.compose.foundation.gestures.awaitFirstDown
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.interaction.collectIsPressedAsState
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ColumnScope
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.automirrored.outlined.KeyboardArrowRight
import androidx.compose.material.icons.automirrored.outlined.ScreenShare
import androidx.compose.material.icons.outlined.Adjust
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
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.CornerRadius
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.hapticfeedback.HapticFeedbackType
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalHapticFeedback
import androidx.compose.ui.res.pluralStringResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import dev.pairly.android.R
import dev.pairly.android.device.LaserPointer
import dev.pairly.android.screen.PcScreenActivity
import dev.pairly.android.ui.theme.Hue
import dev.pairly.core.ffi.BatteryData
import dev.pairly.core.ffi.Device
import dev.pairly.core.ffi.DeviceKind
import dev.pairly.core.ffi.PowerActionData
import dev.pairly.core.ffi.TransferData

private fun isPc(kind: DeviceKind?) = kind != DeviceKind.PHONE && kind != DeviceKind.TABLET

/** A small soft tag: "● Connected" in green, "● Offline" in red, other facts in [hue]. */
@Composable
private fun Tag(text: String, hue: Hue) {
    val c = hue.colors()
    Text(
        text,
        color = c.content,
        style = MaterialTheme.typography.labelMedium,
        fontWeight = FontWeight.Bold,
        maxLines = 1,
        overflow = TextOverflow.Ellipsis,
        modifier = Modifier.background(c.background, CircleShape).padding(horizontal = 10.dp, vertical = 4.dp),
    )
}

/** The tags under a device's name: whether it's connected, how, and battery news. */
@Composable
fun StatusPill(device: Device, modifier: Modifier = Modifier, short: Boolean = false) {
    FlowRow(
        modifier,
        horizontalArrangement = Arrangement.spacedBy(6.dp, Alignment.CenterHorizontally),
        verticalArrangement = Arrangement.spacedBy(6.dp),
    ) {
        when {
            !device.paired -> Tag(stringResource(R.string.available_to_pair), Hue.PURPLE)
            device.link == null -> Tag("● " + stringResource(R.string.status_offline), Hue.RED)
            else -> {
                Tag("● " + stringResource(R.string.status_connected_short), Hue.GREEN)
                if (!short) Tag(status(device), Hue.PURPLE)
            }
        }
        val b = device.battery
        if (!short && b != null) {
            if (b.charging) {
                Tag(stringResource(R.string.tag_charging), Hue.GREEN)
            } else if (b.percent.toInt() <= 15) {
                Tag(stringResource(R.string.tag_battery_low), Hue.RED)
            }
        }
    }
}

@Composable
private fun battery(device: Device): String? = device.battery?.let { b ->
    stringResource(if (b.charging) R.string.battery_charging else R.string.battery_level, b.percent.toInt())
}

/**
 * A drawn laptop (or phone) with its battery on the screen: a level bar along the bottom (green
 * while charging, red when low) and the percentage in the middle.
 */
@Composable
fun DeviceArt(kind: DeviceKind?, battery: BatteryData?, modifier: Modifier = Modifier, scale: Float = 1f) {
    val laptop = isPc(kind)
    val ink = (if (laptop) Hue.BLUE else Hue.PURPLE).colors().content
    val green = Hue.GREEN.colors().content
    val red = Hue.RED.colors().content
    val level = battery?.percent?.toInt()?.coerceIn(0, 100)
    val charging = battery?.charging == true
    val (w, h) = if (laptop) 96.dp to 64.dp else 52.dp to 88.dp
    Box(modifier.size(w * scale, h * scale), contentAlignment = Alignment.Center) {
        Canvas(Modifier.fillMaxSize()) {
            val u = 1.dp.toPx() * scale
            val stroke = Stroke(2.5f * u)
            val screenTopLeft: Offset
            val screen: Size
            if (laptop) {
                screenTopLeft = Offset(8 * u, 2 * u)
                screen = Size(size.width - 16 * u, size.height - 12 * u)
                drawRoundRect(ink.copy(alpha = 0.1f), screenTopLeft, screen, CornerRadius(6 * u))
                drawRoundRect(ink, screenTopLeft, screen, CornerRadius(6 * u), style = stroke)
                drawRoundRect(ink, Offset(u, size.height - 9 * u), Size(size.width - 2 * u, 7 * u), CornerRadius(3.5f * u))
            } else {
                screenTopLeft = Offset(2 * u, 2 * u)
                screen = Size(size.width - 4 * u, size.height - 4 * u)
                drawRoundRect(ink.copy(alpha = 0.1f), screenTopLeft, screen, CornerRadius(11 * u))
                drawRoundRect(ink, screenTopLeft, screen, CornerRadius(11 * u), style = stroke)
                drawRoundRect(ink.copy(alpha = 0.6f), Offset(size.width / 2 - 7 * u, 7 * u), Size(14 * u, 3 * u), CornerRadius(1.5f * u))
            }
            if (level != null) {
                val barLeft = Offset(screenTopLeft.x + 8 * u, screenTopLeft.y + screen.height - 11 * u)
                val barWidth = screen.width - 16 * u
                drawRoundRect(ink.copy(alpha = 0.2f), barLeft, Size(barWidth, 5 * u), CornerRadius(2.5f * u))
                val fill = when {
                    charging -> green
                    level <= 15 -> red
                    else -> ink
                }
                if (level > 0) {
                    drawRoundRect(fill, barLeft, Size((barWidth * level / 100f).coerceAtLeast(5 * u), 5 * u), CornerRadius(2.5f * u))
                }
            }
        }
        if (level != null) {
            Text(
                "$level%",
                color = ink,
                fontWeight = FontWeight.Bold,
                fontSize = (13 * scale).sp,
                modifier = Modifier.padding(bottom = (if (laptop) 12.dp else 6.dp) * scale),
            )
        }
    }
}

/** A paired device on the home screen; tap for everything you can do with it. */
@Composable
fun DeviceSummaryCard(device: Device, activeTransfers: Int, onClick: () -> Unit) {
    Card(
        onClick = onClick,
        shape = RoundedCornerShape(28.dp),
        colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surfaceContainerLow),
        modifier = Modifier.fillMaxWidth(),
    ) {
        Row(Modifier.padding(16.dp), verticalAlignment = Alignment.CenterVertically) {
            Box(Modifier.size(64.dp), contentAlignment = Alignment.Center) {
                DeviceArt(device.kind, device.battery, scale = if (isPc(device.kind)) 0.62f else 0.7f)
            }
            Spacer(Modifier.size(16.dp))
            Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(6.dp)) {
                Text(
                    device.name,
                    style = MaterialTheme.typography.titleMedium,
                    fontWeight = FontWeight.Bold,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
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
            Icon(Icons.AutoMirrored.Outlined.KeyboardArrowRight, contentDescription = null)
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
    val pc = isPc(device.kind)
    val context = LocalContext.current
    val laser = remember(device.id) { LaserPointer(context, device.id) }
    // Never leave the pointer showing (e.g. leaving the screen while holding it).
    DisposableEffect(laser) { onDispose { laser.stop() } }
    val on = device.link != null
    Scaffold(
        topBar = {
            TopAppBar(
                title = {},
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
                .padding(horizontal = 16.dp, vertical = 8.dp),
            verticalArrangement = Arrangement.spacedBy(16.dp),
        ) {
            DeviceHero(device)
            transfers.forEach { t ->
                TransferRow(t, onAccept = { actions.acceptTransfer(t.id) }, onCancel = { actions.cancelTransfer(t.id) })
            }
            val tiles = buildList {
                if (pc) {
                    add(
                        Tile(Icons.AutoMirrored.Outlined.ScreenShare, stringResource(R.string.action_pc_screen), on, Hue.PURPLE, {
                            PcScreenActivity.open(context, device.id, device.name)
                        }),
                    )
                }
                add(Tile(Icons.Outlined.Mouse, stringResource(R.string.action_remote), on, Hue.TEAL, actions.remote))
                add(Tile(Icons.Outlined.Slideshow, stringResource(R.string.action_presenter), on, Hue.CORAL, actions.presenter))
                if (pc && laser.available) {
                    add(
                        Tile(
                            Icons.Outlined.Adjust,
                            stringResource(R.string.action_pointer),
                            on,
                            Hue.PINK,
                            onClick = {},
                            onHold = { down -> if (down) laser.start() else laser.stop() },
                        ),
                    )
                }
                add(Tile(Icons.Outlined.UploadFile, stringResource(R.string.action_send_files), on, Hue.BLUE, actions.sendFiles))
                add(Tile(Icons.Outlined.ContentPaste, stringResource(R.string.action_send_clipboard), on, Hue.AMBER, actions.clipboard))
                if (ringing) {
                    add(Tile(Icons.Outlined.NotificationsOff, stringResource(R.string.action_stop_ring), on, Hue.RED, { actions.ring(false) }))
                } else {
                    add(Tile(Icons.Outlined.NotificationsActive, stringResource(R.string.action_ring), on, Hue.GREEN, { actions.ring(true) }))
                }
                add(Tile(Icons.Outlined.Notifications, stringResource(R.string.action_ping), on, Hue.GREEN, actions.ping))
                actions.commands?.let { add(Tile(Icons.Outlined.Terminal, stringResource(R.string.action_commands), on, Hue.GRAY, it)) }
                if (pc) {
                    add(Tile(Icons.Outlined.Lock, stringResource(R.string.action_lock_pc), on, Hue.GRAY, { actions.power(PowerActionData.LOCK) }))
                    add(Tile(Icons.Outlined.PowerSettingsNew, stringResource(R.string.action_power_off), on, Hue.RED, { confirmPower = true }))
                }
            }
            TileGrid(tiles)
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

/** The top of a device's screen: the drawn device with its battery, the name, and its tags. */
@Composable
private fun DeviceHero(device: Device) {
    Card(
        shape = RoundedCornerShape(32.dp),
        colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surfaceContainerLowest),
        modifier = Modifier.fillMaxWidth(),
    ) {
        Column(
            Modifier.fillMaxWidth().padding(vertical = 24.dp, horizontal = 16.dp),
            horizontalAlignment = Alignment.CenterHorizontally,
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            DeviceArt(device.kind, device.battery, scale = 1.25f)
            Text(
                device.name,
                style = MaterialTheme.typography.headlineSmall,
                fontWeight = FontWeight.Bold,
                textAlign = TextAlign.Center,
                maxLines = 2,
                overflow = TextOverflow.Ellipsis,
            )
            StatusPill(device)
        }
    }
}

private class Tile(
    val icon: ImageVector,
    val label: String,
    val enabled: Boolean,
    val hue: Hue,
    val onClick: () -> Unit,
    /** A tile you hold instead of tap: true when pressed, false when let go. */
    val onHold: ((Boolean) -> Unit)? = null,
)

/** Pastel tiles, two to a row on a phone, more on wider screens. */
@Composable
private fun TileGrid(tiles: List<Tile>) {
    BoxWithConstraints(Modifier.fillMaxWidth()) {
        val columns = (maxWidth / 170.dp).toInt().coerceIn(2, 5)
        Column(verticalArrangement = Arrangement.spacedBy(12.dp)) {
            tiles.chunked(columns).forEach { row ->
                Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.spacedBy(12.dp)) {
                    row.forEach { ActionTile(it, Modifier.weight(1f)) }
                    repeat(columns - row.size) { Spacer(Modifier.weight(1f)) }
                }
            }
        }
    }
}

/** One action on a soft colour card: an icon and a label. It dips a little and ticks when pressed. */
@Composable
private fun ActionTile(tile: Tile, modifier: Modifier) {
    val hold = tile.onHold
    var held by remember { mutableStateOf(false) }
    val c = (if (held) Hue.RED else tile.hue).colors()
    val haptics = LocalHapticFeedback.current
    val interaction = remember { MutableInteractionSource() }
    val pressed by interaction.collectIsPressedAsState()
    val scale by animateFloatAsState(if (pressed || held) 0.95f else 1f, label = "press")
    val colors = CardDefaults.cardColors(
        containerColor = c.background,
        contentColor = c.content,
        disabledContainerColor = c.background.copy(alpha = 0.45f),
        disabledContentColor = c.content.copy(alpha = 0.5f),
    )
    val shape = RoundedCornerShape(24.dp)
    val sized = modifier.height(100.dp).graphicsLayer {
        scaleX = scale
        scaleY = scale
    }
    val body: @Composable ColumnScope.() -> Unit = {
        Column(Modifier.fillMaxSize().padding(16.dp), verticalArrangement = Arrangement.SpaceBetween) {
            Icon(tile.icon, contentDescription = null, modifier = Modifier.size(26.dp))
            Text(
                tile.label,
                style = MaterialTheme.typography.titleSmall,
                fontWeight = FontWeight.Bold,
                maxLines = 2,
                overflow = TextOverflow.Ellipsis,
            )
        }
    }
    if (hold == null) {
        Card(
            onClick = {
                haptics.performHapticFeedback(HapticFeedbackType.ContextClick)
                tile.onClick()
            },
            enabled = tile.enabled,
            shape = shape,
            colors = colors,
            interactionSource = interaction,
            modifier = sized,
            content = body,
        )
    } else {
        Card(
            shape = shape,
            colors = colors,
            content = body,
            modifier = sized.pointerInput(tile.enabled) {
                if (!tile.enabled) return@pointerInput
                awaitEachGesture {
                    awaitFirstDown()
                    held = true
                    haptics.performHapticFeedback(HapticFeedbackType.LongPress)
                    hold(true)
                    do {
                        val event = awaitPointerEvent()
                    } while (event.changes.any { it.pressed })
                    held = false
                    hold(false)
                }
            },
        )
    }
}
