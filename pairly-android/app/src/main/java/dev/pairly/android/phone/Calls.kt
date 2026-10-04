package dev.pairly.android.phone

import android.Manifest
import android.content.Context
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Handler
import android.os.Looper
import android.provider.ContactsContract
import android.telephony.PhoneStateListener
import android.telephony.TelephonyManager
import android.util.Log
import androidx.core.content.ContextCompat
import dev.pairly.android.Pairly
import dev.pairly.core.ffi.CallStateData

/**
 * Reports the phone's calls to paired PCs: ringing, answered, missed and ended.
 *
 * Uses the (deprecated but still working) PhoneStateListener because it is the only API that
 * passes the caller's number, given READ_CALL_LOG.
 */
object Calls {
    private const val TAG = "Calls"

    private var telephony: TelephonyManager? = null
    private var lastState = TelephonyManager.CALL_STATE_IDLE
    private var rang = false
    private var number: String? = null

    fun permitted(context: Context): Boolean = PERMISSIONS.all {
        ContextCompat.checkSelfPermission(context, it) == PackageManager.PERMISSION_GRANTED
    }

    val PERMISSIONS = arrayOf(
        Manifest.permission.READ_PHONE_STATE,
        Manifest.permission.READ_CALL_LOG,
        Manifest.permission.READ_CONTACTS,
        Manifest.permission.ANSWER_PHONE_CALLS,
        Manifest.permission.CALL_PHONE,
    )

    @Suppress("DEPRECATION")
    private val listener = object : PhoneStateListener() {
        @Deprecated("Deprecated in Java")
        override fun onCallStateChanged(state: Int, phoneNumber: String?) {
            if (!phoneNumber.isNullOrBlank()) number = phoneNumber
            changed(state)
        }
    }

    /** Start listening (no-op without the permissions). Main thread. */
    @Suppress("DEPRECATION")
    fun start(context: Context) {
        // Listening needs only the phone state; the other permissions add numbers and control.
        if (telephony != null ||
            ContextCompat.checkSelfPermission(context, Manifest.permission.READ_PHONE_STATE) !=
            PackageManager.PERMISSION_GRANTED
        ) {
            return
        }
        val tm = context.getSystemService(TelephonyManager::class.java) ?: return
        appContext = context.applicationContext
        try {
            Handler(Looper.getMainLooper()).post { tm.listen(listener, PhoneStateListener.LISTEN_CALL_STATE) }
            telephony = tm
        } catch (e: SecurityException) {
            Log.w(TAG, "no permission to watch calls", e)
        }
    }

    @Suppress("DEPRECATION")
    fun stop() {
        telephony?.listen(listener, PhoneStateListener.LISTEN_NONE)
        telephony = null
    }

    private lateinit var appContext: Context

    private fun changed(state: Int) {
        val previous = lastState
        lastState = state
        val event = when (state) {
            TelephonyManager.CALL_STATE_RINGING -> {
                rang = true
                CallStateData.RINGING
            }
            TelephonyManager.CALL_STATE_OFFHOOK -> {
                rang = false
                CallStateData.TALKING
            }
            else -> when (previous) {
                TelephonyManager.CALL_STATE_RINGING -> if (rang) CallStateData.MISSED else CallStateData.ENDED
                TelephonyManager.CALL_STATE_OFFHOOK -> CallStateData.ENDED
                else -> null
            }
        } ?: return
        val num = number
        Pairly.callChanged(event, num, num?.let { contactName(appContext, it) })
        if (state == TelephonyManager.CALL_STATE_IDLE) {
            number = null
            rang = false
        }
    }

    /** The contact's name for a number, if the phone knows it. */
    fun contactName(context: Context, number: String): String? {
        if (ContextCompat.checkSelfPermission(context, Manifest.permission.READ_CONTACTS) !=
            PackageManager.PERMISSION_GRANTED
        ) {
            return null
        }
        val uri = Uri.withAppendedPath(ContactsContract.PhoneLookup.CONTENT_FILTER_URI, Uri.encode(number))
        return runCatching {
            context.contentResolver.query(uri, arrayOf(ContactsContract.PhoneLookup.DISPLAY_NAME), null, null, null)
                ?.use { c -> if (c.moveToFirst()) c.getString(0) else null }
        }.getOrNull()
    }
}
