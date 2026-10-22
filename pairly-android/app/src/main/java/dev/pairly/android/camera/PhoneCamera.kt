package dev.pairly.android.camera

import android.Manifest
import android.annotation.SuppressLint
import android.app.Activity
import android.app.Notification
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.content.pm.ServiceInfo
import android.hardware.camera2.CameraCaptureSession
import android.hardware.camera2.CameraCharacteristics
import android.hardware.camera2.CameraDevice
import android.hardware.camera2.CameraManager
import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaFormat
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.HandlerThread
import android.os.IBinder
import android.util.Log
import android.view.Surface
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import androidx.core.app.ServiceCompat
import androidx.core.content.ContextCompat
import dev.pairly.android.Notifications
import dev.pairly.android.Pairly
import dev.pairly.android.R
import dev.pairly.android.device.PairlyAccessibility
import dev.pairly.core.ffi.CameraHandler
import dev.pairly.core.ffi.PairlyException

/**
 * The phone's camera as a PC's webcam. The PC asks; the phone's user agrees (a notification,
 * or straight away when Pairly's accessibility service can open the screen); then a camera
 * service films and streams H.264 until either side stops it.
 */
class PhoneCamera(context: Context) : CameraHandler {
    private val context = context.applicationContext

    override fun startCamera(deviceId: String, deviceName: String, back: Boolean) {
        if (ContextCompat.checkSelfPermission(context, Manifest.permission.CAMERA) != PackageManager.PERMISSION_GRANTED) {
            throw PairlyException.Failed(context.getString(R.string.camera_no_permission))
        }
        if (CameraShareService.device != null && CameraShareService.device != deviceId) {
            throw PairlyException.Failed(context.getString(R.string.camera_busy))
        }
        val consent = Intent(context, CameraConsentActivity::class.java)
            .putExtra(EXTRA_DEVICE, deviceId)
            .putExtra(EXTRA_NAME, deviceName)
            .putExtra(EXTRA_BACK, back)
            .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        val opened = PairlyAccessibility.instance?.let { runCatching { it.startActivity(consent) }.isSuccess } == true
        if (!opened) ask(consent, deviceName)
    }

    override fun stopCamera(deviceId: String) {
        if (CameraShareService.device == deviceId) CameraShareService.stop(context)
    }

    /** Android starts a camera service only from the screen: a notification to tap. */
    private fun ask(consent: Intent, deviceName: String) {
        val tap = PendingIntent.getActivity(
            context,
            REQUEST_CONSENT,
            consent,
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        val notification = NotificationCompat.Builder(context, Notifications.CHANNEL_SCREEN)
            .setSmallIcon(R.drawable.ic_notification)
            .setContentTitle(context.getString(R.string.camera_ask_title, deviceName))
            .setContentText(context.getString(R.string.camera_ask_body))
            .setContentIntent(tap)
            .setAutoCancel(true)
            .setPriority(NotificationCompat.PRIORITY_HIGH)
            .build()
        val allowed = Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU ||
            ContextCompat.checkSelfPermission(context, Manifest.permission.POST_NOTIFICATIONS) == PackageManager.PERMISSION_GRANTED
        if (!allowed) throw PairlyException.Failed(context.getString(R.string.camera_open_pairly))
        NotificationManagerCompat.from(context).notify(ASK_ID, notification)
    }

    companion object {
        const val TAG = "PhoneCamera"
        const val EXTRA_DEVICE = "device"
        const val EXTRA_NAME = "name"
        const val EXTRA_BACK = "back"
        const val ASK_ID = 43
        private const val REQUEST_CONSENT = 44
    }
}

/** Opened from the request: starts the camera service from the screen, then goes away. */
class CameraConsentActivity : Activity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        NotificationManagerCompat.from(this).cancel(PhoneCamera.ASK_ID)
        val device = intent.getStringExtra(PhoneCamera.EXTRA_DEVICE)
        if (device != null && savedInstanceState == null) {
            CameraShareService.start(
                this,
                device,
                intent.getStringExtra(PhoneCamera.EXTRA_NAME).orEmpty(),
                intent.getBooleanExtra(PhoneCamera.EXTRA_BACK, false),
            )
        }
        finish()
    }
}

/**
 * Films with the chosen camera into the hardware H.264 encoder and sends each frame to the PC.
 * A foreground service of type "camera", as Android requires while filming.
 */
