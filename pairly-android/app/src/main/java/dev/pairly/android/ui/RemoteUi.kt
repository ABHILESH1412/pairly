package dev.pairly.android.ui

import androidx.activity.compose.BackHandler
import androidx.compose.foundation.background
import androidx.compose.foundation.focusable
import androidx.compose.foundation.gestures.awaitEachGesture
import androidx.compose.foundation.gestures.awaitFirstDown
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.Button
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.FilledTonalButton
import androidx.compose.material3.FilterChip
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.input.key.Key
import androidx.compose.ui.input.key.KeyEventType
import androidx.compose.ui.input.key.key
import androidx.compose.ui.input.key.onPreviewKeyEvent
import androidx.compose.ui.input.key.type
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.input.pointer.positionChange
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalView
import androidx.compose.ui.platform.LocalSoftwareKeyboardController
import androidx.compose.ui.platform.LocalViewConfiguration
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.TextRange
import androidx.compose.ui.text.input.TextFieldValue
import androidx.compose.ui.unit.dp
import dev.pairly.android.Pairly
import dev.pairly.android.R
import dev.pairly.android.device.LaserPointer
import dev.pairly.core.ffi.ButtonActionData
import dev.pairly.core.ffi.Device
import dev.pairly.core.ffi.KeyData
import dev.pairly.core.ffi.ModifiersData
import dev.pairly.core.ffi.MouseButtonData
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlin.math.abs

private const val SENSITIVITY = 1.4f
private const val SCROLL_SCALE = 0.6f
private const val TAP_MS = 250L
private const val FRAME_MS = 16L
/** After a tap, how long a new touch turns into a drag (as laptop touchpads do). */
private const val DRAG_WINDOW_MS = 200L
/** Held arrow keys repeat like a keyboard's: after this, then every REPEAT_MS. */
private const val REPEAT_DELAY_MS = 400L
private const val REPEAT_MS = 60L

/** Accumulates finger movement and sends it once per frame. */
private class Mover(private val device: String) {
    private var dx = 0f
    private var dy = 0f
    private var sx = 0f
    private var sy = 0f

    @Synchronized
    fun move(d: Offset) {
        // Mild acceleration: fast flicks travel further.
        val speed = 1f + minOf(d.getDistance() / 25f, 2f)
        dx += d.x * SENSITIVITY * speed
        dy += d.y * SENSITIVITY * speed
    }

    @Synchronized
    fun scroll(d: Offset) {
        // Natural scrolling, like a laptop touchpad.
        sx -= d.x * SCROLL_SCALE
        sy -= d.y * SCROLL_SCALE
    }

