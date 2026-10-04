package dev.pairly.android.notifications

import android.app.Notification
import android.app.PendingIntent
import android.app.RemoteInput
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.graphics.Canvas
import android.graphics.drawable.Drawable
import android.os.Build
import android.os.Bundle
import android.service.notification.NotificationListenerService
import android.service.notification.StatusBarNotification
import android.util.Log
import androidx.core.app.NotificationManagerCompat
import androidx.core.graphics.createBitmap
import dev.pairly.core.ffi.IconData
import dev.pairly.core.ffi.NotificationActionData
import dev.pairly.core.ffi.NotificationData
import java.nio.ByteBuffer
import java.util.Collections
import java.util.concurrent.ConcurrentHashMap

/** Bound by the system once the user grants notification access. */
class PairlyNotificationListener : NotificationListenerService() {
    override fun onListenerConnected() {
        instance = this
        PhoneNotifications.resend(this)
    }

    override fun onListenerDisconnected() {
        instance = null
    }

    override fun onNotificationPosted(sbn: StatusBarNotification) = PhoneNotifications.posted(this, sbn)

    override fun onNotificationRemoved(sbn: StatusBarNotification) = PhoneNotifications.removed(sbn.key)

    companion object {
        @Volatile
        var instance: PairlyNotificationListener? = null
            private set

        fun isEnabled(context: Context): Boolean =
            context.packageName in NotificationManagerCompat.getEnabledListenerPackages(context)
    }
}

/**
 * The phone's own notifications: forwards them to the node and carries out what paired devices
 * ask (reply, action, dismiss) on the originals.
 */
object PhoneNotifications {
    private const val TAG = "PairlyNotifications"
    private const val ICON_SIZE = 64

    /** Fed to the node; set while it runs. */
    @Volatile
    var sink: Sink? = null

    interface Sink {
        fun posted(notification: NotificationData)
        fun removed(id: String)
    }

    @Volatile
    private var prefs: NotificationPrefs? = null
    private val forwarded = ConcurrentHashMap<String, StatusBarNotification>()
    private val labels = ConcurrentHashMap<String, String>()

    /**
     * Recently removed notifications with buttons, kept so a reply typed on another device
     * still works: their PendingIntents stay valid after the notification is gone (chat apps
     * clear notifications as soon as the message is read anywhere, e.g. WhatsApp Web).
     */
    private data class Retired(val sbn: StatusBarNotification, val at: Long)
    private val retired: MutableMap<String, Retired> = Collections.synchronizedMap(
        object : LinkedHashMap<String, Retired>(16, 0.75f, true) {
            override fun removeEldestEntry(eldest: MutableMap.MutableEntry<String, Retired>?) = size > 50
        },
    )
    private const val RETIRED_FOR_MS = 15 * 60 * 1000L

    /** Which conversation a notification belonged to; kept after it goes away (bounded). */
    private data class Conversation(val pkg: String, val id: String)
    private val conversations: MutableMap<String, Conversation> = Collections.synchronizedMap(
        object : LinkedHashMap<String, Conversation>(64, 0.75f, true) {
            override fun removeEldestEntry(eldest: MutableMap.MutableEntry<String, Conversation>?) = size > 200
        },
    )

    private fun conversationOf(sbn: StatusBarNotification): Conversation {
        val n = sbn.notification
        val id = n.shortcutId ?: n.extras.getCharSequence(Notification.EXTRA_TITLE)?.toString() ?: sbn.key
        return Conversation(sbn.packageName, id)
    }

    /**
     * The notification a paired device means by [id]. Chat apps often replace a message's
     * notification (new key) while the mirror is still on screen; then use the newest
     * notification of the same conversation.
     */
    private fun resolve(id: String): StatusBarNotification? {
        forwarded[id]?.let { return it }
        conversations[id]?.let { conversation ->
            forwarded.values.filter { conversationOf(it) == conversation }.maxByOrNull { it.postTime }?.let {
                Log.i(TAG, "notification $id was replaced; using ${it.key}")
                return it
            }
        }
        val old = retired[id]?.takeIf { System.currentTimeMillis() - it.at < RETIRED_FOR_MS } ?: return null
        Log.i(TAG, "notification $id is gone; using its saved actions")
        return old.sbn
    }

    fun prefs(context: Context): NotificationPrefs =
        prefs ?: synchronized(this) { prefs ?: NotificationPrefs(context.applicationContext).also { prefs = it } }

    /** Send everything currently showing (on node start and when the listener connects). */
    fun resend(context: Context) {
        val listener = PairlyNotificationListener.instance ?: return
        val active = runCatching { listener.activeNotifications }.getOrNull() ?: return
        active.forEach { posted(context, it) }
    }

