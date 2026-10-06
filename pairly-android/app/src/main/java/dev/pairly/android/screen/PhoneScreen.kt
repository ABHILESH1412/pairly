package dev.pairly.android.screen

import android.Manifest
import android.app.Activity
import android.app.Notification
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.content.pm.ServiceInfo
import android.hardware.display.DisplayManager
import android.hardware.display.VirtualDisplay
import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaFormat
import android.media.projection.MediaProjection
import android.media.projection.MediaProjectionConfig
import android.media.projection.MediaProjectionManager
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.HandlerThread
import android.os.IBinder
import android.util.Log
import android.view.WindowManager
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import androidx.core.app.ServiceCompat
import androidx.core.content.ContextCompat
import androidx.core.content.IntentCompat
import dev.pairly.android.Notifications
import dev.pairly.android.Pairly
import dev.pairly.android.R
import dev.pairly.android.device.PairlyAccessibility
import dev.pairly.core.ffi.PairlyException
import dev.pairly.core.ffi.ScreenHandler
import dev.pairly.core.ffi.ScreenInputData

/**
 * Shows this phone's screen to a paired PC that asks (same Wi-Fi only), and lets it control
 * the phone: the PC's taps, swipes and keys are performed by Pairly's accessibility service.
 *
 * Android asks the user's permission ("Start recording?") every time; [ScreenConsentActivity]
 * shows that, then [ScreenShareService] records the screen into the hardware H.264 encoder
 * and sends each encoded frame to the PC.
 */
class PhoneScreen(context: Context) : ScreenHandler {
    private val context = context.applicationContext

    override fun startSharing(deviceId: String, deviceName: String) {
        if (ScreenShareService.device != null && ScreenShareService.device != deviceId) {
            throw PairlyException.Failed("the screen is already shared with another device")
        }
        val consent = Intent(context, ScreenConsentActivity::class.java)
            .putExtra(EXTRA_DEVICE, deviceId)
            .putExtra(EXTRA_NAME, deviceName)
            .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        // Pairly's accessibility service may open screens from the background; otherwise a
        // notification asks.
        val opened = PairlyAccessibility.instance?.let { runCatching { it.startActivity(consent) }.isSuccess } == true
        if (!opened) askWithNotification(consent, deviceName)
    }

    override fun stopSharing(deviceId: String) {
        if (ScreenShareService.device == deviceId) ScreenShareService.stop(context)
    }

    override fun input(deviceId: String, input: ScreenInputData) {
        if (ScreenShareService.device != deviceId) return
        val service = PairlyAccessibility.instance
        if (service == null) {
            Log.i(TAG, "screen input ignored: the accessibility service is off")
            return
        }
        service.screenInput(input)
    }

    override fun disconnected(deviceId: String) = stopSharing(deviceId)

    // Watching a PC's screen: hand its stream to the open viewer.
    override fun viewerStarted(deviceId: String, width: UInt, height: UInt) =
        PcScreenSession.deliver(deviceId, PcScreenSession.Event.Started(width.toInt(), height.toInt()))

    override fun viewerFrame(deviceId: String, data: ByteArray, key: Boolean) =
        PcScreenSession.deliver(deviceId, PcScreenSession.Event.Frame(data, key))

    override fun viewerStopped(deviceId: String, reason: String) =
        PcScreenSession.deliver(deviceId, PcScreenSession.Event.Stopped(reason))

