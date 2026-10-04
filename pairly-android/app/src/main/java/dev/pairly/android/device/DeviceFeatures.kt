package dev.pairly.android.device

import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.os.BatteryManager
import android.os.Handler
import android.os.Looper
import dev.pairly.android.Pairly
import dev.pairly.android.R
import dev.pairly.core.ffi.BatteryData
import dev.pairly.core.ffi.DeviceHandler

/** Clipboard, find my phone and battery for the Rust core. */
class DeviceFeatures(context: Context) : DeviceHandler {
    private val context = context.applicationContext
    private val main = Handler(Looper.getMainLooper())

    override fun setClipboard(fromId: String, fromName: String, text: String) {
        main.post {
            context.getSystemService(ClipboardManager::class.java)
                ?.setPrimaryClip(ClipData.newPlainText(context.getString(R.string.app_name), text))
            Pairly.say(context.getString(R.string.clipboard_received, fromName))
        }
    }

    override fun ring(fromId: String, fromName: String, on: Boolean) {
        if (on) Ringer.start(context, fromName) else Ringer.stop()
    }

    override fun battery(): BatteryData? = current(context)

    override fun peerBattery(fromId: String, fromName: String, state: BatteryData, previous: BatteryData?) {
        Pairly.devicesChanged()
    }

    companion object {
        fun current(context: Context): BatteryData? {
            val status = context.registerReceiver(null, IntentFilter(Intent.ACTION_BATTERY_CHANGED)) ?: return null
            return fromIntent(status)
        }

        fun fromIntent(intent: Intent): BatteryData? {
            val level = intent.getIntExtra(BatteryManager.EXTRA_LEVEL, -1)
            val scale = intent.getIntExtra(BatteryManager.EXTRA_SCALE, 100)
            if (level < 0 || scale <= 0) return null
            val plugged = intent.getIntExtra(BatteryManager.EXTRA_PLUGGED, 0) != 0
            return BatteryData((level * 100 / scale).coerceIn(0, 100).toUByte(), plugged)
        }
    }
}