    fun flush() {
        val (x, y, a, b) = synchronized(this) {
            val v = arrayOf(dx, dy, sx, sy)
            dx = 0f; dy = 0f; sx = 0f; sy = 0f
            v
        }
        if (x != 0f || y != 0f || a != 0f || b != 0f) Pairly.inputPointer(device, x, y, a, b)
    }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun RemoteInputScreen(device: Device, onBack: () -> Unit) {
    BackHandler(onBack = onBack)
    val mover = remember(device.id) { Mover(device.id) }
    LaunchedEffect(mover) {
        while (true) {
            delay(FRAME_MS)
            mover.flush()
        }
    }
    var mods by remember { mutableStateOf(ModifiersData(ctrl = false, alt = false, shift = false, logo = false)) }
    fun key(k: KeyData) {
        Pairly.inputKey(device.id, null, k, mods)
        mods = ModifiersData(ctrl = false, alt = false, shift = false, logo = false)
    }
    fun text(t: String) {
        Pairly.inputKey(device.id, t, null, mods)
        mods = ModifiersData(ctrl = false, alt = false, shift = false, logo = false)
    }
    val focus = remember { FocusRequester() }
    val keyboard = LocalSoftwareKeyboardController.current
    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text(stringResource(R.string.remote_title, device.name)) },
                actions = {
                    TextButton(onClick = {
                        focus.requestFocus()
                        keyboard?.show()
                    }) { Text(stringResource(R.string.remote_keyboard)) }
                    TextButton(onClick = onBack) { Text(stringResource(R.string.action_done)) }
                },
            )
        },
    ) { padding ->
        Column(Modifier.padding(padding).fillMaxSize().imePadding().padding(12.dp)) {
            Touchpad(mover, device.id, Modifier.weight(1f).fillMaxWidth())
            Row(Modifier.fillMaxWidth().padding(top = 8.dp), horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                HoldButton(stringResource(R.string.remote_left), device.id, MouseButtonData.LEFT, Modifier.weight(1f))
                HoldButton(stringResource(R.string.remote_right), device.id, MouseButtonData.RIGHT, Modifier.weight(1f))
            }
            Row(
                Modifier.fillMaxWidth().padding(top = 8.dp).horizontalScroll(rememberScrollState()),
                horizontalArrangement = Arrangement.spacedBy(6.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                FilterChip(mods.ctrl, { mods = mods.copy(ctrl = !mods.ctrl) }, { Text("Ctrl") })
                FilterChip(mods.alt, { mods = mods.copy(alt = !mods.alt) }, { Text("Alt") })
                FilterChip(mods.shift, { mods = mods.copy(shift = !mods.shift) }, { Text("Shift") })
                FilterChip(mods.logo, { mods = mods.copy(logo = !mods.logo) }, { Text("Super") })
                OutlinedButton(onClick = { key(KeyData.Escape) }) { Text("Esc") }
                OutlinedButton(onClick = { key(KeyData.Tab) }) { Text("Tab") }
                OutlinedButton(onClick = { key(KeyData.Home) }) { Text("Home") }
                OutlinedButton(onClick = { key(KeyData.End) }) { Text("End") }
                OutlinedButton(onClick = { key(KeyData.Delete) }) { Text("Del") }
            }
            // Always in view: the arrows (held: repeat), Enter and Backspace.
            Row(Modifier.fillMaxWidth().padding(top = 8.dp), horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                RepeatKey("←", Modifier.weight(1f)) { key(KeyData.Left) }
                RepeatKey("↑", Modifier.weight(1f)) { key(KeyData.Up) }
                RepeatKey("↓", Modifier.weight(1f)) { key(KeyData.Down) }
                RepeatKey("→", Modifier.weight(1f)) { key(KeyData.Right) }
                RepeatKey("⌫", Modifier.weight(1f)) { key(KeyData.Backspace) }
                RepeatKey("⏎", Modifier.weight(1f)) { key(KeyData.Enter) }
            }
            KeyCatcher(focus, onText = ::text, onBackspace = { key(KeyData.Backspace) })
        }
    }
}

/**
 * The touchpad surface: move, two-finger scroll, tap to click (two fingers: right, three:
 * middle). Like a laptop touchpad, a one-finger tap waits [DRAG_WINDOW_MS]: touching again
 * within it holds the left button down while you move (select text, drag windows), and a
 * second quick tap makes it a double-click.
 */
