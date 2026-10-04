package dev.pairly.android

import android.Manifest
import android.app.Notification
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import androidx.core.app.NotificationChannelCompat
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import androidx.core.content.ContextCompat
import dev.pairly.android.device.ClipboardActivity
import java.util.concurrent.atomic.AtomicInteger

object Notifications {
    const val SERVICE_ID = 1
    const val CHANNEL_MIRRORS = "mirrors"
    const val CHANNEL_FINDMY = "findmy"
    /** Offers, received files, links and text. */
    const val CHANNEL_SHARED = "shared"
    /** A paired PC's media player. */
    const val CHANNEL_MEDIA = "media"
    /** Progress of file transfers. */
    const val CHANNEL_TRANSFERS = "transfers"
    private const val CHANNEL_SERVICE = "service"
    private const val CHANNEL_PINGS = "pings"
    private val nextId = AtomicInteger(100)

    fun createChannels(context: Context) {
        NotificationManagerCompat.from(context).createNotificationChannelsCompat(
            listOf(
                NotificationChannelCompat.Builder(CHANNEL_SERVICE, NotificationManagerCompat.IMPORTANCE_LOW)
                    .setName(context.getString(R.string.channel_service))
                    .setShowBadge(false)
                    .build(),
                NotificationChannelCompat.Builder(CHANNEL_PINGS, NotificationManagerCompat.IMPORTANCE_HIGH)
                    .setName(context.getString(R.string.channel_pings))
                    .build(),
                NotificationChannelCompat.Builder(CHANNEL_FINDMY, NotificationManagerCompat.IMPORTANCE_HIGH)
                    .setName(context.getString(R.string.channel_findmy))
                    .setSound(null, null)
                    .build(),
                NotificationChannelCompat.Builder(CHANNEL_SHARED, NotificationManagerCompat.IMPORTANCE_HIGH)
                    .setName(context.getString(R.string.channel_shared))
                    .build(),
                NotificationChannelCompat.Builder(CHANNEL_MEDIA, NotificationManagerCompat.IMPORTANCE_LOW)
                    .setName(context.getString(R.string.channel_media))
                    .setShowBadge(false)
                    .build(),
                NotificationChannelCompat.Builder(CHANNEL_TRANSFERS, NotificationManagerCompat.IMPORTANCE_LOW)
                    .setName(context.getString(R.string.channel_transfers))
                    .setShowBadge(false)
                    .build(),
                NotificationChannelCompat.Builder(CHANNEL_MIRRORS, NotificationManagerCompat.IMPORTANCE_HIGH)
                    .setName(context.getString(R.string.channel_mirrors))
                    .setDescription(context.getString(R.string.channel_mirrors_description))
                    .build(),
            ),
        )
    }

    /** The foreground service's ongoing notification. */
    fun service(context: Context, connected: List<String>): Notification {
        val title = if (connected.isEmpty()) {
            context.getString(R.string.service_waiting)
        } else {
            context.getString(R.string.service_connected, connected.joinToString())
        }
        return NotificationCompat.Builder(context, CHANNEL_SERVICE)
            .setSmallIcon(R.drawable.ic_notification)
            .setContentTitle(title)
            .setOngoing(true)
            .setSilent(true)
            .setContentIntent(openApp(context))
            .addAction(
                0,
                context.getString(R.string.action_send_clipboard),
                PendingIntent.getActivity(
                    context,
                    1,
                    Intent(context, ClipboardActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK),
                    PendingIntent.FLAG_IMMUTABLE,
                ),
            )
            .setForegroundServiceBehavior(NotificationCompat.FOREGROUND_SERVICE_IMMEDIATE)
            .build()
    }

    fun updateService(context: Context, connected: List<String>) {
        post(context, SERVICE_ID, service(context, connected))
    }

    fun ping(context: Context, from: String, message: String?) {
        val notification = NotificationCompat.Builder(context, CHANNEL_PINGS)
            .setSmallIcon(R.drawable.ic_notification)
            .setContentTitle(context.getString(R.string.ping_from, from))
            .setContentText(message)
            .setAutoCancel(true)
            .setContentIntent(openApp(context))
            .build()
        post(context, nextId.getAndIncrement(), notification)
    }

    /** Post if the user allowed notifications (Android 13+ asks at runtime). */
    fun post(context: Context, id: Int, notification: Notification) {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU &&
            ContextCompat.checkSelfPermission(context, Manifest.permission.POST_NOTIFICATIONS) !=
            PackageManager.PERMISSION_GRANTED
        ) {
            return
        }
        NotificationManagerCompat.from(context).notify(id, notification)
    }

    private fun openApp(context: Context): PendingIntent = PendingIntent.getActivity(
        context,
        0,
        Intent(context, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP),
        PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
    )
}
