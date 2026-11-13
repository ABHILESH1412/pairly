package dev.pairly.android.helper

import android.graphics.Rect
import android.hardware.display.DisplayManager
import android.hardware.display.VirtualDisplay
import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaFormat
import android.os.Build
import android.os.Bundle
import android.os.IBinder
import android.os.Looper
import android.os.SystemClock
import android.view.InputDevice
import android.view.InputEvent
import android.view.KeyCharacterMap
import android.view.KeyEvent
import android.view.MotionEvent
import android.view.Surface
import java.io.File
import java.net.InetAddress
import java.net.ServerSocket
import java.net.Socket
import java.nio.ByteBuffer
import java.util.concurrent.Executors
import kotlin.math.hypot
import kotlin.system.exitProcess

/**
 * The screen helper. It runs as Android's "shell" user, started over adb like scrcpy's server:
 *
 *     CLASSPATH=<Pairly's APK> app_process / dev.pairly.android.helper.Helper --uid <Pairly's uids>
 *
 * (one uid per phone profile Pairly is installed in, comma-separated)
 *
 * As the shell user it may record the screen without the "Start recording?" prompt and inject
 * real touches and keys (no accessibility service). It keeps running until the phone restarts
 * and serves only the Pairly app: it listens on 127.0.0.1 and closes any connection another
 * app opened (each connection's owner is in /proc/net/tcp).
 *
 * Runs outside any app: no Context, only system calls (some hidden, reached by reflection).
 */
object Helper {
    private const val TAG = "pairly-helper"

    @JvmStatic
    fun main(args: Array<String>) {
        val uids = args.indexOf("--uid").takeIf { it >= 0 }
            ?.let { args.getOrNull(it + 1) }
            ?.split(',')
            ?.mapNotNull { it.trim().toIntOrNull() }
            .orEmpty()
        if (uids.isEmpty()) {
            System.err.println("usage: Helper --uid <Pairly's app uids, comma-separated>")
            exitProcess(2)
        }
        // Some system services expect the process to have a main looper.
        runCatching { Looper.prepareMainLooper() }
        val server = try {
            ServerSocket(HelperProtocol.PORT, 1, InetAddress.getLoopbackAddress())
        } catch (e: Exception) {
            System.err.println("$TAG: not starting, port taken (already running?): $e")
            return
        }
        println("$TAG: serving uids $uids on ${HelperProtocol.PORT}")
        var current: Session? = null
        while (true) {
            val socket = runCatching { server.accept() }.getOrNull() ?: continue
            if (ownerUid(socket) !in uids) {
                runCatching { socket.close() }
                continue
            }
            // One app session at a time: a new connection replaces the old one.
            current?.close()
            current = Session(socket).also { s -> Thread({ s.run() }, "pairly-session").start() }
        }
    }

    /** The uid of the app that opened `socket` (its end of the connection in /proc/net/tcp). */
    private fun ownerUid(socket: Socket): Int? {
        val theirPort = socket.port
        for (table in listOf("/proc/net/tcp6", "/proc/net/tcp")) {
            val lines = runCatching { File(table).readLines() }.getOrNull() ?: continue
            for (line in lines.drop(1)) {
                val f = line.trim().split(Regex("\\s+"))
                if (f.size < 8) continue
                val local = f[1].substringAfterLast(':').toIntOrNull(16)
                val remote = f[2].substringAfterLast(':').toIntOrNull(16)
                if (local == theirPort && remote == HelperProtocol.PORT) return f[7].toIntOrNull()
            }
        }
        return null
    }
}

/** One connection from the app: commands in, video out. */
private class Session(socket: Socket) {
    private val wire = HelperProtocol.Wire(socket)
    private var recorder: Recorder? = null
    private val input = Executors.newSingleThreadExecutor()

    fun run() {
        try {
            wire.send(HelperProtocol.HELLO, ByteBuffer.allocate(4).putInt(HelperProtocol.VERSION).array())
            while (true) {
                val (kind, body) = wire.read() ?: break
                val b = ByteBuffer.wrap(body)
                when (kind) {
                    HelperProtocol.START -> {
                        recorder?.finish()
                        recorder = Recorder(wire, b.int, b.int, b.int).also { it.start() }
                    }
                    HelperProtocol.STOP -> {
                        recorder?.finish()
                        recorder = null
                    }
                    HelperProtocol.KEY_FRAME -> recorder?.keyFrame()
                    HelperProtocol.TAP -> { val (x, y) = b.float to b.float; input.execute { Input.stroke(listOf(x to y), 40) } }
                    HelperProtocol.LONG_PRESS -> { val (x, y) = b.float to b.float; input.execute { Input.stroke(listOf(x to y), 650) } }
                    HelperProtocol.SWIPE -> {
                        val duration = b.int.toLong()
                        val points = List(b.short.toInt()) { b.float to b.float }
                        if (points.size >= 2) input.execute { Input.stroke(points, duration) }
                    }
                    HelperProtocol.KEY -> { val code = b.int; input.execute { Input.key(code) } }
                    HelperProtocol.TEXT -> { val text = String(body, Charsets.UTF_8); input.execute { Input.text(text) } }
                }
            }
        } catch (e: Exception) {
            System.err.println("pairly-helper: session ended: $e")
        } finally {
            close()
        }
    }

