package dev.pairly.android.screen

import android.Manifest
import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.net.nsd.NsdManager
import android.net.nsd.NsdServiceInfo
import android.os.Build
import android.os.Process
import android.provider.Settings
import android.util.Base64
import android.util.Log
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import androidx.core.app.RemoteInput
import androidx.core.content.ContextCompat
import androidx.core.content.edit
import dev.pairly.android.Notifications
import dev.pairly.android.R
import dev.pairly.core.ffi.adbPair
import dev.pairly.core.ffi.adbShell
import java.io.File
import java.math.BigInteger
import java.net.InetAddress
import java.net.NetworkInterface
import java.nio.ByteBuffer
import java.nio.ByteOrder
import java.security.KeyPairGenerator
import java.security.interfaces.RSAPublicKey
import java.security.spec.RSAKeyGenParameterSpec
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit
import kotlin.concurrent.thread

/**
 * Starting the screen helper ([dev.pairly.android.helper.Helper]) from the phone itself, the
 * way Shizuku does: Pairly pairs once with Android's wireless debugging (the user types the
 * 6-digit code into a notification), then runs the helper through it as the shell user.
 *
 * The helper also grants Pairly "write secure settings", so after a restart Pairly can switch
 * wireless debugging on just long enough to start it again (on Wi-Fi), then off.
 */
object WirelessDebugging {
    private const val TAG = "WirelessDebugging"
    private const val PAIRING_SERVICE = "_adb-tls-pairing._tcp"
    private const val CONNECT_SERVICE = "_adb-tls-connect._tcp"
    /** Settings.Global: wireless debugging on (1) or off (0). */
    private const val ADB_WIFI = "adb_wifi_enabled"
    private const val HELPER = "dev.pairly.android.helper.Helper"
    private const val PREFS = "wireless_debugging"
    private const val KEY_PAIRED = "paired"
    private const val KEY_CODE = "code"
    private const val NOTIFICATION_ID = 47

    /** Paired before (the helper can be started without a PC). */
    fun paired(context: Context): Boolean =
        context.getSharedPreferences(PREFS, Context.MODE_PRIVATE).getBoolean(KEY_PAIRED, false) &&
            File(keyDir(context), "key.pk8").exists()