    private fun askWithNotification(consent: Intent, deviceName: String) {
        val tap = PendingIntent.getActivity(
            context,
            REQUEST_CONSENT,
            consent,
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        val notification = NotificationCompat.Builder(context, Notifications.CHANNEL_SCREEN)
            .setSmallIcon(R.drawable.ic_notification)
            .setContentTitle(context.getString(R.string.screen_ask_title, deviceName))
            .setContentText(context.getString(R.string.screen_ask_body))
            .setContentIntent(tap)
            .setAutoCancel(true)
            .setPriority(NotificationCompat.PRIORITY_HIGH)
            .build()
        val allowed = Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU ||
            ContextCompat.checkSelfPermission(context, Manifest.permission.POST_NOTIFICATIONS) == PackageManager.PERMISSION_GRANTED
        if (allowed) {
            NotificationManagerCompat.from(context).notify(ASK_ID, notification)
        } else {
            throw PairlyException.Failed("open Pairly on the phone to share its screen")
        }
    }

    companion object {
        const val TAG = "PhoneScreen"
        const val EXTRA_DEVICE = "device"
        const val EXTRA_NAME = "name"
        const val ASK_ID = 40
        private const val REQUEST_CONSENT = 41
    }
}

/** Shows Android's "Start recording?" prompt, then hands the permission to the service. */
class ScreenConsentActivity : Activity() {
    private var device: String? = null
    private var name: String? = null

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        device = intent.getStringExtra(PhoneScreen.EXTRA_DEVICE)
        name = intent.getStringExtra(PhoneScreen.EXTRA_NAME)
        NotificationManagerCompat.from(this).cancel(PhoneScreen.ASK_ID)
        if (device == null) {
            finish()
            return
        }
        if (savedInstanceState == null) {
            val projection = getSystemService(MediaProjectionManager::class.java)
            // Android 14+: ask for the whole screen directly, so the prompt is a plain
            // "Cancel / Start" with no "single app or entire screen" picker or app list.
            val ask = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
                projection.createScreenCaptureIntent(MediaProjectionConfig.createConfigForDefaultDisplay())
            } else {
                projection.createScreenCaptureIntent()
            }
            @Suppress("DEPRECATION") // the result API needs a ComponentActivity
            startActivityForResult(ask, REQUEST_RECORD)
        }
    }

    @Deprecated("Deprecated in Java")
    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        @Suppress("DEPRECATION")
        super.onActivityResult(requestCode, resultCode, data)
        val target = device
        if (requestCode == REQUEST_RECORD && target != null) {
            if (resultCode == RESULT_OK && data != null) {
                ScreenShareService.start(this, target, name.orEmpty(), resultCode, data)
            } else {
                Pairly.screenStopped(target, getString(R.string.screen_declined))
            }
        }
        finish()
    }

    private companion object {
        const val REQUEST_RECORD = 1
    }
}

/**
 * Records the screen into the hardware H.264 encoder and sends each encoded frame to the PC.
 * A foreground service of type "mediaProjection", as Android requires while recording.
 */