@Composable
private fun Touchpad(mover: Mover, device: String, modifier: Modifier) {
    val slop = LocalViewConfiguration.current.touchSlop
    val scope = rememberCoroutineScope()
    // The click of the last one-finger tap, still waiting to see if a drag follows.
    val pendingClick = remember { arrayOfNulls<Job>(1) }
    Box(
        modifier
            .background(MaterialTheme.colorScheme.surfaceVariant, RoundedCornerShape(16.dp))
            .pointerInput(device) {
                awaitEachGesture {
                    val down = awaitFirstDown(requireUnconsumed = false)
                    val start = down.uptimeMillis
                    var fingers = 1
                    var travel = 0f
                    // Touched again right after a tap: hold the button down instead of clicking.
                    val dragging = pendingClick[0]?.isActive == true
                    if (dragging) {
                        pendingClick[0]?.cancel()
                        pendingClick[0] = null
                        Pairly.inputButton(device, MouseButtonData.LEFT, ButtonActionData.PRESS)
                    }
                    while (true) {
                        val event = awaitPointerEvent()
                        val pressed = event.changes.filter { it.pressed }
                        if (pressed.isEmpty()) {
                            val tap = travel < slop && event.changes.first().uptimeMillis - start < TAP_MS * fingers
                            when {
                                dragging -> {
                                    // Movement first, so the drag ends where the finger did.
                                    mover.flush()
                                    Pairly.inputButton(device, MouseButtonData.LEFT, ButtonActionData.RELEASE)
                                    // Tap, tap without moving: that was a double-click.
                                    if (tap) Pairly.inputButton(device, MouseButtonData.LEFT, ButtonActionData.CLICK)
                                }
                                tap && fingers == 1 -> {
                                    pendingClick[0] = scope.launch {
                                        delay(DRAG_WINDOW_MS)
                                        Pairly.inputButton(device, MouseButtonData.LEFT, ButtonActionData.CLICK)
                                    }
                                }
                                tap -> {
                                    val button = if (fingers == 2) MouseButtonData.RIGHT else MouseButtonData.MIDDLE
                                    Pairly.inputButton(device, button, ButtonActionData.CLICK)
                                }
                            }
                            break
                        }
                        fingers = maxOf(fingers, pressed.size)
                        val delta = pressed
                            .map { it.positionChange() }
                            .fold(Offset.Zero) { a, b -> a + b } / pressed.size.toFloat()
                        travel += abs(delta.x) + abs(delta.y)
                        if (travel >= slop) {
                            if (pressed.size == 1 && fingers == 1) mover.move(delta) else if (pressed.size >= 2) mover.scroll(delta)
                        }
                        event.changes.forEach { it.consume() }
                    }
                }
            },
        contentAlignment = Alignment.Center,
    ) {
        Text(
            stringResource(R.string.remote_touchpad_hint),
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            modifier = Modifier.padding(24.dp),
        )
    }
}

/** A mouse button you hold: press on touch, release on lift (drag with another finger). */
@Composable
private fun HoldButton(label: String, device: String, button: MouseButtonData, modifier: Modifier) {
    Box(
        modifier
            .height(56.dp)
            .background(MaterialTheme.colorScheme.secondaryContainer, RoundedCornerShape(12.dp))
            .pointerInput(device, button) {
                awaitEachGesture {
                    awaitFirstDown()
                    Pairly.inputButton(device, button, ButtonActionData.PRESS)
                    while (awaitPointerEvent().changes.any { it.pressed }) Unit
                    Pairly.inputButton(device, button, ButtonActionData.RELEASE)
                }
            },
        contentAlignment = Alignment.Center,
    ) {
        Text(label, color = MaterialTheme.colorScheme.onSecondaryContainer)
    }
}

/** A key that sends once when touched and repeats while held, like a keyboard's. */
@Composable
private fun RepeatKey(label: String, modifier: Modifier, onKey: () -> Unit) {
    val scope = rememberCoroutineScope()
    Box(
        modifier
            .height(48.dp)
            .background(MaterialTheme.colorScheme.secondaryContainer, RoundedCornerShape(12.dp))
            .pointerInput(label) {
                awaitEachGesture {
                    awaitFirstDown()
                    onKey()
                    val repeat = scope.launch {
                        delay(REPEAT_DELAY_MS)
                        while (true) {
                            onKey()
                            delay(REPEAT_MS)
                        }
                    }
                    while (awaitPointerEvent().changes.any { it.pressed }) Unit
                    repeat.cancel()
                }
            },
        contentAlignment = Alignment.Center,
    ) {
        Text(label, style = MaterialTheme.typography.titleMedium, color = MaterialTheme.colorScheme.onSecondaryContainer)
    }
}

/**
 * An invisible text field for the phone's keyboard. It always holds one placeholder character,
 * so typed text shows up as additions and Backspace as the placeholder being deleted.
 */
@Composable
private fun KeyCatcher(focus: FocusRequester, onText: (String) -> Unit, onBackspace: () -> Unit) {
    val sentinel = "​"
    var value by remember { mutableStateOf(TextFieldValue(sentinel, TextRange(1))) }
    BasicTextField(
        value = value,
        onValueChange = { new ->
            when {
                new.text.isEmpty() -> onBackspace()
                new.text.length > sentinel.length -> {
                    val typed = new.text.replace(sentinel, "")
                    if (typed.isNotEmpty()) onText(typed)
                }
            }
            value = TextFieldValue(sentinel, TextRange(1))
        },
        keyboardOptions = KeyboardOptions(autoCorrectEnabled = false),
        modifier = Modifier.size(1.dp).focusRequester(focus),
    )
}