    fun close() {
        recorder?.finish()
        recorder = null
        input.shutdownNow()
        wire.close()
    }
}

/** The main display's size now, in pixels, as it's shown (swapped when rotated). */
private object Screen {
    private val info: () -> Any = {
        val global = Class.forName("android.hardware.display.DisplayManagerGlobal")
            .getMethod("getInstance").invoke(null)!!
        global.javaClass.getMethod("getDisplayInfo", Int::class.javaPrimitiveType).invoke(global, 0)!!
    }

    fun size(): Pair<Int, Int> {
        val i = info()
        return i.javaClass.getField("logicalWidth").getInt(i) to i.javaClass.getField("logicalHeight").getInt(i)
    }

    fun layerStack(): Int = runCatching {
        val i = info()
        i.javaClass.getField("layerStack").getInt(i)
    }.getOrDefault(0)
}

/**
 * Records the main display into the hardware H.264 encoder: a mirror of it through a virtual
 * display (the shell user may make one of the real screen). Starts over when the screen
 * rotates, with the new size.
 */
private class Recorder(
    private val wire: HelperProtocol.Wire,
    private val maxSide: Int,
    private val bitRate: Int,
    private val frameRate: Int,
) : Thread("pairly-recorder") {
    @Volatile
    private var running = true

    @Volatile
    private var codec: MediaCodec? = null

    fun finish() {
        running = false
        join(2_000)
    }

    fun keyFrame() {
        runCatching { codec?.setParameters(Bundle().apply { putInt(MediaCodec.PARAMETER_KEY_REQUEST_SYNC_FRAME, 0) }) }
    }

    override fun run() {
        try {
            while (running) record()
        } catch (e: Exception) {
            System.err.println("pairly-helper: recording failed: $e")
            runCatching { wire.send(HelperProtocol.STOPPED, "the screen recording failed: $e".toByteArray()) }
        }
    }

    /** Record until stopped or the screen's size changes. */
    private fun record() {
        val (dw, dh) = Screen.size()
        val scale = minOf(1f, maxSide.toFloat() / maxOf(dw, dh))
        val w = (dw * scale).toInt() / 8 * 8
        val h = (dh * scale).toInt() / 8 * 8
        val format = MediaFormat.createVideoFormat(MediaFormat.MIMETYPE_VIDEO_AVC, w, h).apply {
            setInteger(MediaFormat.KEY_COLOR_FORMAT, MediaCodecInfo.CodecCapabilities.COLOR_FormatSurface)
            setInteger(MediaFormat.KEY_BIT_RATE, bitRate)
            setInteger(MediaFormat.KEY_FRAME_RATE, frameRate)
            setInteger(MediaFormat.KEY_I_FRAME_INTERVAL, 2)
            // Repeat the last picture when nothing changes, so a still screen keeps flowing.
            setLong(MediaFormat.KEY_REPEAT_PREVIOUS_FRAME_AFTER, 100_000L)
        }
        val encoder = MediaCodec.createEncoderByType(MediaFormat.MIMETYPE_VIDEO_AVC)
        codec = encoder
        encoder.configure(format, null, null, MediaCodec.CONFIGURE_FLAG_ENCODE)
        val surface = encoder.createInputSurface()
        encoder.start()
        val release = Mirror.create(surface, dw, dh, w, h)
        try {
            wire.send(HelperProtocol.STARTED, ByteBuffer.allocate(8).putInt(w).putInt(h).array())
            val info = MediaCodec.BufferInfo()
            var checked = SystemClock.uptimeMillis()
            while (running) {
                val index = encoder.dequeueOutputBuffer(info, 100_000)
                if (index >= 0) {
                    val buffer = encoder.getOutputBuffer(index)
                    if (buffer != null && info.size > 0) {
                        val out = ByteArray(1 + info.size)
                        val config = info.flags and MediaCodec.BUFFER_FLAG_CODEC_CONFIG != 0
                        val key = info.flags and MediaCodec.BUFFER_FLAG_KEY_FRAME != 0
                        out[0] = ((if (key) 1 else 0) or (if (config) 2 else 0)).toByte()
                        buffer.position(info.offset)
                        buffer.get(out, 1, info.size)
                        wire.send(HelperProtocol.FRAME, out)
                    }
                    encoder.releaseOutputBuffer(index, false)
                }
                val now = SystemClock.uptimeMillis()
                if (now - checked > 500) {
                    checked = now
                    if (Screen.size() != dw to dh) break // rotated: start over at the new size
                }
            }
        } finally {
            runCatching { release() }
            runCatching { encoder.stop() }
            runCatching { encoder.release() }
            surface.release()
            codec = null
        }
    }
}