class ScreenShareService : Service() {
    private var projection: MediaProjection? = null
    private var display: VirtualDisplay? = null
    private var encoder: MediaCodec? = null
    private var thread: HandlerThread? = null
    private var stopReason: String? = null

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent?.action == ACTION_STOP) {
            stopReason = getString(R.string.screen_stopped_phone)
            stopSelf()
            return START_NOT_STICKY
        }
        val target = intent?.getStringExtra(PhoneScreen.EXTRA_DEVICE)
        val data = intent?.let { IntentCompat.getParcelableExtra(it, EXTRA_DATA, Intent::class.java) }
        if (target == null || data == null) {
            stopSelf()
            return START_NOT_STICKY
        }
        val name = intent.getStringExtra(PhoneScreen.EXTRA_NAME).orEmpty()
        // Android requires the foreground notification before the recording starts.
        ServiceCompat.startForeground(
            this,
            ONGOING_ID,
            ongoing(name),
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION else 0,
        )
        device = target
        runCatching { startRecording(target, intent.getIntExtra(EXTRA_RESULT, 0), data) }
            .onFailure {
                Log.w(PhoneScreen.TAG, "screen recording failed", it)
                stopReason = getString(R.string.screen_failed, it.message ?: it.javaClass.simpleName)
                stopSelf()
            }
        return START_NOT_STICKY
    }

    private fun startRecording(target: String, resultCode: Int, data: Intent) {
        val manager = getSystemService(MediaProjectionManager::class.java)
        val mp = manager.getMediaProjection(resultCode, data) ?: error("no permission to record")
        projection = mp
        val worker = HandlerThread("pairly-screen").apply { start() }
        thread = worker
        val handler = Handler(worker.looper)
        mp.registerCallback(
            object : MediaProjection.Callback() {
                override fun onStop() {
                    stopReason = stopReason ?: getString(R.string.screen_stopped_phone)
                    stopSelf()
                }
            },
            handler,
        )

        // The whole screen, scaled so its longer side is at most MAX_SIDE (even sizes).
        val (sw, sh) = screenSize()
        val (w, h) = scaled(sw, sh)
        val format = MediaFormat.createVideoFormat(MediaFormat.MIMETYPE_VIDEO_AVC, w, h).apply {
            setInteger(MediaFormat.KEY_COLOR_FORMAT, MediaCodecInfo.CodecCapabilities.COLOR_FormatSurface)
            setInteger(MediaFormat.KEY_BIT_RATE, BIT_RATE)
            setInteger(MediaFormat.KEY_FRAME_RATE, FRAME_RATE)
            setInteger(MediaFormat.KEY_I_FRAME_INTERVAL, 1)
            // Repeat the last frame when nothing changes, so a still screen keeps flowing.
            setLong(MediaFormat.KEY_REPEAT_PREVIOUS_FRAME_AFTER, 200_000L)
        }
        val codec = MediaCodec.createEncoderByType(MediaFormat.MIMETYPE_VIDEO_AVC)
        encoder = codec
        codec.setCallback(
            object : MediaCodec.Callback() {
                override fun onInputBufferAvailable(codec: MediaCodec, index: Int) {}

                override fun onOutputBufferAvailable(codec: MediaCodec, index: Int, info: MediaCodec.BufferInfo) {
                    val buffer = runCatching { codec.getOutputBuffer(index) }.getOrNull()
                    if (buffer != null && info.size > 0) {
                        val bytes = ByteArray(info.size)
                        buffer.position(info.offset)
                        buffer.get(bytes, 0, info.size)
                        val config = info.flags and MediaCodec.BUFFER_FLAG_CODEC_CONFIG != 0
                        val key = info.flags and MediaCodec.BUFFER_FLAG_KEY_FRAME != 0
                        if (Pairly.screenFrame(target, bytes, key, config)) {
                            runCatching {
                                codec.setParameters(
                                    Bundle().apply { putInt(MediaCodec.PARAMETER_KEY_REQUEST_SYNC_FRAME, 0) },
                                )
                            }
                        }
                    }
                    runCatching { codec.releaseOutputBuffer(index, false) }
                }

                override fun onError(codec: MediaCodec, e: MediaCodec.CodecException) {
                    Log.w(PhoneScreen.TAG, "encoder error", e)
                    stopReason = getString(R.string.screen_failed, e.diagnosticInfo)
                    stopSelf()
                }

                override fun onOutputFormatChanged(codec: MediaCodec, format: MediaFormat) {}
            },
            handler,
        )
        codec.configure(format, null, null, MediaCodec.CONFIGURE_FLAG_ENCODE)
        val surface = codec.createInputSurface()
        codec.start()
        display = mp.createVirtualDisplay(
            "Pairly",
            w,
            h,
            resources.displayMetrics.densityDpi,
            DisplayManager.VIRTUAL_DISPLAY_FLAG_AUTO_MIRROR,
            surface,
            null,
            handler,
        )
        Pairly.screenStarted(target, w, h)
        Log.i(PhoneScreen.TAG, "sharing the screen at ${w}x$h")
    }

    /** The whole display in real pixels. */
    private fun screenSize(): Pair<Int, Int> =
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            val b = getSystemService(WindowManager::class.java).maximumWindowMetrics.bounds
            b.width() to b.height()
        } else {
            val m = android.util.DisplayMetrics()
            @Suppress("DEPRECATION")
            getSystemService(WindowManager::class.java).defaultDisplay.getRealMetrics(m)
            m.widthPixels to m.heightPixels
        }

    private fun ongoing(name: String): Notification {
        val stop = PendingIntent.getService(
            this,
            0,
            Intent(this, ScreenShareService::class.java).setAction(ACTION_STOP),
            PendingIntent.FLAG_IMMUTABLE,
        )
        return NotificationCompat.Builder(this, Notifications.CHANNEL_SCREEN)
            .setSmallIcon(R.drawable.ic_notification)
            .setContentTitle(getString(R.string.screen_sharing_title, name))
            .setContentText(getString(R.string.screen_sharing_body))
            .setOngoing(true)
            .addAction(0, getString(R.string.screen_stop), stop)
            .build()
    }

    override fun onDestroy() {
        runCatching { display?.release() }
        runCatching { encoder?.stop() }
        runCatching { encoder?.release() }
        runCatching { projection?.stop() }
        thread?.quitSafely()
        device?.let { Pairly.screenStopped(it, stopReason ?: getString(R.string.screen_stopped_phone)) }
        device = null
        super.onDestroy()
    }

    companion object {
        /** The PC the screen is shared with, while sharing. */
        @Volatile
        var device: String? = null
            private set

        private const val EXTRA_RESULT = "result"
        private const val EXTRA_DATA = "data"
        private const val ACTION_STOP = "dev.pairly.android.screen.STOP"
        private const val ONGOING_ID = 42
        private const val MAX_SIDE = 1600
        private const val BIT_RATE = 6_000_000
        private const val FRAME_RATE = 30

        fun start(context: Context, device: String, name: String, resultCode: Int, data: Intent) {
            context.startForegroundService(
                Intent(context, ScreenShareService::class.java)
                    .putExtra(PhoneScreen.EXTRA_DEVICE, device)
                    .putExtra(PhoneScreen.EXTRA_NAME, name)
                    .putExtra(EXTRA_RESULT, resultCode)
                    .putExtra(EXTRA_DATA, data),
            )
        }

        fun stop(context: Context) {
            context.stopService(Intent(context, ScreenShareService::class.java))
        }

        /** Fit the longer side within [MAX_SIDE], keeping the shape, with even sizes. */
        fun scaled(width: Int, height: Int): Pair<Int, Int> {
            val scale = minOf(1f, MAX_SIDE.toFloat() / maxOf(width, height))
            fun even(v: Float) = (v.toInt() / 2) * 2
            return even(width * scale) to even(height * scale)
        }
    }
}
