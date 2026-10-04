package dev.pairly.android.media

import android.app.Notification
import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.graphics.drawable.Icon
import android.media.MediaMetadata
import android.media.VolumeProvider
import android.media.session.MediaSession
import android.media.session.PlaybackState
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.util.Log
import androidx.core.app.NotificationManagerCompat
import dev.pairly.android.Notifications
import dev.pairly.android.Pairly
import dev.pairly.android.R
import dev.pairly.core.ffi.MediaActionData
import dev.pairly.core.ffi.PlayerData
import java.net.HttpURLConnection
import java.net.URL
import kotlin.concurrent.thread

/**
 * A paired PC's player, shown as a real media session: a media notification and lock-screen and
 * quick-settings controls, with Volume −/+ buttons for the PC player.
 *
 * The session also offers remote volume, so the hardware volume keys control the PC on older
 * Android. Android 13+ only routes volume keys to remote sessions of apps that are a full
 * MediaRouter2 output (as Cast is), hence the buttons.
 */
object PcPlayers {
    private const val TAG = "PcPlayers"
    private const val NOTIFICATION_ID = 7
    private const val MAX_ART_BYTES = 1024 * 1024
    private const val ACTION = "dev.pairly.android.media.COMMAND"
    private const val EXTRA_COMMAND = "command"
    private const val VOLUME_DOWN = "volume_down"
    private const val VOLUME_UP = "volume_up"
    private const val VOLUME_STEP = 10

    private val main = Handler(Looper.getMainLooper())
    private var session: MediaSession? = null
    private var deviceId: String? = null
    private var deviceName: String = ""
    private var player: PlayerData? = null
    private var volume: VolumeProvider? = null
    private val art = mutableMapOf<String, Bitmap>()
    private val fetching = mutableSetOf<String>()

    fun show(context: Context, fromId: String, fromName: String, players: List<PlayerData>) {
        val app = context.applicationContext
        main.post {
            val current = players.firstOrNull()
            if (current == null) {
                if (deviceId == fromId) hide(app)
                return@post
            }
            deviceId = fromId
            deviceName = fromName.ifEmpty { deviceName }
            player = current
            current.art?.takeIf { it.startsWith("https://") && it !in art && fetching.add(it) }?.let { fetch(app, it) }
            update(app)
        }
    }

    fun artwork(context: Context, key: String, data: ByteArray) {
        val bitmap = BitmapFactory.decodeByteArray(data, 0, data.size) ?: return
        val app = context.applicationContext
        main.post {
            art[key] = bitmap
            if (art.size > 8) art.keys.first { it != key }.let(art::remove)
            if (player?.art == key) update(app)
        }
    }

    private fun fetch(context: Context, url: String) {
        thread(name = "pairly-art", isDaemon = true) {
            val bitmap = runCatching {
                val conn = URL(url).openConnection() as HttpURLConnection
                conn.connectTimeout = 5000
                conn.readTimeout = 5000
                conn.inputStream.use { input ->
                    val bytes = input.readNBytesCompat(MAX_ART_BYTES)
                    BitmapFactory.decodeByteArray(bytes, 0, bytes.size)
                }
            }.getOrNull()
            main.post {
                fetching.remove(url)
                if (bitmap != null) {
                    art[url] = bitmap
                    if (player?.art == url) update(context)
                }
            }
        }
    }

    private fun java.io.InputStream.readNBytesCompat(max: Int): ByteArray {
        val out = java.io.ByteArrayOutputStream()
        val buf = ByteArray(16 * 1024)
        while (out.size() < max) {
            val n = read(buf)
            if (n < 0) break
            out.write(buf, 0, n)
        }
        return out.toByteArray()
    }

    /** Volume −/+: relative to the last volume the PC reported. */
    private fun stepVolume(delta: Int) {
        val p = player ?: return
        val now = p.volume?.toInt() ?: return
        val next = (now + delta).coerceIn(0, 100)
        player = p.copy(volume = next.toUByte())
        volume?.currentVolume = next
        send(MediaActionData.SetVolume(next.toUByte()))
    }

    private fun send(action: MediaActionData) {
        val id = deviceId ?: return
        val p = player ?: return
        Pairly.mediaCommand(id, p.id, action)
    }

    private fun ensureSession(context: Context): MediaSession =
        session ?: MediaSession(context, "Pairly PC player").also { s ->
            s.setCallback(object : MediaSession.Callback() {
                override fun onPlay() = send(MediaActionData.Play)
                override fun onPause() = send(MediaActionData.Pause)
                override fun onStop() = send(MediaActionData.Stop)
                override fun onSkipToNext() = send(MediaActionData.Next)
                override fun onSkipToPrevious() = send(MediaActionData.Previous)
                override fun onSeekTo(pos: Long) = send(MediaActionData.SetPosition(pos.coerceAtLeast(0).toULong()))
                override fun onCustomAction(action: String, extras: android.os.Bundle?) = when (action) {
                    VOLUME_DOWN -> stepVolume(-VOLUME_STEP)
                    VOLUME_UP -> stepVolume(VOLUME_STEP)
                    else -> Unit
                }
            })
            s.isActive = true
            session = s
        }

