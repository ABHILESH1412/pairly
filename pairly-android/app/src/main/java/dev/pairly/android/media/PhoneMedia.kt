package dev.pairly.android.media

import android.content.ComponentName
import android.content.Context
import android.graphics.Bitmap
import android.media.AudioManager
import android.media.MediaMetadata
import android.media.session.MediaController
import android.media.session.MediaSessionManager
import android.media.session.PlaybackState
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.util.Log
import androidx.core.graphics.scale
import dev.pairly.android.Pairly
import dev.pairly.android.notifications.PairlyNotificationListener
import dev.pairly.core.ffi.MediaActionData
import dev.pairly.core.ffi.PlayerData
import java.io.ByteArrayOutputStream
import kotlin.math.roundToInt

/**
 * The phone's own players (Spotify, YouTube, …), read through MediaSessionManager. That's
 * allowed because Pairly has notification access. Paired PCs see them and can control them.
 */
object PhoneMedia {
    private const val TAG = "PhoneMedia"
    private const val DEBOUNCE_MS = 300L
    private const val ART_SIZE = 300

    private val main = Handler(Looper.getMainLooper())
    private lateinit var appContext: Context
    private var manager: MediaSessionManager? = null
    private val controllers = mutableMapOf<String, Pair<MediaController, MediaController.Callback>>()

    /** Read by Rust threads, written on the main thread. */
    @Volatile
    private var snapshot: List<PlayerData> = emptyList()
    private val art = object : LinkedHashMap<String, ByteArray>(8, 0.75f, true) {
        override fun removeEldestEntry(eldest: MutableMap.MutableEntry<String, ByteArray>) = size > 8
    }

    private val sessionsChanged = MediaSessionManager.OnActiveSessionsChangedListener { list ->
        bind(list.orEmpty())
    }
    private val publish = Runnable {
        snapshot = controllers.values.map { (c, _) -> describe(c) }.sortedByDescending { it.playing }
        Pairly.mediaChanged()
    }

    /** Start watching (again, e.g. after notification access was granted). Main thread. */
    fun start(context: Context) {
        appContext = context.applicationContext
        if (manager != null || !PairlyNotificationListener.isEnabled(appContext)) return
        val m = appContext.getSystemService(MediaSessionManager::class.java) ?: return
        val component = ComponentName(appContext, PairlyNotificationListener::class.java)
        try {
            m.addOnActiveSessionsChangedListener(sessionsChanged, component, main)
            manager = m
            bind(m.getActiveSessions(component))
        } catch (e: SecurityException) {
            Log.w(TAG, "no access to media sessions", e)
        }
    }

    fun stop() {
        main.post {
            manager?.removeOnActiveSessionsChangedListener(sessionsChanged)
            manager = null
            controllers.values.forEach { (c, cb) -> c.unregisterCallback(cb) }
            controllers.clear()
            snapshot = emptyList()
        }
    }

    fun players(): List<PlayerData> = snapshot

    fun artwork(key: String): ByteArray? = synchronized(art) { art[key] }

    private fun bind(list: List<MediaController>) {
        val own = appContext.packageName
        val wanted = list.filter { it.packageName != own }.associateBy { it.packageName }
        controllers.keys.filter { it !in wanted }.forEach { pkg ->
            controllers.remove(pkg)?.let { (c, cb) -> c.unregisterCallback(cb) }
        }
        for ((pkg, controller) in wanted) {
            if (controllers[pkg]?.first?.sessionToken == controller.sessionToken) continue
            controllers.remove(pkg)?.let { (c, cb) -> c.unregisterCallback(cb) }
            val callback = object : MediaController.Callback() {
                override fun onMetadataChanged(metadata: MediaMetadata?) = changed()
                override fun onPlaybackStateChanged(state: PlaybackState?) = changed()
                override fun onAudioInfoChanged(info: MediaController.PlaybackInfo) = changed()
                override fun onSessionDestroyed() = changed()
            }
            controller.registerCallback(callback, main)
            controllers[pkg] = controller to callback
        }
        changed()
    }

    private fun changed() {
        main.removeCallbacks(publish)
        main.postDelayed(publish, DEBOUNCE_MS)
    }

    private fun appName(pkg: String): String = runCatching {
        val pm = appContext.packageManager
        pm.getApplicationLabel(pm.getApplicationInfo(pkg, 0)).toString()
    }.getOrDefault(pkg)