    /** Start pairing: ask for the code in a notification and open Developer options. */
    fun startPairing(context: Context) {
        val input = RemoteInput.Builder(KEY_CODE).setLabel(context.getString(R.string.helper_pair_label)).build()
        val reply = PendingIntent.getBroadcast(
            context,
            0,
            Intent(context, CodeReceiver::class.java),
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_MUTABLE,
        )
        val action = NotificationCompat.Action.Builder(0, context.getString(R.string.helper_pair_action), reply)
            .addRemoteInput(input)
            .build()
        notify(
            context,
            NotificationCompat.Builder(context, Notifications.CHANNEL_SCREEN)
                .setSmallIcon(R.drawable.ic_notification)
                .setContentTitle(context.getString(R.string.helper_pair_title))
                .setContentText(context.getString(R.string.helper_pair_body))
                .setStyle(NotificationCompat.BigTextStyle().bigText(context.getString(R.string.helper_pair_body)))
                .setPriority(NotificationCompat.PRIORITY_HIGH)
                .addAction(action),
        )
        val developer = Intent(Settings.ACTION_APPLICATION_DEVELOPMENT_SETTINGS).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        runCatching { context.startActivity(developer) }
            .onFailure { runCatching { context.startActivity(Intent(Settings.ACTION_SETTINGS).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)) } }
    }

    /** The code typed into the notification. */
    class CodeReceiver : BroadcastReceiver() {
        override fun onReceive(context: Context, intent: Intent) {
            val code = RemoteInput.getResultsFromIntent(intent)?.getCharSequence(KEY_CODE)?.filter(Char::isDigit)?.toString()
            if (code.isNullOrEmpty()) return
            val done = goAsync()
            thread(name = "pairly-adb-pair") {
                try {
                    pair(context.applicationContext, code)
                } finally {
                    done.finish()
                }
            }
        }
    }

    private fun pair(context: Context, code: String) {
        status(context, context.getString(R.string.helper_pairing), null)
        val port = findPort(context, PAIRING_SERVICE, 5_000)
        if (port == null) {
            status(context, context.getString(R.string.helper_pair_failed_title), context.getString(R.string.helper_no_pairing))
            return
        }
        val (key, public) = key(context)
        val failed = runCatching { adbPair(port.toUShort(), code, key, public) }.exceptionOrNull()
        if (failed != null) {
            Log.w(TAG, "pairing failed", failed)
            status(context, context.getString(R.string.helper_pair_failed_title), failed.message.orEmpty())
            return
        }
        context.getSharedPreferences(PREFS, Context.MODE_PRIVATE).edit { putBoolean(KEY_PAIRED, true) }
        startHelper(context)
        status(context, context.getString(R.string.helper_paired_title), context.getString(R.string.helper_paired_body))
    }

    /**
     * Start the helper through wireless debugging, switching it on for the moment if it's off
     * (and Pairly may). True when the helper was started.
     */
    @Synchronized
    fun startHelper(context: Context): Boolean {
        if (!paired(context)) return false
        val resolver = context.contentResolver
        val wasOn = Settings.Global.getInt(resolver, ADB_WIFI, 0) == 1
        if (!wasOn) {
            val may = context.checkSelfPermission(Manifest.permission.WRITE_SECURE_SETTINGS) == PackageManager.PERMISSION_GRANTED
            if (!may || runCatching { Settings.Global.putInt(resolver, ADB_WIFI, 1) }.isFailure) return false
        }
        try {
            // Wireless debugging needs a moment (and Wi-Fi) to start listening, and on a network
            // not allowed before, Android asks first ("Allow wireless debugging on this
            // network?"): time to tap Allow.
            val port = findPort(context, CONNECT_SERVICE, if (wasOn) 3_000 else 30_000) ?: return false
            adbShell(port.toUShort(), key(context).first, helperCommand(context))
            Log.i(TAG, "started the screen helper through wireless debugging")
            return true
        } catch (e: Exception) {
            Log.w(TAG, "couldn't start the screen helper", e)
            return false
        } finally {
            // The helper keeps running; wireless debugging goes back to how it was.
            if (!wasOn) runCatching { Settings.Global.putInt(resolver, ADB_WIFI, 0) }
        }
    }

    /**
     * Replaces a helper already running (maybe an older one) and grants Pairly the setting.
     * Not `pkill -f` to stop the old one: that matches the shell running this very command (its
     * command line names the helper too) and kills it before the helper starts.
     */
    private fun helperCommand(context: Context): String {
        val apk = context.applicationInfo.sourceDir
        val stop = "for p in \$(pidof app_process); do " +
            "case \"\$(tr \"\\0\" \" \" < /proc/\$p/cmdline)\" in *$HELPER*) kill \$p;; esac; done; "
        return "pm grant ${context.packageName} ${Manifest.permission.WRITE_SECURE_SETTINGS}; " +
            stop + "sleep 0.3; " +
            "CLASSPATH='$apk' setsid app_process / $HELPER --uid ${Process.myUid()} </dev/null >/dev/null 2>&1 & " +
            // The plain "shell:" service kills the command's process group when it ends: wait
            // for setsid to have moved the helper out of it first.
            "sleep 1"
    }

    /**
     * The port of this phone's own wireless debugging service of `type`, found the way Android
     * announces it (DNS-SD on the network), or null after `timeoutMs`.
     */
    @Suppress("DEPRECATION") // resolveService: its replacement needs Android 14
    private fun findPort(context: Context, type: String, timeoutMs: Long): Int? {
        val nsd = context.getSystemService(NsdManager::class.java) ?: return null
        val mine: Set<InetAddress> = runCatching {
            NetworkInterface.getNetworkInterfaces().toList().flatMap { it.inetAddresses.toList() }.toSet()
        }.getOrDefault(emptySet())
        val found = LinkedBlockingQueue<Int>()
        val listener = object : NsdManager.DiscoveryListener {
            override fun onServiceFound(info: NsdServiceInfo) {
                nsd.resolveService(
                    info,
                    object : NsdManager.ResolveListener {
                        override fun onServiceResolved(resolved: NsdServiceInfo) {
                            // Other phones on the network announce theirs too.
                            if (resolved.host in mine) found.offer(resolved.port)
                        }

                        override fun onResolveFailed(info: NsdServiceInfo, error: Int) {}
                    },
                )
            }

            override fun onDiscoveryStarted(type: String) {}
            override fun onDiscoveryStopped(type: String) {}
            override fun onServiceLost(info: NsdServiceInfo) {}
            override fun onStartDiscoveryFailed(type: String, error: Int) {}
            override fun onStopDiscoveryFailed(type: String, error: Int) {}
        }
        runCatching { nsd.discoverServices(type, NsdManager.PROTOCOL_DNS_SD, listener) }.onFailure { return null }
        return try {
            found.poll(timeoutMs, TimeUnit.MILLISECONDS)
        } finally {
            runCatching { nsd.stopServiceDiscovery(listener) }
        }
    }

    private fun keyDir(context: Context) = File(context.filesDir, "adb")

    /** Pairly's adb key, made once: the private key (PKCS#8) and the public key in adb's format. */
    @Synchronized
    private fun key(context: Context): Pair<ByteArray, String> {
        val dir = keyDir(context).apply { mkdirs() }
        val private = File(dir, "key.pk8")
        val public = File(dir, "key.pub")
        if (!private.exists() || !public.exists()) {
            val pair = KeyPairGenerator.getInstance("RSA").apply {
                initialize(RSAKeyGenParameterSpec(2048, RSAKeyGenParameterSpec.F4))
            }.generateKeyPair()
            val name = "pairly@" + Build.MODEL.replace(Regex("\\s+"), "_")
            public.writeText(Base64.encodeToString(adbPublicKey(pair.public as RSAPublicKey), Base64.NO_WRAP) + " " + name)
            private.writeBytes(pair.private.encoded)
        }
        return private.readBytes() to public.readText()
    }

    /**
     * adb's RSA public key (`android_pubkey_encode`): words in the modulus, -1/n mod 2^32, the
     * modulus and R² mod n (R = 2^2048) little-endian, and the exponent.
     */
    private fun adbPublicKey(key: RSAPublicKey): ByteArray {
        val n = key.modulus
        val r32 = BigInteger.ONE.shiftLeft(32)
        val n0inv = r32.subtract(n.mod(r32).modInverse(r32))
        val rr = BigInteger.ONE.shiftLeft(4096).mod(n)
        return ByteBuffer.allocate(524).order(ByteOrder.LITTLE_ENDIAN)
            .putInt(64)
            .putInt(n0inv.toInt())
            .put(littleEndian(n))
            .put(littleEndian(rr))
            .putInt(key.publicExponent.toInt())
            .array()
    }

    private fun littleEndian(v: BigInteger): ByteArray {
        val big = v.toByteArray()
        return ByteArray(256) { i -> big.getOrElse(big.size - 1 - i) { 0 } }
    }

    private fun status(context: Context, title: String, body: String?) = notify(
        context,
        NotificationCompat.Builder(context, Notifications.CHANNEL_SCREEN)
            .setSmallIcon(R.drawable.ic_notification)
            .setContentTitle(title)
            .setContentText(body)
            .setStyle(body?.let { NotificationCompat.BigTextStyle().bigText(it) })
            .setAutoCancel(true),
    )

    private fun notify(context: Context, builder: NotificationCompat.Builder) {
        val allowed = Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU ||
            ContextCompat.checkSelfPermission(context, Manifest.permission.POST_NOTIFICATIONS) == PackageManager.PERMISSION_GRANTED
        if (allowed) NotificationManagerCompat.from(context).notify(NOTIFICATION_ID, builder.build())
    }
}
