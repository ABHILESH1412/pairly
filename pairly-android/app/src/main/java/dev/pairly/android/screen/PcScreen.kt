package dev.pairly.android.screen

import android.content.Context
import android.content.Intent
import android.content.pm.ActivityInfo
import android.content.res.Configuration
import android.graphics.SurfaceTexture
import android.media.MediaCodec
import android.media.MediaFormat
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.HandlerThread
import android.util.Log
import android.view.Surface
import android.view.TextureView
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.compose.foundation.background
import androidx.compose.foundation.gestures.awaitEachGesture
import androidx.compose.foundation.gestures.awaitFirstDown
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.outlined.Keyboard
import androidx.compose.material.icons.outlined.ScreenRotation
import androidx.compose.material.icons.outlined.ZoomInMap
import androidx.compose.material.icons.outlined.ZoomOutMap
import androidx.compose.material3.FilledTonalIconButton
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButtonDefaults
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.SideEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableFloatStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.platform.LocalSoftwareKeyboardController
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.TextRange
import androidx.compose.ui.text.input.TextFieldValue
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.WindowInsetsControllerCompat
import dev.pairly.android.Pairly
import dev.pairly.android.R
import dev.pairly.android.ui.theme.PairlyTheme
import dev.pairly.core.ffi.KeyData
import dev.pairly.core.ffi.ModifiersData
import dev.pairly.core.ffi.PointData
import dev.pairly.core.ffi.ScreenInputData

/** The PC's stream reaching the phone (from the core, on its own threads). */
object PcScreenSession {
    sealed interface Event {
        data class Started(val width: Int, val height: Int) : Event
        class Frame(val data: ByteArray, val key: Boolean) : Event
        data class Stopped(val reason: String) : Event
    }

    @Volatile
    private var watching: Pair<String, (Event) -> Unit>? = null

    fun watch(device: String, listener: (Event) -> Unit) {
        watching = device to listener
    }

    fun unwatch() {
        watching = null
    }

    fun deliver(device: String, event: Event) {
        val (who, listener) = watching ?: return
        if (who == device) listener(event)
    }
}

/**
 * The PC's screen, full screen, controlled by touch: tap to click, hold for a right click, drag
 * to drag, two fingers to scroll, pinch to zoom, and buttons to turn the view, zoom and type.
 */
class PcScreenActivity : ComponentActivity() {
    private lateinit var device: String

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        device = intent.getStringExtra(EXTRA_DEVICE) ?: run { finish(); return }
        val name = intent.getStringExtra(EXTRA_NAME).orEmpty()
        WindowCompat.setDecorFitsSystemWindows(window, false)
        WindowInsetsControllerCompat(window, window.decorView).apply {
            hide(WindowInsetsCompat.Type.systemBars())
            systemBarsBehavior = WindowInsetsControllerCompat.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
        }
        window.decorView.keepScreenOn = true
        setContent { PairlyTheme { PcScreen(device, name, ::rotate) } }
        if (savedInstanceState == null) Pairly.screenRequest(device, true)
    }

    /** Turn the view between landscape and portrait, whichever way the phone is held. */
    private fun rotate() {
        requestedOrientation = if (resources.configuration.orientation == Configuration.ORIENTATION_LANDSCAPE) {
            ActivityInfo.SCREEN_ORIENTATION_SENSOR_PORTRAIT
        } else {
            ActivityInfo.SCREEN_ORIENTATION_SENSOR_LANDSCAPE
        }
    }

    override fun onDestroy() {
        PcScreenSession.unwatch()
        if (isFinishing) Pairly.screenRequest(device, false)
        super.onDestroy()
    }

    companion object {
        private const val EXTRA_DEVICE = "device"
        private const val EXTRA_NAME = "name"

        fun open(context: Context, device: String, name: String) {
            context.startActivity(
                Intent(context, PcScreenActivity::class.java)
                    .putExtra(EXTRA_DEVICE, device)
                    .putExtra(EXTRA_NAME, name),
            )
        }
    }
}

/** Hardware H.264 decoding straight onto a surface, fed frame by frame. */
private class Decoder(surface: Surface, width: Int, height: Int) {
    private val thread = HandlerThread("pairly-decode").apply { start() }
    private val handler = Handler(thread.looper)
    private val codec = MediaCodec.createDecoderByType(MediaFormat.MIMETYPE_VIDEO_AVC)
    private val freeInputs = ArrayDeque<Int>()
    private val frames = ArrayDeque<ByteArray>()
    private var synced = false

