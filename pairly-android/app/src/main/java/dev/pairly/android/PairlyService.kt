package dev.pairly.android

import android.app.Service
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.content.pm.ServiceInfo
import android.net.ConnectivityManager
import android.net.LinkProperties
import android.net.Network
import android.net.NetworkRequest
import android.net.wifi.WifiManager
import android.os.Build
import android.os.IBinder
import androidx.core.app.ServiceCompat
import androidx.core.content.ContextCompat
import dev.pairly.android.device.DeviceFeatures
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.launch

/** Keeps the node (and its connections) alive while the app is in the background. */
class PairlyService : Service() {
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main.immediate)
    private var multicastLock: WifiManager.MulticastLock? = null
    private val batteryReceiver = object : BroadcastReceiver() {
        override fun onReceive(context: Context, intent: Intent) {
            DeviceFeatures.fromIntent(intent)?.let(Pairly::batteryChanged)
        }
    }
    private val networkCallback = object : ConnectivityManager.NetworkCallback() {
        override fun onAvailable(network: Network) = Pairly.networkChanged()
        override fun onLost(network: Network) = Pairly.networkChanged()
        override fun onLinkPropertiesChanged(network: Network, linkProperties: LinkProperties) =
            Pairly.networkChanged()
    }

    override fun onCreate() {
        super.onCreate()
        val type = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            ServiceInfo.FOREGROUND_SERVICE_TYPE_CONNECTED_DEVICE
        } else {
            0
        }
        ServiceCompat.startForeground(this, Notifications.SERVICE_ID, Notifications.service(this, emptyList()), type)
        // Many phones drop inbound multicast (mDNS) unless an app holds this lock.
        multicastLock = getSystemService(WifiManager::class.java)
            ?.createMulticastLock("pairly-mdns")
            ?.apply {
                setReferenceCounted(false)
                acquire()
            }
        getSystemService(ConnectivityManager::class.java)
            ?.registerNetworkCallback(NetworkRequest.Builder().build(), networkCallback)
        ContextCompat.registerReceiver(
            this,
            batteryReceiver,
            IntentFilter(Intent.ACTION_BATTERY_CHANGED),
            ContextCompat.RECEIVER_NOT_EXPORTED,
        )
        scope.launch { Pairly.start(applicationContext) }
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int = START_STICKY

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onDestroy() {
        getSystemService(ConnectivityManager::class.java)?.unregisterNetworkCallback(networkCallback)
        unregisterReceiver(batteryReceiver)
        scope.cancel()
        Pairly.stop()
        multicastLock?.release()
        super.onDestroy()
    }

    companion object {
        fun start(context: Context) {
            ContextCompat.startForegroundService(context, Intent(context, PairlyService::class.java))
        }
    }
}
