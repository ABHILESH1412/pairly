package dev.pairly.android.notifications

import android.Manifest
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import androidx.core.app.RemoteInput
import androidx.core.content.ContextCompat
import androidx.core.graphics.createBitmap
import dev.pairly.android.Notifications
import dev.pairly.android.Pairly
import dev.pairly.android.R
import dev.pairly.core.ffi.NotificationData
import dev.pairly.core.ffi.NotificationHandler
import java.nio.ByteBuffer

/**
 * Shows notifications mirrored from paired devices and does what they ask with the phone's own
 * ones. Mirrors are tagged `device|id`, so they can be updated and removed individually.
 */
class MirroredNotifications(context: Context) : NotificationHandler {
    private val context = context.applicationContext
    private val manager = NotificationManagerCompat.from(this.context)

    override fun show(fromId: String, fromName: String, notification: NotificationData) {
        val tag = tag(fromId, notification.id)
        val builder = NotificationCompat.Builder(context, Notifications.CHANNEL_MIRRORS)
            .setSmallIcon(R.drawable.ic_notification)
            .setContentTitle(notification.title)
            .setContentText(notification.text)
            .setStyle(NotificationCompat.BigTextStyle().bigText(notification.text))
            .setSubText("${notification.app} · $fromName")
            .setWhen(notification.timeMs.toLong())
            .setShowWhen(true)
            .setOnlyAlertOnce(true)
            .setSilent(notification.silent)
            .setAutoCancel(true)
            .setDeleteIntent(broadcast(ACTION_DISMISS, fromId, notification.id, tag.hashCode()))
        notification.icon?.let { icon ->
            val bitmap = createBitmap(icon.width.toInt(), icon.height.toInt())
            bitmap.copyPixelsFromBuffer(ByteBuffer.wrap(icon.rgba))
            builder.setLargeIcon(bitmap)
        }
        notification.actions.forEachIndexed { i, action ->
            val intent = broadcast(ACTION_ACTION, fromId, notification.id, tag.hashCode() * 31 + i) { putExtra(EXTRA_KEY, action.key) }
            builder.addAction(0, action.label, intent)
        }
        if (notification.canReply) {
            val input = RemoteInput.Builder(EXTRA_REPLY).setLabel(context.getString(R.string.mirror_reply)).build()
            val intent = broadcast(ACTION_REPLY, fromId, notification.id, tag.hashCode() * 31 + 7, mutable = true)
            builder.addAction(
                NotificationCompat.Action.Builder(0, context.getString(R.string.mirror_reply), intent)
                    .addRemoteInput(input)
                    .setAllowGeneratedReplies(false)
                    .build(),
            )
        }
        post(tag, builder)
    }

    override fun remove(fromId: String, id: String) {
        manager.cancel(tag(fromId, id), NOTIFICATION_ID)
    }

    override fun sync(fromId: String, active: List<String>) {
        val keep = active.map { tag(fromId, it) }.toSet()
        val system = context.getSystemService(NotificationManager::class.java) ?: return
        system.activeNotifications
            .filter { it.id == NOTIFICATION_ID && it.tag?.startsWith("$fromId|") == true && it.tag !in keep }
            .forEach { manager.cancel(it.tag, NOTIFICATION_ID) }
    }

    override fun dismissLocal(id: String) = PhoneNotifications.dismiss(id)

    override fun actionLocal(id: String, action: String) = PhoneNotifications.action(context, id, action)

    override fun replyLocal(id: String, text: String) = PhoneNotifications.reply(context, id, text)

    private fun post(tag: String, builder: NotificationCompat.Builder) {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU &&
            ContextCompat.checkSelfPermission(context, Manifest.permission.POST_NOTIFICATIONS) !=
            PackageManager.PERMISSION_GRANTED
        ) {
            return
        }
        manager.notify(tag, NOTIFICATION_ID, builder.build())
    }

    private fun broadcast(
        action: String,
        device: String,
        id: String,
        requestCode: Int,
        mutable: Boolean = false,
        extras: Intent.() -> Unit = {},
    ): PendingIntent {
        val intent = Intent(context, MirrorActionReceiver::class.java)
            .setAction(action)
            .putExtra(EXTRA_DEVICE, device)
            .putExtra(EXTRA_ID, id)
            .apply(extras)
        // Replies need a mutable intent so the system can attach the typed text.
        val flags = PendingIntent.FLAG_UPDATE_CURRENT or
            if (mutable) PendingIntent.FLAG_MUTABLE else PendingIntent.FLAG_IMMUTABLE
        return PendingIntent.getBroadcast(context, requestCode, intent, flags)
    }

    companion object {
        const val NOTIFICATION_ID = 2
        const val ACTION_DISMISS = "dev.pairly.android.MIRROR_DISMISSED"
        const val ACTION_ACTION = "dev.pairly.android.MIRROR_ACTION"
        const val ACTION_REPLY = "dev.pairly.android.MIRROR_REPLY"
        const val EXTRA_DEVICE = "device"
        const val EXTRA_ID = "id"
        const val EXTRA_KEY = "key"
        const val EXTRA_REPLY = "reply"

        fun tag(device: String, id: String) = "$device|$id"
    }
}

/** Taps, swipes and replies on mirrored notifications, sent back to the source device. */
class MirrorActionReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        val device = intent.getStringExtra(MirroredNotifications.EXTRA_DEVICE) ?: return
        val id = intent.getStringExtra(MirroredNotifications.EXTRA_ID) ?: return
        when (intent.action) {
            MirroredNotifications.ACTION_DISMISS -> Pairly.mirrorDismissed(device, id)
            MirroredNotifications.ACTION_ACTION -> {
                val key = intent.getStringExtra(MirroredNotifications.EXTRA_KEY) ?: return
                Pairly.mirrorAction(device, id, key)
            }
            MirroredNotifications.ACTION_REPLY -> {
                val text = RemoteInput.getResultsFromIntent(intent)
                    ?.getCharSequence(MirroredNotifications.EXTRA_REPLY)?.toString() ?: return
                Pairly.mirrorReply(device, id, text)
                // Replace the "sending" spinner the system shows after an inline reply.
                NotificationManagerCompat.from(context).cancel(MirroredNotifications.tag(device, id), MirroredNotifications.NOTIFICATION_ID)
            }
        }
    }
}