    private fun update(context: Context) {
        val p = player ?: return
        val s = ensureSession(context)
        val bitmap = p.art?.let(art::get)
        s.setMetadata(
            MediaMetadata.Builder()
                .putString(MediaMetadata.METADATA_KEY_TITLE, p.title.ifEmpty { p.name })
                .putString(MediaMetadata.METADATA_KEY_ARTIST, p.artist)
                .putString(MediaMetadata.METADATA_KEY_ALBUM, p.album)
                .putLong(MediaMetadata.METADATA_KEY_DURATION, p.lengthMs?.toLong() ?: -1L)
                .apply { if (bitmap != null) putBitmap(MediaMetadata.METADATA_KEY_ALBUM_ART, bitmap) }
                .build(),
        )
        var actions = PlaybackState.ACTION_PLAY_PAUSE
        if (p.canPlay) actions = actions or PlaybackState.ACTION_PLAY
        if (p.canPause) actions = actions or PlaybackState.ACTION_PAUSE
        if (p.canNext) actions = actions or PlaybackState.ACTION_SKIP_TO_NEXT
        if (p.canPrevious) actions = actions or PlaybackState.ACTION_SKIP_TO_PREVIOUS
        if (p.canSeek) actions = actions or PlaybackState.ACTION_SEEK_TO
        val state = PlaybackState.Builder().setActions(actions)
        if (p.volume != null) {
            state.addCustomAction(
                PlaybackState.CustomAction.Builder(VOLUME_DOWN, context.getString(R.string.media_volume_down), R.drawable.ic_volume_down).build(),
            )
            state.addCustomAction(
                PlaybackState.CustomAction.Builder(VOLUME_UP, context.getString(R.string.media_volume_up), R.drawable.ic_volume_up).build(),
            )
        }
        s.setPlaybackState(
            state
                .setState(
                    if (p.playing) PlaybackState.STATE_PLAYING else PlaybackState.STATE_PAUSED,
                    p.positionMs?.toLong() ?: PlaybackState.PLAYBACK_POSITION_UNKNOWN,
                    if (p.playing) 1f else 0f,
                    SystemClock.elapsedRealtime(),
                )
                .build(),
        )
        updateVolume(s, p)
        notify(context, s, p, bitmap)
    }

    /** With a remote volume, the phone's volume keys set the PC player's volume. */
    private fun updateVolume(s: MediaSession, p: PlayerData) {
        val percent = p.volume?.toInt()
        if (percent == null) {
            if (volume != null) {
                s.setPlaybackToLocal(android.media.AudioAttributes.Builder().build())
                volume = null
            }
            return
        }
        val existing = volume
        if (existing != null) {
            existing.currentVolume = percent
            return
        }
        val provider = object : VolumeProvider(VOLUME_CONTROL_ABSOLUTE, 100, percent) {
            override fun onSetVolumeTo(v: Int) {
                currentVolume = v.coerceIn(0, 100)
                send(MediaActionData.SetVolume(currentVolume.toUByte()))
            }

            override fun onAdjustVolume(direction: Int) = onSetVolumeTo(currentVolume + direction * 5)
        }
        s.setPlaybackToRemote(provider)
        volume = provider
    }

    private fun action(context: Context, code: Int, icon: Int, title: String, command: String): Notification.Action =
        Notification.Action.Builder(
            Icon.createWithResource(context, icon),
            title,
            PendingIntent.getBroadcast(
                context,
                code,
                Intent(context, MediaCommandReceiver::class.java).setAction(ACTION).putExtra(EXTRA_COMMAND, command),
                PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
            ),
        ).build()

    private fun notify(context: Context, s: MediaSession, p: PlayerData, bitmap: Bitmap?) {
        val playPause = if (p.playing) {
            action(context, 2, android.R.drawable.ic_media_pause, context.getString(R.string.media_pause), "play_pause")
        } else {
            action(context, 2, android.R.drawable.ic_media_play, context.getString(R.string.media_play), "play_pause")
        }
        val subtitle = listOf(p.artist, context.getString(R.string.media_on, deviceName))
            .filter { it.isNotEmpty() }
            .joinToString(" · ")
        val builder = Notification.Builder(context, Notifications.CHANNEL_MEDIA)
            .setSmallIcon(R.drawable.ic_notification)
            .setContentTitle(p.title.ifEmpty { p.name })
            .setContentText(subtitle)
            .setLargeIcon(bitmap)
            .setOngoing(p.playing)
            .setVisibility(Notification.VISIBILITY_PUBLIC)
            .addAction(action(context, 1, android.R.drawable.ic_media_previous, context.getString(R.string.media_previous), "previous"))
            .addAction(playPause)
            .addAction(action(context, 3, android.R.drawable.ic_media_next, context.getString(R.string.media_next), "next"))
        if (p.volume != null) {
            builder.addAction(action(context, 4, R.drawable.ic_volume_down, context.getString(R.string.media_volume_down), VOLUME_DOWN))
            builder.addAction(action(context, 5, R.drawable.ic_volume_up, context.getString(R.string.media_volume_up), VOLUME_UP))
        }
        val n = builder
            .setStyle(Notification.MediaStyle().setMediaSession(s.sessionToken).setShowActionsInCompactView(0, 1, 2))
            .build()
        Notifications.post(context, NOTIFICATION_ID, n)
    }

    private fun hide(context: Context) {
        NotificationManagerCompat.from(context).cancel(NOTIFICATION_ID)
        session?.release()
        session = null
        volume = null
        player = null
        deviceId = null
    }

    /** Disconnected: drop the PC's player. */
    fun clear(context: Context) {
        main.post { hide(context.applicationContext) }
    }

    class MediaCommandReceiver : BroadcastReceiver() {
        override fun onReceive(context: Context, intent: Intent) {
            when (intent.getStringExtra(EXTRA_COMMAND)) {
                "previous" -> main.post { send(MediaActionData.Previous) }
                "next" -> main.post { send(MediaActionData.Next) }
                "play_pause" -> main.post { send(MediaActionData.PlayPause) }
                VOLUME_DOWN -> main.post { stepVolume(-VOLUME_STEP) }
                VOLUME_UP -> main.post { stepVolume(VOLUME_STEP) }
                else -> Log.w(TAG, "unknown media command")
            }
        }
    }
}