    init {
        val format = MediaFormat.createVideoFormat(MediaFormat.MIMETYPE_VIDEO_AVC, width, height).apply {
            setInteger(MediaFormat.KEY_MAX_INPUT_SIZE, 2 * 1024 * 1024)
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) setInteger(MediaFormat.KEY_LOW_LATENCY, 1)
        }
        codec.setCallback(
            object : MediaCodec.Callback() {
                override fun onInputBufferAvailable(codec: MediaCodec, index: Int) {
                    freeInputs.addLast(index)
                    drain()
                }

                override fun onOutputBufferAvailable(codec: MediaCodec, index: Int, info: MediaCodec.BufferInfo) {
                    runCatching { codec.releaseOutputBuffer(index, true) }
                }

                override fun onError(codec: MediaCodec, e: MediaCodec.CodecException) {
                    Log.w(PhoneScreen.TAG, "decoder error", e)
                }

                override fun onOutputFormatChanged(codec: MediaCodec, format: MediaFormat) {}
            },
            handler,
        )
        codec.configure(format, surface, null, 0)
        codec.start()
    }

    /** Start at a key frame; if frames pile up (a slow moment), skip to the next one. */
    fun push(data: ByteArray, key: Boolean) {
        handler.post {
            if (key && frames.size > MAX_BACKLOG) frames.clear()
            if (!synced && !key) return@post
            synced = true
            frames.addLast(data)
            drain()
        }
    }

    private fun drain() {
        while (freeInputs.isNotEmpty() && frames.isNotEmpty()) {
            val index = freeInputs.removeFirst()
            val frame = frames.removeFirst()
            runCatching {
                val buffer = codec.getInputBuffer(index) ?: return@runCatching
                buffer.clear()
                buffer.put(frame, 0, minOf(frame.size, buffer.capacity()))
                codec.queueInputBuffer(index, 0, minOf(frame.size, buffer.capacity()), System.nanoTime() / 1000, 0)
            }
        }
    }

    fun release() {
        handler.post {
            runCatching { codec.stop() }
            runCatching { codec.release() }
            thread.quitSafely()
        }
    }

    private companion object {
        const val MAX_BACKLOG = 10
    }
}

/**
 * How the PC's screen sits on the phone: fitted, then zoomed by [scale] around its centre and
 * moved by [offset] (in pixels). [area] is the whole view, [fit] the fitted video's size.
 */
private class Viewport {
    var scale by mutableFloatStateOf(1f)
    var offset by mutableStateOf(Offset.Zero)
    var area = Size.Zero
    var fit = Size.Zero

    private val center get() = Offset(area.width / 2, area.height / 2)

    /** Where a touch at [p] lands on the PC's screen, as fractions of it. */
    fun frac(p: Offset): Pair<Float, Float> {
        if (fit.width <= 0f || fit.height <= 0f) return 0f to 0f
        val q = (p - center - offset) / scale
        return ((q.x + fit.width / 2) / fit.width).coerceIn(0f, 1f) to
            ((q.y + fit.height / 2) / fit.height).coerceIn(0f, 1f)
    }

    /** The video's size on the phone now. */
    val shown get() = Size(fit.width * scale, fit.height * scale)

    /** Zoom by [factor] keeping [focus] in place, then move by [pan]. */
    fun pinch(focus: Offset, factor: Float, pan: Offset) {
        val next = (scale * factor).coerceIn(1f, MAX_ZOOM)
        val f = focus - center
        offset = f - (f - offset) * (next / scale) + pan
        scale = next
        clamp()
    }

    /** Fitted ↔ zoomed in: filling the phone's screen (or twice the size, if that's more). */
    fun toggle() {
        scale = if (scale > 1.01f || fit.width <= 0f) {
            1f
        } else {
            maxOf(area.width / fit.width, area.height / fit.height, 2f).coerceAtMost(MAX_ZOOM)
        }
        offset = Offset.Zero
        clamp()
    }

    /** Keep the video on screen: no empty edge where there is more video to see. */
    fun clamp() {
        val maxX = maxOf(0f, (fit.width * scale - area.width) / 2)
        val maxY = maxOf(0f, (fit.height * scale - area.height) / 2)
        offset = Offset(offset.x.coerceIn(-maxX, maxX), offset.y.coerceIn(-maxY, maxY))
    }

    companion object {
        const val MAX_ZOOM = 5f
    }
}

