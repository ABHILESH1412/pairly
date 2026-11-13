package dev.pairly.android.screen

import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.util.Log
import android.view.KeyEvent
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import dev.pairly.android.Notifications
import dev.pairly.android.Pairly
import dev.pairly.android.R
import dev.pairly.android.device.PairlyAccessibility
import dev.pairly.android.helper.HelperProtocol
import dev.pairly.core.ffi.ScreenInputData
import dev.pairly.core.ffi.ScreenKeyData
import java.net.InetAddress
import java.net.InetSocketAddress
import java.net.Socket
import java.nio.ByteBuffer

/**
 * Sharing the screen through the screen helper ([dev.pairly.android.helper.Helper]) when it
 * runs: no "Start recording?" prompt, and real touches and keys instead of the accessibility
 * service's gestures. Without the helper, [PhoneScreen] asks as before.
 *
 * Pairly's own notification shows while the screen is shared (Android shows its recording
 * indicator only for the prompted kind), with Stop.
 */
object ShellScreen {
    /** The PC the screen is shared with through the helper, while sharing. */
    @Volatile
    var device: String? = null
        private set

    @Volatile
    private var wire: HelperProtocol.Wire? = null

    /** Share with `deviceId` through the helper; false when it isn't running. */
    fun start(context: Context, deviceId: String, deviceName: String): Boolean {
        val wire = connect() ?: return false
        val internet = Pairly.screenOverInternet(deviceId)
        val (side, bits, fps) = if (internet) {
            Triple(ScreenShareService.INTERNET_SIDE, ScreenShareService.INTERNET_BIT_RATE, ScreenShareService.INTERNET_FRAME_RATE)
        } else {
            Triple(ScreenShareService.MAX_SIDE, ScreenShareService.BIT_RATE, ScreenShareService.FRAME_RATE)
        }
        this.wire?.close()
        this.wire = wire
        device = deviceId
        wire.send(HelperProtocol.START, ByteBuffer.allocate(12).putInt(side).putInt(bits).putInt(fps).array())
        showOngoing(context.applicationContext, deviceName)
        Log.i(PhoneScreen.TAG, "sharing the screen through the helper${if (internet) " over the internet" else ""}")
        Thread({ forward(context.applicationContext, deviceId, wire) }, "pairly-shell-screen").start()
        return true
    }

    /** The helper's video to the PC, until either side stops. */
    private fun forward(context: Context, deviceId: String, wire: HelperProtocol.Wire) {
        var reason = context.getString(R.string.screen_stopped_phone)
        while (true) {
            val (kind, body) = wire.read() ?: break
            when (kind) {
                HelperProtocol.STARTED -> {
                    val b = ByteBuffer.wrap(body)
                    Pairly.screenStarted(deviceId, b.int, b.int)
                }
                HelperProtocol.FRAME -> if (body.isNotEmpty()) {
                    val flags = body[0].toInt()
                    val behind = Pairly.screenFrame(deviceId, body.copyOfRange(1, body.size), flags and 1 != 0, flags and 2 != 0)
                    if (behind) runCatching { wire.send(HelperProtocol.KEY_FRAME) }
                }
                HelperProtocol.STOPPED -> {
                    reason = String(body, Charsets.UTF_8)
                    break
                }
            }
        }
        // Only if this is still the current session (a newer one may have replaced it).
        if (this.wire === wire) end(context, reason)
    }

    /** Stop sharing (from the PC or the notification). */
    fun stop(context: Context) {
        runCatching { wire?.send(HelperProtocol.STOP) }
        end(context, context.getString(R.string.screen_stopped_phone))
    }

    @Synchronized
    private fun end(context: Context, reason: String) {
        val target = device ?: return
        device = null
        wire?.close()
        wire = null
        NotificationManagerCompat.from(context).cancel(ONGOING_ID)
        Pairly.screenStopped(target, reason)
    }