/** A copy of the main display drawn into a surface, scaled to `w`×`h`. Returns its release. */
private object Mirror {
    fun create(surface: Surface, dw: Int, dh: Int, w: Int, h: Int): () -> Unit {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            // Android 14+: a hidden static DisplayManager call mirrors a display.
            runCatching {
                val display = DisplayManager::class.java.getMethod(
                    "createVirtualDisplay",
                    String::class.java,
                    Int::class.javaPrimitiveType,
                    Int::class.javaPrimitiveType,
                    Int::class.javaPrimitiveType,
                    Surface::class.java,
                ).invoke(null, "pairly", w, h, 0, surface) as VirtualDisplay
                return { display.release() }
            }.onFailure { System.err.println("pairly-helper: no mirror display ($it), trying SurfaceControl") }
        }
        // Older Android: a SurfaceControl display showing the main display's layers.
        val sc = Class.forName("android.view.SurfaceControl")
        val secure = Build.VERSION.SDK_INT < Build.VERSION_CODES.R
        val token = sc.getMethod("createDisplay", String::class.java, Boolean::class.javaPrimitiveType)
            .invoke(null, "pairly", secure) as IBinder
        sc.getMethod("openTransaction").invoke(null)
        try {
            sc.getMethod("setDisplaySurface", IBinder::class.java, Surface::class.java).invoke(null, token, surface)
            sc.getMethod(
                "setDisplayProjection",
                IBinder::class.java,
                Int::class.javaPrimitiveType,
                Rect::class.java,
                Rect::class.java,
            ).invoke(null, token, 0, Rect(0, 0, dw, dh), Rect(0, 0, w, h))
            sc.getMethod("setDisplayLayerStack", IBinder::class.java, Int::class.javaPrimitiveType)
                .invoke(null, token, Screen.layerStack())
        } finally {
            sc.getMethod("closeTransaction").invoke(null)
        }
        return { sc.getMethod("destroyDisplay", IBinder::class.java).invoke(null, token) }
    }
}

/** Real touches and keys, injected the way a USB keyboard's or the touchscreen's arrive. */
private object Input {
    private val manager: Any by lazy {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            Class.forName("android.hardware.input.InputManagerGlobal").getMethod("getInstance").invoke(null)!!
        } else {
            android.hardware.input.InputManager::class.java.getMethod("getInstance").invoke(null)!!
        }
    }
    private val injectMethod by lazy {
        manager.javaClass.getMethod("injectInputEvent", InputEvent::class.java, Int::class.javaPrimitiveType)
    }

    private fun inject(event: InputEvent) {
        // 0: INJECT_INPUT_EVENT_MODE_ASYNC.
        runCatching { injectMethod.invoke(manager, event, 0) }
            .onFailure { System.err.println("pairly-helper: input failed: $it") }
    }

    private fun touch(action: Int, down: Long, x: Float, y: Float) {
        val event = MotionEvent.obtain(down, SystemClock.uptimeMillis(), action, x, y, 0)
        event.source = InputDevice.SOURCE_TOUCHSCREEN
        inject(event)
        event.recycle()
    }

    /** A finger down on the first point, through the rest over `durationMs`, then up. */
    fun stroke(fractions: List<Pair<Float, Float>>, durationMs: Long) {
        val (w, h) = Screen.size()
        val points = fractions.map { (x, y) -> x * (w - 1) to y * (h - 1) }
        val down = SystemClock.uptimeMillis()
        touch(MotionEvent.ACTION_DOWN, down, points[0].first, points[0].second)
        val lengths = points.zipWithNext { a, b -> hypot(b.first - a.first, b.second - a.second) }
        val total = lengths.sum()
        val steps = (durationMs / 10).coerceAtLeast(1)
        for (step in 1..steps) {
            Thread.sleep(durationMs / steps)
            if (total == 0f) continue
            // The point this far along the path.
            var along = total * step / steps
            var i = 0
            while (i < lengths.size - 1 && along > lengths[i]) along -= lengths[i++]
            val t = if (lengths[i] == 0f) 1f else (along / lengths[i]).coerceIn(0f, 1f)
            val (a, b) = points[i] to points[i + 1]
            touch(MotionEvent.ACTION_MOVE, down, a.first + (b.first - a.first) * t, a.second + (b.second - a.second) * t)
        }
        val last = points.last()
        touch(MotionEvent.ACTION_UP, down, last.first, last.second)
    }

    fun key(code: Int) {
        val now = SystemClock.uptimeMillis()
        for (action in listOf(KeyEvent.ACTION_DOWN, KeyEvent.ACTION_UP)) {
            inject(KeyEvent(now, now, action, code, 0, 0, KeyCharacterMap.VIRTUAL_KEYBOARD, 0, 0, InputDevice.SOURCE_KEYBOARD))
        }
    }

    /** Typed as key presses: letters, digits and the symbols on a keyboard. */
    fun text(text: String) {
        val events = KeyCharacterMap.load(KeyCharacterMap.VIRTUAL_KEYBOARD).getEvents(text.toCharArray()) ?: return
        events.forEach { inject(it) }
    }
}