    fun posted(context: Context, sbn: StatusBarNotification) {
        val sink = sink ?: return
        val n = sbn.notification
        val pkg = sbn.packageName
        if (pkg == context.packageName) return // our own mirrors and status: never echo
        val flags = n.flags
        if (flags and (Notification.FLAG_ONGOING_EVENT or Notification.FLAG_FOREGROUND_SERVICE) != 0) return
        if (flags and Notification.FLAG_GROUP_SUMMARY != 0) return
        if (n.extras.containsKey(Notification.EXTRA_MEDIA_SESSION)) return
        val label = appLabel(context, pkg)
        prefs(context).noteSeen(pkg, label)
        if (prefs(context).isMuted(pkg)) return

        val extras = n.extras
        val title = extras.getCharSequence(Notification.EXTRA_TITLE)?.toString().orEmpty()
        val text = (extras.getCharSequence(Notification.EXTRA_BIG_TEXT) ?: extras.getCharSequence(Notification.EXTRA_TEXT))
            ?.toString().orEmpty()
        if (title.isBlank() && text.isBlank()) return

        val actions = n.actions.orEmpty()
        val buttons = actions.withIndex()
            .filter { (_, a) -> a.remoteInputs.isNullOrEmpty() && a.title != null }
            .take(3)
            .map { (i, a) -> NotificationActionData(i.toString(), a.title.toString()) }
        val canReply = actions.any { a -> a.remoteInputs?.any { it.allowFreeFormInput } == true }
        val update = forwarded.containsKey(sbn.key)
        forwarded[sbn.key] = sbn
        conversations[sbn.key] = conversationOf(sbn)
        Log.d(TAG, "forwarding ${sbn.key} (conversation ${conversations[sbn.key]?.id})")
        val data = NotificationData(
            id = sbn.key,
            app = label,
            title = title,
            text = text,
            timeMs = sbn.postTime.toULong(),
            actions = buttons,
            canReply = canReply,
            icon = icon(context, sbn),
            silent = update && flags and Notification.FLAG_ONLY_ALERT_ONCE != 0,
        )
        runCatching { sink.posted(data) }.onFailure { Log.w(TAG, "forwarding failed", it) }
    }

    fun removed(key: String) {
        val sbn = forwarded.remove(key) ?: return
        if (!sbn.notification.actions.isNullOrEmpty()) retired[key] = Retired(sbn, System.currentTimeMillis())
        Log.d(TAG, "removed $key")
        sink?.removed(key)
    }

    fun dismiss(id: String) {
        PairlyNotificationListener.instance?.cancelNotification(id)
    }

    fun action(context: Context, id: String, key: String) {
        val action = resolve(id)?.notification?.actions?.getOrNull(key.toIntOrNull() ?: return) ?: return
        runCatching { action.actionIntent.send() }.onFailure { Log.w(TAG, "action failed", it) }
    }

    fun reply(context: Context, id: String, text: String) {
        val sbn = resolve(id)
        if (sbn == null) {
            Log.w(TAG, "reply: notification $id is no longer showing (${forwarded.size} tracked)")
            return
        }
        val actions = sbn.notification.actions.orEmpty()
        val action = actions.firstOrNull { a -> a.remoteInputs?.any { it.allowFreeFormInput } == true }
        if (action == null) {
            Log.w(TAG, "reply: ${sbn.packageName} has no reply action (${actions.map { it.title }})")
            return
        }
        val target = if (Build.VERSION.SDK_INT < Build.VERSION_CODES.S) "unknown" else action.actionIntent.let {
            when {
                it.isActivity -> "activity"
                it.isBroadcast -> "broadcast"
                it.isForegroundService -> "foreground service"
                it.isService -> "service"
                else -> "other"
            }
        }
        Log.i(TAG, "reply: sending to ${sbn.packageName} via \"${action.title}\" ($target, ${action.remoteInputs.size} inputs)")
        val inputs = action.remoteInputs
        val results = Bundle().apply { inputs.filter { it.allowFreeFormInput }.forEach { putCharSequence(it.resultKey, text) } }
        val intent = Intent().addFlags(Intent.FLAG_RECEIVER_FOREGROUND)
        RemoteInput.addResultsToIntent(inputs, intent, results)
        try {
            action.actionIntent.send(context, 0, intent)
            Log.i(TAG, "reply: sent")
        } catch (e: PendingIntent.CanceledException) {
            Log.w(TAG, "reply failed", e)
        }
    }

    private fun appLabel(context: Context, pkg: String): String = labels.getOrPut(pkg) {
        runCatching {
            val pm = context.packageManager
            val info = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
                pm.getApplicationInfo(pkg, PackageManager.ApplicationInfoFlags.of(0))
            } else {
                @Suppress("DEPRECATION")
                pm.getApplicationInfo(pkg, 0)
            }
            pm.getApplicationLabel(info).toString()
        }.getOrDefault(pkg)
    }

    /** The sender's avatar if there is one, else the app icon, as straight-alpha RGBA. */
    private fun icon(context: Context, sbn: StatusBarNotification): IconData? = runCatching {
        val drawable: Drawable = sbn.notification.getLargeIcon()?.loadDrawable(context)
            ?: context.packageManager.getApplicationIcon(sbn.packageName)
        drawable.toIconData()
    }.getOrNull()

    private fun Drawable.toIconData(): IconData {
        val bitmap = createBitmap(ICON_SIZE, ICON_SIZE)
        val canvas = Canvas(bitmap)
        setBounds(0, 0, ICON_SIZE, ICON_SIZE)
        draw(canvas)
        val buffer = ByteBuffer.allocate(ICON_SIZE * ICON_SIZE * 4)
        bitmap.copyPixelsToBuffer(buffer) // RGBA, premultiplied
        bitmap.recycle()
        val px = buffer.array()
        for (i in px.indices step 4) {
            val a = px[i + 3].toInt() and 0xff
            if (a in 1..254) {
                for (c in 0..2) px[i + c] = ((px[i + c].toInt() and 0xff) * 255 / a).coerceAtMost(255).toByte()
            }
        }
        return IconData(ICON_SIZE.toUShort(), ICON_SIZE.toUShort(), px)
    }
}
