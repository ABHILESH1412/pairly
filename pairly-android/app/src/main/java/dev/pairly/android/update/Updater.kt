package dev.pairly.android.update

import android.Manifest
import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.pm.PackageInstaller
import android.content.pm.PackageManager
import android.os.Build
import android.util.Log
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import androidx.core.content.ContextCompat
import androidx.core.content.IntentCompat
import androidx.core.content.edit
import dev.pairly.android.MainActivity
import dev.pairly.android.Notifications
import dev.pairly.android.PairlyService
import dev.pairly.android.R
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext
import org.json.JSONObject
import java.io.File
import java.net.HttpURLConnection
import java.net.URL
import java.security.MessageDigest

/**
 * Updates from Pairly's GitHub releases. The service checks now and then; a newer release's APK
 * is downloaded, its checksum checked against the release's SHA256SUMS, and handed to Android's
 * package installer. Android itself refuses an APK not signed with the same key as the installed
 * app, so a forged download can't install. The first update needs the user to tap Install (and
 * to allow Pairly to install apps); once Pairly installed itself, Android 12+ lets later updates
 * go through without asking.
 */
object Updater {
    sealed interface Status {
        data object Idle : Status
        data object Checking : Status
        data object UpToDate : Status
        data class Available(val version: String) : Status
        data class Downloading(val version: String) : Status
        data class Installing(val version: String) : Status
        data class Failed(val reason: String) : Status
    }

    private const val TAG = "PairlyUpdate"
    private const val API = "https://api.github.com/repos/ABHILESH1412/pairly/releases/latest"
    private const val PREFS = "updates"
    private const val KEY_AUTO = "auto"
    const val ACTION_INSTALL = "dev.pairly.android.update.INSTALL"
    private const val ACTION_STATUS = "dev.pairly.android.update.STATUS"
    private const val NOTIFICATION_ID = 7301

    private val _status = MutableStateFlow<Status>(Status.Idle)
    val status: StateFlow<Status> = _status.asStateFlow()
    private val busy = Mutex()

    /** Development builds ("Pairly Dev") have another package and key: they never update. */
    fun supported(context: Context): Boolean = !context.packageName.endsWith(".dev")

    fun auto(context: Context): Boolean =
        context.getSharedPreferences(PREFS, Context.MODE_PRIVATE).getBoolean(KEY_AUTO, true)

    fun setAuto(context: Context, on: Boolean) =
        context.getSharedPreferences(PREFS, Context.MODE_PRIVATE).edit { putBoolean(KEY_AUTO, on) }

    fun currentVersion(context: Context): String =
        runCatching { context.packageManager.getPackageInfo(context.packageName, 0).versionName }
            .getOrNull().orEmpty()

    /**
     * Look for a newer release. With automatic updates on (or [install]), it's downloaded and
     * installed; otherwise the user gets a notification to do it.
     */
    suspend fun check(context: Context, install: Boolean = false) {
        if (!supported(context)) return
        busy.withLock {
            _status.value = Status.Checking
            val release = runCatching { withContext(Dispatchers.IO) { latest() } }.getOrElse {
                Log.w(TAG, "update check failed", it)
                _status.value = Status.Failed(it.message ?: "couldn't reach GitHub")
                return
            }
            val current = currentVersion(context)
            if (!newer(release.version, current)) {
                _status.value = Status.UpToDate
                return
            }
            if (!install && !auto(context)) {
                _status.value = Status.Available(release.version)
                notifyAvailable(context, release.version)
                return
            }
            _status.value = Status.Downloading(release.version)
            val apk = runCatching { withContext(Dispatchers.IO) { download(context, release) } }.getOrElse {
                Log.w(TAG, "update download failed", it)
                _status.value = Status.Failed(it.message ?: "download failed")
                return
            }
            _status.value = Status.Installing(release.version)
            runCatching { install(context, apk) }.onFailure {
                Log.w(TAG, "update install failed", it)
                _status.value = Status.Failed(it.message ?: "couldn't install")
            }
        }
    }

    private class Release(val version: String, val apkUrl: String, val sumsUrl: String?, val apkName: String)

    private fun latest(): Release {
        val json = JSONObject(get(API).decodeToString())
        val version = json.getString("tag_name").removePrefix("v")
        val assets = json.getJSONArray("assets")
        var apk: Pair<String, String>? = null
        var sums: String? = null
        for (i in 0 until assets.length()) {
            val a = assets.getJSONObject(i)
            val name = a.getString("name")
            val url = a.getString("browser_download_url")
            when {
                name == "pairly-$version.apk" -> apk = name to url
                name == "SHA256SUMS" -> sums = url
            }
        }
        val (name, url) = apk ?: error("the release has no APK")
        return Release(version, url, sums, name)
    }

    private fun download(context: Context, release: Release): File {
        val dir = File(context.cacheDir, "updates").apply {
            deleteRecursively()
            mkdirs()
        }
        val bytes = get(release.apkUrl)
        release.sumsUrl?.let { url ->
            val expected = get(url).decodeToString().lines()
                .map { it.trim().split(Regex("\\s+")) }
                .firstOrNull { it.size == 2 && it[1].trimStart('*') == release.apkName }
                ?.get(0)
            val actual = MessageDigest.getInstance("SHA-256").digest(bytes).joinToString("") { "%02x".format(it) }
            check(expected == null || expected.equals(actual, ignoreCase = true)) { "the download is damaged" }
        }
        return File(dir, release.apkName).apply { writeBytes(bytes) }
    }