@Composable
private fun PcScreen(device: String, name: String, onRotate: () -> Unit) {
    var size by remember { mutableStateOf<Pair<Int, Int>?>(null) }
    var status by remember { mutableStateOf<String?>(null) }
    val decoderHolder = remember { arrayOfNulls<Decoder>(1) }
    val surfaceHolder = remember { arrayOfNulls<Surface>(1) }
    val viewport = remember { Viewport() }
    val waiting = stringResource(R.string.pc_screen_waiting, name)

    fun startDecoder() {
        val s = surfaceHolder[0] ?: return
        val (w, h) = size ?: return
        if (decoderHolder[0] == null) decoderHolder[0] = runCatching { Decoder(s, w, h) }.getOrNull()
    }

    fun stopDecoder() {
        decoderHolder[0]?.release()
        decoderHolder[0] = null
    }

    DisposableEffect(device) {
        val main = android.os.Handler(android.os.Looper.getMainLooper())
        PcScreenSession.watch(device) { event ->
            when (event) {
                // State changes go to the main thread.
                is PcScreenSession.Event.Started -> main.post {
                    size = event.width to event.height
                    status = null
                    startDecoder()
                }
                is PcScreenSession.Event.Frame -> decoderHolder[0]?.push(event.data, event.key)
                is PcScreenSession.Event.Stopped -> main.post {
                    status = event.reason
                    stopDecoder()
                }
            }
        }
        onDispose {
            PcScreenSession.unwatch()
            stopDecoder()
        }
    }

    val focus = remember { FocusRequester() }
    val keyboard = LocalSoftwareKeyboardController.current
    BoxWithConstraints(
        Modifier.fillMaxSize().background(Color.Black).touchControl(device, viewport),
        contentAlignment = Alignment.Center,
    ) {
        val (w, h) = size ?: (16 to 9)
        val area = Size(constraints.maxWidth.toFloat(), constraints.maxHeight.toFloat())
        val ratio = w.toFloat() / h.toFloat()
        val fitWidth = minOf(area.width, area.height * ratio)
        val fit = Size(fitWidth, fitWidth / ratio)
        SideEffect {
            viewport.area = area
            viewport.fit = fit
            viewport.clamp() // after turning the phone
        }
        val density = LocalDensity.current
        AndroidView(
            factory = { context ->
                TextureView(context).apply {
                    surfaceTextureListener = object : TextureView.SurfaceTextureListener {
                        override fun onSurfaceTextureAvailable(texture: SurfaceTexture, width: Int, height: Int) {
                            surfaceHolder[0] = Surface(texture)
                            startDecoder()
                        }

                        override fun onSurfaceTextureSizeChanged(texture: SurfaceTexture, width: Int, height: Int) {}

                        override fun onSurfaceTextureDestroyed(texture: SurfaceTexture): Boolean {
                            stopDecoder()
                            surfaceHolder[0]?.release()
                            surfaceHolder[0] = null
                            return true
                        }

                        override fun onSurfaceTextureUpdated(texture: SurfaceTexture) {}
                    }
                }
            },
            modifier = Modifier
                .size(with(density) { fit.width.toDp() }, with(density) { fit.height.toDp() })
                .graphicsLayer {
                    scaleX = viewport.scale
                    scaleY = viewport.scale
                    translationX = viewport.offset.x
                    translationY = viewport.offset.y
                },
        )
        if (size == null || status != null) {
            Text(
                status?.let { stringResource(R.string.pc_screen_ended, it) } ?: waiting,
                color = Color.White,
                style = MaterialTheme.typography.titleMedium,
                modifier = Modifier.padding(32.dp),
            )
        }
        Column(
            Modifier.align(Alignment.CenterEnd).padding(8.dp),
            verticalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            ScreenButton(Icons.Outlined.ScreenRotation, stringResource(R.string.pc_screen_rotate), onRotate)
            ScreenButton(
                if (viewport.scale > 1.01f) Icons.Outlined.ZoomInMap else Icons.Outlined.ZoomOutMap,
                stringResource(if (viewport.scale > 1.01f) R.string.pc_screen_fit else R.string.pc_screen_zoom),
            ) { viewport.toggle() }
            ScreenButton(Icons.Outlined.Keyboard, stringResource(R.string.remote_keyboard)) {
                focus.requestFocus()
                keyboard?.show()
            }
        }
        PcKeyCatcher(device, focus)
    }
}

/** A small see-through button over the PC's screen. */
@Composable
private fun ScreenButton(icon: ImageVector, label: String, onClick: () -> Unit) {
    FilledTonalIconButton(
        onClick = onClick,
        colors = IconButtonDefaults.filledTonalIconButtonColors(
            containerColor = MaterialTheme.colorScheme.secondaryContainer.copy(alpha = 0.6f),
        ),
        modifier = Modifier.size(44.dp),
    ) { Icon(icon, contentDescription = label) }
}

/** How a two-finger touch is read: undecided yet, pinching (zoom and move), or scrolling. */
private enum class TwoFingers { Undecided, Pinch, Scroll }