    /** The PC's input, performed by the helper. False when it can't (the caller falls back). */
    fun input(input: ScreenInputData): Boolean {
        val wire = wire ?: return false
        fun xy(kind: Int, x: Float, y: Float) = wire.send(kind, ByteBuffer.allocate(8).putFloat(x).putFloat(y).array())
        runCatching {
            when (input) {
                is ScreenInputData.Tap -> xy(HelperProtocol.TAP, input.x, input.y)
                is ScreenInputData.LongPress -> xy(HelperProtocol.LONG_PRESS, input.x, input.y)
                is ScreenInputData.Swipe -> swipe(wire, input.points.map { it.x to it.y }, input.durationMs.toInt())
                // Scrolling: a quick swipe through the middle of the screen.
                is ScreenInputData.Scroll -> swipe(
                    wire,
                    listOf(0.5f to 0.5f, (0.5f + input.dx).coerceIn(0f, 1f) to (0.5f + input.dy).coerceIn(0f, 1f)),
                    SCROLL_MS,
                )
                is ScreenInputData.Key -> wire.send(HelperProtocol.KEY, ByteBuffer.allocate(4).putInt(keyCode(input.key)).array())
                is ScreenInputData.Text -> {
                    // Key presses type only what a keyboard has: anything else (other scripts,
                    // emoji) goes through the accessibility service if it's on.
                    if (!input.text.all { it == '\n' || it.code in 32..126 }) {
                        return PairlyAccessibility.instance?.let { it.screenInput(input); true } ?: true
                    }
                    wire.send(HelperProtocol.TEXT, input.text.toByteArray(Charsets.UTF_8))
                }
            }
        }.onFailure { Log.w(PhoneScreen.TAG, "helper input failed", it) }
        return true
    }

    private fun swipe(wire: HelperProtocol.Wire, points: List<Pair<Float, Float>>, durationMs: Int) {
        val b = ByteBuffer.allocate(6 + points.size * 8).putInt(durationMs).putShort(points.size.toShort())
        points.forEach { (x, y) -> b.putFloat(x).putFloat(y) }
        wire.send(HelperProtocol.SWIPE, b.array())
    }

    private fun keyCode(key: ScreenKeyData): Int = when (key) {
        ScreenKeyData.BACK -> KeyEvent.KEYCODE_BACK
        ScreenKeyData.HOME -> KeyEvent.KEYCODE_HOME
        ScreenKeyData.RECENTS -> KeyEvent.KEYCODE_APP_SWITCH
        ScreenKeyData.ENTER -> KeyEvent.KEYCODE_ENTER
        ScreenKeyData.BACKSPACE -> KeyEvent.KEYCODE_DEL
        ScreenKeyData.DELETE -> KeyEvent.KEYCODE_FORWARD_DEL
        ScreenKeyData.LEFT -> KeyEvent.KEYCODE_DPAD_LEFT
        ScreenKeyData.RIGHT -> KeyEvent.KEYCODE_DPAD_RIGHT
        ScreenKeyData.UP -> KeyEvent.KEYCODE_DPAD_UP
        ScreenKeyData.DOWN -> KeyEvent.KEYCODE_DPAD_DOWN
    }

    /** The helper, if it runs and speaks our version. */
    private fun connect(): HelperProtocol.Wire? = runCatching {
        val socket = Socket()
        socket.connect(InetSocketAddress(InetAddress.getLoopbackAddress(), HelperProtocol.PORT), 300)
        socket.soTimeout = 2_000
        val wire = HelperProtocol.Wire(socket)
        val (kind, body) = wire.read() ?: return@runCatching null
        if (kind != HelperProtocol.HELLO || ByteBuffer.wrap(body).int != HelperProtocol.VERSION) {
            Log.w(PhoneScreen.TAG, "the screen helper is from another Pairly version: restart it")
            wire.close()
            return@runCatching null
        }
        socket.soTimeout = 0
        wire
    }.getOrNull()

    private fun showOngoing(context: Context, name: String) {
        val stop = PendingIntent.getBroadcast(
            context,
            0,
            Intent(context, StopReceiver::class.java),
            PendingIntent.FLAG_IMMUTABLE,
        )
        val notification = NotificationCompat.Builder(context, Notifications.CHANNEL_SCREEN)
            .setSmallIcon(R.drawable.ic_notification)
            .setContentTitle(context.getString(R.string.screen_sharing_title, name))
            .setContentText(context.getString(R.string.screen_sharing_body))
            .setOngoing(true)
            .addAction(0, context.getString(R.string.screen_stop), stop)
            .build()
        runCatching { NotificationManagerCompat.from(context).notify(ONGOING_ID, notification) }
    }

    /** The notification's Stop. */
    class StopReceiver : BroadcastReceiver() {
        override fun onReceive(context: Context, intent: Intent) = stop(context)
    }

    private const val ONGOING_ID = 46
    private const val SCROLL_MS = 250
}