class CameraShareService : Service() {
    private var camera: CameraDevice? = null
    private var encoder: MediaCodec? = null
    private var thread: HandlerThread? = null
    private var stopReason: String? = null

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent?.action == ACTION_STOP) {
            stopReason = getString(R.string.camera_stopped_phone)
            stopSelf()
            return START_NOT_STICKY
        }
        val target = intent?.getStringExtra(PhoneCamera.EXTRA_DEVICE)
        if (target == null || device != null) {
            if (device == null) stopSelf()
            return START_NOT_STICKY
        }
        val name = intent.getStringExtra(PhoneCamera.EXTRA_NAME).orEmpty()
        ServiceCompat.startForeground(
            this,
            ONGOING_ID,
            ongoing(name),
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) ServiceInfo.FOREGROUND_SERVICE_TYPE_CAMERA else 0,
        )
        device = target
        runCatching { startFilming(target, intent.getBooleanExtra(PhoneCamera.EXTRA_BACK, false)) }
            .onFailure { fail(it) }
        return START_NOT_STICKY
    }

    private fun fail(e: Throwable) {
        Log.w(PhoneCamera.TAG, "the camera failed", e)
        stopReason = getString(R.string.camera_failed, e.message ?: e.javaClass.simpleName)
        stopSelf()
    }

    @SuppressLint("MissingPermission") // checked before the service is started
    private fun startFilming(target: String, back: Boolean) {
        val worker = HandlerThread("pairly-camera").apply { start() }
        thread = worker
        val handler = Handler(worker.looper)

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
                        if (Pairly.cameraFrame(target, bytes, key, config)) {
                            runCatching {
                                codec.setParameters(Bundle().apply { putInt(MediaCodec.PARAMETER_KEY_REQUEST_SYNC_FRAME, 0) })
                            }
                        }
                    }
                    runCatching { codec.releaseOutputBuffer(index, false) }
                }

                override fun onError(codec: MediaCodec, e: MediaCodec.CodecException) = fail(e)

                override fun onOutputFormatChanged(codec: MediaCodec, format: MediaFormat) {}
            },
            handler,
        )
        val format = MediaFormat.createVideoFormat(MediaFormat.MIMETYPE_VIDEO_AVC, WIDTH, HEIGHT).apply {
            setInteger(MediaFormat.KEY_COLOR_FORMAT, MediaCodecInfo.CodecCapabilities.COLOR_FormatSurface)
            setInteger(MediaFormat.KEY_BIT_RATE, BIT_RATE)
            setInteger(MediaFormat.KEY_FRAME_RATE, FRAME_RATE)
            setInteger(MediaFormat.KEY_I_FRAME_INTERVAL, 1)
        }
        codec.configure(format, null, null, MediaCodec.CONFIGURE_FLAG_ENCODE)
        val surface = codec.createInputSurface()
        codec.start()

        val manager = getSystemService(CameraManager::class.java)
        val wanted = if (back) CameraCharacteristics.LENS_FACING_BACK else CameraCharacteristics.LENS_FACING_FRONT
        val id = manager.cameraIdList.firstOrNull {
            manager.getCameraCharacteristics(it).get(CameraCharacteristics.LENS_FACING) == wanted
        } ?: manager.cameraIdList.firstOrNull() ?: error(getString(R.string.camera_none))
        manager.openCamera(
            id,
            object : CameraDevice.StateCallback() {
                override fun onOpened(opened: CameraDevice) {
                    camera = opened
                    record(opened, surface, handler, target)
                }

                override fun onDisconnected(closed: CameraDevice) {
                    stopReason = stopReason ?: getString(R.string.camera_taken)
                    stopSelf()
                }

                override fun onError(failed: CameraDevice, error: Int) =
                    fail(IllegalStateException(getString(R.string.camera_error, error)))
            },
            handler,
        )
    }

    private fun record(opened: CameraDevice, surface: Surface, handler: Handler, target: String) {
        @Suppress("DEPRECATION") // SessionConfiguration needs Android 9; this works on 8 too
        opened.createCaptureSession(
            listOf(surface),
            object : CameraCaptureSession.StateCallback() {
                override fun onConfigured(session: CameraCaptureSession) {
                    runCatching {
                        val request = opened.createCaptureRequest(CameraDevice.TEMPLATE_RECORD).apply { addTarget(surface) }
                        session.setRepeatingRequest(request.build(), null, handler)
                        Pairly.cameraStarted(target, WIDTH, HEIGHT)
                        Log.i(PhoneCamera.TAG, "filming for the PC at ${WIDTH}x$HEIGHT")
                    }.onFailure { fail(it) }
                }

                override fun onConfigureFailed(session: CameraCaptureSession) =
                    fail(IllegalStateException(getString(R.string.camera_cant_film)))
            },
            handler,
        )
    }

    private fun ongoing(name: String): Notification {
        val stop = PendingIntent.getService(
            this,
            0,
            Intent(this, CameraShareService::class.java).setAction(ACTION_STOP),
            PendingIntent.FLAG_IMMUTABLE,
        )
        return NotificationCompat.Builder(this, Notifications.CHANNEL_SCREEN)
            .setSmallIcon(R.drawable.ic_notification)
            .setContentTitle(getString(R.string.camera_in_use_title, name))
            .setContentText(getString(R.string.camera_in_use_body))
            .setOngoing(true)
            .addAction(0, getString(R.string.camera_stop), stop)
            .build()
    }

    override fun onDestroy() {
        runCatching { camera?.close() }
        runCatching { encoder?.stop() }
        runCatching { encoder?.release() }
        thread?.quitSafely()
        device?.let { Pairly.cameraStopped(it, stopReason ?: getString(R.string.camera_stopped_phone)) }
        device = null
        super.onDestroy()
    }

    companion object {
        /** The PC the camera films for, while filming. */
        @Volatile
        var device: String? = null
            private set

        private const val ACTION_STOP = "dev.pairly.android.camera.STOP"
        private const val ONGOING_ID = 45
        private const val WIDTH = 1280
        private const val HEIGHT = 720
        private const val BIT_RATE = 4_000_000
        private const val FRAME_RATE = 30

        fun start(context: Context, device: String, name: String, back: Boolean) {
            val intent = Intent(context, CameraShareService::class.java)
                .putExtra(PhoneCamera.EXTRA_DEVICE, device)
                .putExtra(PhoneCamera.EXTRA_NAME, name)
                .putExtra(PhoneCamera.EXTRA_BACK, back)
            ContextCompat.startForegroundService(context, intent)
        }

        fun stop(context: Context) {
            context.stopService(Intent(context, CameraShareService::class.java))
        }
    }
}