/** Hold to show the laser pointer on the PC; it follows the phone until you let go. */
@Composable
private fun PointerButton(laser: LaserPointer, modifier: Modifier) {
    var held by remember { mutableStateOf(false) }
    Box(
        modifier
            .background(
                if (held) MaterialTheme.colorScheme.errorContainer else MaterialTheme.colorScheme.tertiaryContainer,
                RoundedCornerShape(24.dp),
            )
            .pointerInput(laser) {
                awaitEachGesture {
                    awaitFirstDown()
                    held = true
                    laser.start()
                    while (awaitPointerEvent().changes.any { it.pressed }) Unit
                    held = false
                    laser.stop()
                }
            },
        contentAlignment = Alignment.Center,
    ) {
        Text(
            stringResource(if (held) R.string.presenter_pointing else R.string.presenter_pointer),
            style = MaterialTheme.typography.titleLarge,
            color = if (held) MaterialTheme.colorScheme.onErrorContainer else MaterialTheme.colorScheme.onTertiaryContainer,
        )
    }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun PresenterScreen(device: Device, onBack: () -> Unit) {
    BackHandler(onBack = onBack)
    val none = ModifiersData(ctrl = false, alt = false, shift = false, logo = false)
    fun key(k: KeyData) = Pairly.inputKey(device.id, null, k, none)
    val focus = remember { FocusRequester() }
    val view = LocalView.current
    // Keep the screen on and catch the volume keys while presenting.
    DisposableEffect(view) {
        view.keepScreenOn = true
        onDispose { view.keepScreenOn = false }
    }
    LaunchedEffect(Unit) { focus.requestFocus() }
    val context = LocalContext.current
    val laser = remember(device.id) { LaserPointer(context, device.id) }
    DisposableEffect(laser) { onDispose { laser.stop() } }
    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text(stringResource(R.string.presenter_title, device.name)) },
                actions = { TextButton(onClick = onBack) { Text(stringResource(R.string.action_done)) } },
            )
        },
    ) { padding ->
        Column(
            Modifier
                .padding(padding)
                .fillMaxSize()
                .padding(16.dp)
                .focusRequester(focus)
                .focusable()
                .onPreviewKeyEvent { e ->
                    if (e.type != KeyEventType.KeyDown) return@onPreviewKeyEvent e.key == Key.VolumeUp || e.key == Key.VolumeDown
                    when (e.key) {
                        Key.VolumeDown -> { key(KeyData.Right); true }
                        Key.VolumeUp -> { key(KeyData.Left); true }
                        else -> false
                    }
                },
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            Text(
                stringResource(R.string.presenter_hint),
                style = MaterialTheme.typography.bodyMedium,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            Button(onClick = { key(KeyData.Right) }, modifier = Modifier.fillMaxWidth().weight(2f)) {
                Text(stringResource(R.string.presenter_next), style = MaterialTheme.typography.headlineMedium)
            }
            if (laser.available) PointerButton(laser, Modifier.fillMaxWidth().weight(1f))
            FilledTonalButton(onClick = { key(KeyData.Left) }, modifier = Modifier.fillMaxWidth().weight(1f)) {
                Text(stringResource(R.string.presenter_previous), style = MaterialTheme.typography.titleLarge)
            }
            Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                OutlinedButton(onClick = { key(KeyData.Function(5u)) }, modifier = Modifier.weight(1f)) {
                    Text(stringResource(R.string.presenter_start))
                }
                OutlinedButton(onClick = { key(KeyData.Escape) }, modifier = Modifier.weight(1f)) {
                    Text(stringResource(R.string.presenter_end))
                }
                OutlinedButton(
                    onClick = { Pairly.inputKey(device.id, "b", null, none) },
                    modifier = Modifier.weight(1f),
                ) { Text(stringResource(R.string.presenter_black)) }
            }
        }
    }
}