/**
 * Touch as a touch screen for the PC: tap = click, hold = right click, drag = drag (sent when
 * the finger lifts), two fingers = scroll (sent as you move). Pinching zooms the phone's view;
 * keep the two fingers down after pinching to move around.
 */
private fun Modifier.touchControl(device: String, viewport: Viewport): Modifier = pointerInput(device, viewport) {
    val slop = viewConfiguration.touchSlop
    awaitEachGesture {
        val down = awaitFirstDown(requireUnconsumed = false)
        val start = down.uptimeMillis
        val points = mutableListOf(viewport.frac(down.position))
        var travel = 0f
        var fingers = 1
        var two = TwoFingers.Undecided
        var spread = 0f
        var middle = Offset.Zero
        var scrollX = 0f
        var scrollY = 0f
        var lastScroll = start
        var lastTime = start
        while (true) {
            val event = awaitPointerEvent()
            val now = event.changes.first().uptimeMillis
            lastTime = now
            val pressed = event.changes.filter { it.pressed }
            if (pressed.isEmpty()) break
            if (pressed.size >= 2) {
                val (a, b) = pressed
                val mid = (a.position + b.position) / 2f
                val dist = (a.position - b.position).getDistance()
                val prevMid = (a.previousPosition + b.previousPosition) / 2f
                val prevDist = (a.previousPosition - b.previousPosition).getDistance()
                if (fingers < 2) {
                    fingers = 2
                    spread = dist
                    middle = mid
                }
                if (two == TwoFingers.Undecided) {
                    two = when {
                        kotlin.math.abs(dist - spread) > slop * 2 -> TwoFingers.Pinch
                        (mid - middle).getDistance() > slop -> TwoFingers.Scroll
                        else -> TwoFingers.Undecided
                    }
                }
                when (two) {
                    TwoFingers.Pinch -> viewport.pinch(mid, if (prevDist > 0f) dist / prevDist else 1f, mid - prevMid)
                    TwoFingers.Scroll -> {
                        val shown = viewport.shown
                        scrollX += (mid.x - prevMid.x) / shown.width
                        scrollY += (mid.y - prevMid.y) / shown.height
                        if (now - lastScroll >= SCROLL_EVERY_MS && (scrollX != 0f || scrollY != 0f)) {
                            Pairly.screenInput(device, ScreenInputData.Scroll(scrollX, scrollY))
                            scrollX = 0f
                            scrollY = 0f
                            lastScroll = now
                        }
                    }
                    TwoFingers.Undecided -> {}
                }
            } else if (fingers == 1) {
                val change = pressed.first()
                val delta = change.position - change.previousPosition
                travel += kotlin.math.abs(delta.x) + kotlin.math.abs(delta.y)
                if (points.size < MAX_POINTS) points += viewport.frac(change.position)
            }
            event.changes.forEach { it.consume() }
        }
        if (fingers >= 2) return@awaitEachGesture
        val (x, y) = points.first()
        val duration = lastTime - start
        when {
            travel < slop && duration >= LONG_PRESS_MS -> Pairly.screenInput(device, ScreenInputData.LongPress(x, y))
            travel < slop -> Pairly.screenInput(device, ScreenInputData.Tap(x, y))
            else -> Pairly.screenInput(
                device,
                ScreenInputData.Swipe(points.map { PointData(it.first, it.second) }, duration.toUInt()),
            )
        }
    }
}

private const val SCROLL_EVERY_MS = 30L
private const val LONG_PRESS_MS = 500L
private const val MAX_POINTS = 120

/** The phone's keyboard, typing on the PC (through remote input). */
@Composable
private fun PcKeyCatcher(device: String, focus: FocusRequester) {
    val sentinel = "​"
    val none = ModifiersData(ctrl = false, alt = false, shift = false, logo = false)
    var value by remember { mutableStateOf(TextFieldValue(sentinel, TextRange(1))) }
    BasicTextField(
        value = value,
        onValueChange = { new ->
            when {
                new.text.isEmpty() -> Pairly.inputKey(device, null, KeyData.Backspace, none)
                new.text.length > sentinel.length -> {
                    val typed = new.text.replace(sentinel, "")
                    typed.split("\n").forEachIndexed { i, part ->
                        if (i > 0) Pairly.inputKey(device, null, KeyData.Enter, none)
                        if (part.isNotEmpty()) Pairly.inputKey(device, part, null, none)
                    }
                }
            }
            value = TextFieldValue(sentinel, TextRange(1))
        },
        keyboardOptions = KeyboardOptions(autoCorrectEnabled = false),
        modifier = Modifier.size(1.dp).focusRequester(focus),
    )
}