    private fun get(url: String): ByteArray {
        val conn = URL(url).openConnection() as HttpURLConnection
        conn.connectTimeout = 15_000
        conn.readTimeout = 60_000
        conn.setRequestProperty("Accept", "application/vnd.github+json")
        conn.setRequestProperty("User-Agent", "Pairly-Android")
        try {
            check(conn.responseCode == 200) { "GitHub answered ${conn.responseCode}" }
            return conn.inputStream.use { it.readBytes() }
        } finally {
            conn.disconnect()
        }
    }

    private fun install(context: Context, apk: File) {
        val installer = context.packageManager.packageInstaller
        val params = PackageInstaller.SessionParams(PackageInstaller.SessionParams.MODE_FULL_INSTALL).apply {
            setAppPackageName(context.packageName)
            // Updating ourselves: Android 12+ lets this through without asking once Pairly is the
            // app's installer.
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
                setRequireUserAction(PackageInstaller.SessionParams.USER_ACTION_NOT_REQUIRED)
            }
        }
        val id = installer.createSession(params)
        installer.openSession(id).use { session ->
            session.openWrite("base.apk", 0, apk.length()).use { out ->
                apk.inputStream().use { it.copyTo(out) }
                session.fsync(out)
            }
            val intent = Intent(context, UpdateReceiver::class.java).setAction(ACTION_STATUS)
            val pending = PendingIntent.getBroadcast(
                context,
                id,
                intent,
                PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_MUTABLE,
            )
            session.commit(pending.intentSender)
        }
    }

    /** The installer's answer: done, failed, or it needs the user to confirm. */
    internal fun onInstallStatus(context: Context, intent: Intent) {
        when (intent.getIntExtra(PackageInstaller.EXTRA_STATUS, PackageInstaller.STATUS_FAILURE)) {
            PackageInstaller.STATUS_PENDING_USER_ACTION -> {
                val confirm = IntentCompat.getParcelableExtra(intent, Intent.EXTRA_INTENT, Intent::class.java) ?: return
                confirm.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
                val version = (status.value as? Status.Installing)?.version.orEmpty()
                notify(
                    context,
                    context.getString(R.string.update_ready_title, version),
                    context.getString(R.string.update_ready_body),
                    PendingIntent.getActivity(context, 1, confirm, PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE),
                )
                // In the foreground, show the installer straight away.
                runCatching { context.startActivity(confirm) }
            }
            PackageInstaller.STATUS_SUCCESS -> _status.value = Status.UpToDate
            else -> {
                val message = intent.getStringExtra(PackageInstaller.EXTRA_STATUS_MESSAGE) ?: "install failed"
                _status.value = Status.Failed(message)
            }
        }
    }

    private fun notifyAvailable(context: Context, version: String) {
        val install = PendingIntent.getBroadcast(
            context,
            2,
            Intent(context, UpdateReceiver::class.java).setAction(ACTION_INSTALL),
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )
        notify(
            context,
            context.getString(R.string.update_available_title, version),
            context.getString(R.string.update_available_body),
            install,
        )
    }

    /** After an update: say so (the service is started again by [UpdateReceiver]). */
    internal fun notifyUpdated(context: Context) {
        val open = PendingIntent.getActivity(
            context,
            3,
            Intent(context, MainActivity::class.java),
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )
        notify(context, context.getString(R.string.update_done_title, currentVersion(context)), null, open)
    }

    private fun notify(context: Context, title: String, body: String?, tap: PendingIntent) {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU &&
            ContextCompat.checkSelfPermission(context, Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED
        ) {
            return
        }
        val notification = NotificationCompat.Builder(context, Notifications.CHANNEL_UPDATES)
            .setSmallIcon(R.drawable.ic_notification)
            .setContentTitle(title)
            .setContentText(body)
            .setContentIntent(tap)
            .setAutoCancel(true)
            .build()
        NotificationManagerCompat.from(context).notify(NOTIFICATION_ID, notification)
    }

    /** `1.2.3` newer than `1.2.2`? */
    fun newer(candidate: String, current: String): Boolean {
        fun parts(v: String) = v.removePrefix("v").split('.').map { it.takeWhile(Char::isDigit).toIntOrNull() ?: 0 }
        val a = parts(candidate)
        val b = parts(current)
        for (i in 0 until maxOf(a.size, b.size)) {
            val x = a.getOrElse(i) { 0 }
            val y = b.getOrElse(i) { 0 }
            if (x != y) return x > y
        }
        return false
    }
}

/** The installer's answers, the notification's Update button, and "we were just updated". */
class UpdateReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        when (intent.action) {
            Intent.ACTION_MY_PACKAGE_REPLACED -> {
                Updater.notifyUpdated(context)
                PairlyService.start(context)
            }
            Updater.ACTION_INSTALL -> {
                val pending = goAsync()
                CoroutineScope(Dispatchers.IO).launch {
                    try {
                        Updater.check(context.applicationContext, install = true)
                    } finally {
                        pending.finish()
                    }
                }
            }
            else -> Updater.onInstallStatus(context, intent)
        }
    }
}