    private fun describe(c: MediaController): PlayerData {
        val meta = c.metadata
        val state = c.playbackState
        val actions = state?.actions ?: 0L
        fun can(flag: Long) = actions and flag != 0L
        val playing = state?.state == PlaybackState.STATE_PLAYING
        val position = state?.let {
            var pos = it.position
            if (playing && it.lastPositionUpdateTime > 0) {
                pos += ((SystemClock.elapsedRealtime() - it.lastPositionUpdateTime) * it.playbackSpeed).toLong()
            }
            pos.coerceAtLeast(0)
        }
        val title = meta?.getString(MediaMetadata.METADATA_KEY_TITLE).orEmpty()
        val artist = meta?.getString(MediaMetadata.METADATA_KEY_ARTIST)
            ?: meta?.getString(MediaMetadata.METADATA_KEY_ALBUM_ARTIST).orEmpty()
        val bitmap = meta?.getBitmap(MediaMetadata.METADATA_KEY_ALBUM_ART)
            ?: meta?.getBitmap(MediaMetadata.METADATA_KEY_ART)
            ?: meta?.getBitmap(MediaMetadata.METADATA_KEY_DISPLAY_ICON)
        val artKey = bitmap?.let { b ->
            val key = "art:${c.packageName}:${(title + artist).hashCode()}"
            synchronized(art) { if (key !in art) art[key] = jpeg(b) }
            key
        }
        val duration = meta?.getLong(MediaMetadata.METADATA_KEY_DURATION)?.takeIf { it > 0 }
        return PlayerData(
            id = c.packageName,
            name = appName(c.packageName),
            title = title,
            artist = artist,
            album = meta?.getString(MediaMetadata.METADATA_KEY_ALBUM).orEmpty(),
            art = artKey,
            lengthMs = duration?.toULong(),
            positionMs = position?.toULong(),
            playing = playing,
            canPlay = can(PlaybackState.ACTION_PLAY) || can(PlaybackState.ACTION_PLAY_PAUSE),
            canPause = can(PlaybackState.ACTION_PAUSE) || can(PlaybackState.ACTION_PLAY_PAUSE),
            canNext = can(PlaybackState.ACTION_SKIP_TO_NEXT),
            canPrevious = can(PlaybackState.ACTION_SKIP_TO_PREVIOUS),
            canSeek = can(PlaybackState.ACTION_SEEK_TO),
            volume = volume(c)?.toUByte(),
        )
    }

    private fun volume(c: MediaController): Int? {
        val info = c.playbackInfo
        return if (info.playbackType == MediaController.PlaybackInfo.PLAYBACK_TYPE_REMOTE) {
            if (info.maxVolume > 0) info.currentVolume * 100 / info.maxVolume else null
        } else {
            val audio = appContext.getSystemService(AudioManager::class.java) ?: return null
            val max = audio.getStreamMaxVolume(AudioManager.STREAM_MUSIC)
            if (max > 0) audio.getStreamVolume(AudioManager.STREAM_MUSIC) * 100 / max else null
        }
    }

    private fun jpeg(bitmap: Bitmap): ByteArray {
        val scale = ART_SIZE.toFloat() / maxOf(bitmap.width, bitmap.height)
        val small = if (scale < 1f) {
            bitmap.scale((bitmap.width * scale).roundToInt(), (bitmap.height * scale).roundToInt())
        } else {
            bitmap
        }
        return ByteArrayOutputStream().use {
            small.compress(Bitmap.CompressFormat.JPEG, 85, it)
            it.toByteArray()
        }
    }

    /** A paired device controls one of our players. */
    fun command(player: String, action: MediaActionData) {
        main.post {
            val c = controllers[player]?.first ?: return@post
            val t = c.transportControls
            when (action) {
                MediaActionData.Play -> t.play()
                MediaActionData.Pause -> t.pause()
                MediaActionData.PlayPause ->
                    if (c.playbackState?.state == PlaybackState.STATE_PLAYING) t.pause() else t.play()
                MediaActionData.Stop -> t.stop()
                MediaActionData.Next -> t.skipToNext()
                MediaActionData.Previous -> t.skipToPrevious()
                is MediaActionData.Seek -> {
                    val now = describe(c).positionMs?.toLong() ?: 0L
                    t.seekTo((now + action.offsetMs).coerceAtLeast(0))
                }
                is MediaActionData.SetPosition -> t.seekTo(action.positionMs.toLong())
                is MediaActionData.SetVolume -> setVolume(c, action.percent.toInt())
            }
        }
    }

    private fun setVolume(c: MediaController, percent: Int) {
        val info = c.playbackInfo
        if (info.playbackType == MediaController.PlaybackInfo.PLAYBACK_TYPE_REMOTE) {
            c.setVolumeTo(percent * info.maxVolume / 100, 0)
        } else {
            val audio = appContext.getSystemService(AudioManager::class.java) ?: return
            val max = audio.getStreamMaxVolume(AudioManager.STREAM_MUSIC)
            audio.setStreamVolume(AudioManager.STREAM_MUSIC, percent * max / 100, 0)
        }
        changed()
    }
}
