package dev.pairly.android.device

import android.Manifest
import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.media.AudioAttributes
import android.media.AudioManager
import android.media.MediaPlayer
import android.media.RingtoneManager
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.os.VibrationEffect
import android.os.Vibrator
import android.os.VibratorManager
import android.util.Log
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import androidx.core.content.ContextCompat
import dev.pairly.android.Notifications
import dev.pairly.android.R

/**
 * Find my phone: loops the alarm sound on the alarm stream at full volume (so it rings even
 * on silent), vibrates, and shows a notification with Stop. Gives up after a minute.
 */
object Ringer {
    private const val TAG = "PairlyRinger"
    private const val NOTIFICATION_ID = 3
    private const val MAX_RING_MS = 60_000L

    private val main = Handler(Looper.getMainLooper())
    private var player: MediaPlayer? = null
    private var savedVolume: Int? = null
    private val autoStop = Runnable { stopNow() }
    private lateinit var appContext: Context

    fun start(context: Context, from: String) {
        main.post { startNow(context, from) }
    }

    fun stop() {
        main.post { stopNow() }
    }

    private fun startNow(context: Context, from: String) {
        appContext = context.applicationContext
        if (player != null) return
        val audio = appContext.getSystemService(AudioManager::class.java)
        if (audio != null) {
            savedVolume = audio.getStreamVolume(AudioManager.STREAM_ALARM)
            audio.setStreamVolume(AudioManager.STREAM_ALARM, audio.getStreamMaxVolume(AudioManager.STREAM_ALARM), 0)
        }
        val uri = RingtoneManager.getDefaultUri(RingtoneManager.TYPE_ALARM)
            ?: RingtoneManager.getDefaultUri(RingtoneManager.TYPE_RINGTONE)
        player = runCatching {
            MediaPlayer().apply {
                setAudioAttributes(
                    AudioAttributes.Builder()
                        .setUsage(AudioAttributes.USAGE_ALARM)
                        .setContentType(AudioAttributes.CONTENT_TYPE_SONIFICATION)
                        .build(),
                )
                setDataSource(appContext, uri)
                isLooping = true
                prepare()
                start()
            }
        }.onFailure { Log.w(TAG, "can't play the ring sound", it) }.getOrNull()
        vibrator()?.vibrate(VibrationEffect.createWaveform(longArrayOf(0, 800, 400), 0))
        showNotification(from)
        main.postDelayed(autoStop, MAX_RING_MS)
    }

    private fun stopNow() {
        main.removeCallbacks(autoStop)
        player?.runCatching { stop(); release() }
        player = null
        if (!::appContext.isInitialized) return
        vibrator()?.cancel()
        val audio = appContext.getSystemService(AudioManager::class.java)
        savedVolume?.let { audio?.setStreamVolume(AudioManager.STREAM_ALARM, it, 0) }
        savedVolume = null
        NotificationManagerCompat.from(appContext).cancel(NOTIFICATION_ID)
    }

    private fun vibrator(): Vibrator? = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
        appContext.getSystemService(VibratorManager::class.java)?.defaultVibrator
    } else {
        @Suppress("DEPRECATION")
        appContext.getSystemService(Vibrator::class.java)
    }

    private fun showNotification(from: String) {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU &&
            ContextCompat.checkSelfPermission(appContext, Manifest.permission.POST_NOTIFICATIONS) !=
            PackageManager.PERMISSION_GRANTED
        ) {
            return
        }
        val stop = PendingIntent.getBroadcast(
            appContext,
            0,
            Intent(appContext, RingStopReceiver::class.java),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        val notification = NotificationCompat.Builder(appContext, Notifications.CHANNEL_FINDMY)
            .setSmallIcon(R.drawable.ic_notification)
            .setContentTitle(appContext.getString(R.string.ring_title))
            .setContentText(appContext.getString(R.string.ring_body, from))
            .setCategory(NotificationCompat.CATEGORY_ALARM)
            .setPriority(NotificationCompat.PRIORITY_MAX)
            .setOngoing(true)
            .setContentIntent(stop)
            .addAction(0, appContext.getString(R.string.ring_stop), stop)
            .build()
        NotificationManagerCompat.from(appContext).notify(NOTIFICATION_ID, notification)
    }
}

class RingStopReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) = Ringer.stop()
}
