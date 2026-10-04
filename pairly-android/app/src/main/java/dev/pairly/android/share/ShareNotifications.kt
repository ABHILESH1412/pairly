package dev.pairly.android.share

import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.text.format.Formatter
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import androidx.core.net.toUri
import dev.pairly.android.Notifications
import dev.pairly.android.Pairly
import dev.pairly.android.R
import dev.pairly.core.ffi.TransferData
import dev.pairly.core.ffi.TransferStatus
import java.util.concurrent.atomic.AtomicInteger

/** Notifications for offers, progress, results and received links/text. */
object ShareNotifications {
    private const val ACTION_ACCEPT = "dev.pairly.android.share.ACCEPT"
    private const val ACTION_CANCEL = "dev.pairly.android.share.CANCEL"
    private const val EXTRA_TRANSFER = "transfer"
    private val nextTextId = AtomicInteger(0x4000_0000)

    /** One notification per transfer, updated in place. */
    private fun idFor(transfer: ULong): Int = 0x5000_0000 or (transfer.toInt() and 0x0FFF_FFFF)

    private fun size(context: Context, bytes: ULong) = Formatter.formatShortFileSize(context, bytes.toLong())

    fun offer(context: Context, t: TransferData) {
        val n = NotificationCompat.Builder(context, Notifications.CHANNEL_SHARED)
            .setSmallIcon(R.drawable.ic_notification)
            .setContentTitle(context.getString(R.string.share_offer_title, t.deviceName))
            .setContentText(context.getString(R.string.share_offer_body, t.name, size(context, t.size)))
            .setOngoing(true)
            .setCategory(NotificationCompat.CATEGORY_RECOMMENDATION)
            .addAction(0, context.getString(R.string.share_decline), action(context, ACTION_CANCEL, t.id))
            .addAction(0, context.getString(R.string.share_accept), action(context, ACTION_ACCEPT, t.id))
            .build()
        Notifications.post(context, idFor(t.id), n)
    }

    fun progress(context: Context, t: TransferData) {
        val title = context.getString(
            if (t.incoming) R.string.share_receiving else R.string.share_sending,
            t.name,
        )
        val waiting = t.status == TransferStatus.Waiting
        val text = if (waiting) {
            context.getString(R.string.share_waiting, t.deviceName)
        } else {
            context.getString(R.string.share_progress, size(context, t.bytes), size(context, t.size), t.deviceName)
        }
        val max = 1000
        val done = if (t.size == 0uL) 0 else (t.bytes.toDouble() / t.size.toDouble() * max).toInt()
        val n = NotificationCompat.Builder(context, Notifications.CHANNEL_TRANSFERS)
            .setSmallIcon(if (t.incoming) android.R.drawable.stat_sys_download else android.R.drawable.stat_sys_upload)
            .setContentTitle(title)
            .setContentText(text)
            .setOngoing(true)
            .setOnlyAlertOnce(true)
            .setSilent(true)
            .setProgress(max, done, waiting)
            .addAction(0, context.getString(R.string.action_cancel), action(context, ACTION_CANCEL, t.id))
            .build()
        Notifications.post(context, idFor(t.id), n)
    }

    fun received(context: Context, t: TransferData, uri: Uri) {
        val view = Intent(Intent.ACTION_VIEW)
            .setDataAndType(Downloads.viewable(context, uri), t.mime ?: Downloads.guessMime(t.name))
            .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_ACTIVITY_NEW_TASK)
        val open = PendingIntent.getActivity(
            context,
            idFor(t.id),
            Intent.createChooser(view, null).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        val n = NotificationCompat.Builder(context, Notifications.CHANNEL_SHARED)
            .setSmallIcon(R.drawable.ic_notification)
            .setContentTitle(context.getString(R.string.share_received, t.name))
            .setContentText(context.getString(R.string.share_received_body, t.deviceName, size(context, t.size)))
            .setContentIntent(open)
            .setAutoCancel(true)
            .build()
        Notifications.post(context, idFor(t.id), n)
    }

    fun sent(context: Context, t: TransferData) {
        val n = NotificationCompat.Builder(context, Notifications.CHANNEL_TRANSFERS)
            .setSmallIcon(R.drawable.ic_notification)
            .setContentTitle(context.getString(R.string.share_sent, t.name, t.deviceName))
            .setContentText(size(context, t.size))
            .setAutoCancel(true)
            .setTimeoutAfter(10_000)
            .build()
        Notifications.post(context, idFor(t.id), n)
    }

    fun failed(context: Context, t: TransferData, reason: String) {
        val title = if (t.incoming) {
            context.getString(R.string.share_receive_failed, t.name)
        } else {
            context.getString(R.string.share_send_failed, t.name, t.deviceName)
        }
        val n = NotificationCompat.Builder(context, Notifications.CHANNEL_SHARED)
            .setSmallIcon(R.drawable.ic_notification)
            .setContentTitle(title)
            .setContentText(reason)
            .setAutoCancel(true)
            .build()
        Notifications.post(context, idFor(t.id), n)
    }

    fun clear(context: Context, t: TransferData) {
        NotificationManagerCompat.from(context).cancel(idFor(t.id))
    }

    /** A link: tap to open (Android doesn't let a background app open the browser itself). */
    fun link(context: Context, from: String, url: String) {
        val open = PendingIntent.getActivity(
            context,
            nextTextId.get(),
            Intent(Intent.ACTION_VIEW, url.toUri()).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK),
            PendingIntent.FLAG_IMMUTABLE,
        )
        val n = NotificationCompat.Builder(context, Notifications.CHANNEL_SHARED)
            .setSmallIcon(R.drawable.ic_notification)
            .setContentTitle(context.getString(R.string.share_link_from, from))
            .setContentText(url)
            .setContentIntent(open)
            .setAutoCancel(true)
            .addAction(0, context.getString(R.string.share_open), open)
            .build()
        Notifications.post(context, nextTextId.getAndIncrement(), n)
    }

    fun text(context: Context, from: String, text: String) {
        val n = NotificationCompat.Builder(context, Notifications.CHANNEL_SHARED)
            .setSmallIcon(R.drawable.ic_notification)
            .setContentTitle(context.getString(R.string.share_text_from, from))
            .setContentText(text)
            .setStyle(NotificationCompat.BigTextStyle().bigText(text.take(1000)))
            .setAutoCancel(true)
            .build()
        Notifications.post(context, nextTextId.getAndIncrement(), n)
    }

    private fun action(context: Context, action: String, transfer: ULong): PendingIntent =
        PendingIntent.getBroadcast(
            context,
            idFor(transfer) + if (action == ACTION_ACCEPT) 1 else 0,
            Intent(context, ShareActionReceiver::class.java)
                .setAction(action)
                .putExtra(EXTRA_TRANSFER, transfer.toLong()),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )

    /** Accept, Decline and Cancel buttons. */
    class ShareActionReceiver : BroadcastReceiver() {
        override fun onReceive(context: Context, intent: Intent) {
            val transfer = intent.getLongExtra(EXTRA_TRANSFER, 0).toULong()
            when (intent.action) {
                ACTION_ACCEPT -> Pairly.acceptTransfer(transfer)
                ACTION_CANCEL -> Pairly.cancelTransfer(transfer)
            }
        }
    }
}
