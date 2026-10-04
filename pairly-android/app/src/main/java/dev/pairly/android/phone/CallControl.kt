package dev.pairly.android.phone

import android.Manifest
import android.annotation.SuppressLint
import android.content.Context
import android.content.pm.PackageManager
import android.media.AudioDeviceInfo
import android.media.AudioManager
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.provider.ContactsContract
import android.telecom.TelecomManager
import android.util.Log
import androidx.core.content.ContextCompat
import dev.pairly.android.Pairly
import dev.pairly.android.R
import dev.pairly.android.device.PairlyAccessibility
import dev.pairly.core.ffi.CallActionData
import dev.pairly.core.ffi.ContactData
import dev.pairly.core.ffi.ContactsHandler
import dev.pairly.core.ffi.PairlyException
import dev.pairly.core.ffi.TelephonyHandler

/** Answers, rejects, hangs up and places calls when a paired PC asks. */
class CallControl(context: Context) : TelephonyHandler {
    private val context = context.applicationContext
    private val main = Handler(Looper.getMainLooper())

    private fun granted(permission: String) =
        ContextCompat.checkSelfPermission(context, permission) == PackageManager.PERMISSION_GRANTED

    @SuppressLint("MissingPermission") // checked first
    override fun control(fromName: String, action: CallActionData) {
        val telecom = context.getSystemService(TelecomManager::class.java) ?: return
        if (!granted(Manifest.permission.ANSWER_PHONE_CALLS)) {
            Pairly.say(context.getString(R.string.calls_need_permission))
            return
        }
        main.post {
            runCatching {
                when (action) {
                    CallActionData.ANSWER -> telecom.acceptRingingCall()
                    CallActionData.ANSWER_ON_SPEAKER -> {
                        telecom.acceptRingingCall()
                        // The call needs a moment to become active before audio can be routed.
                        main.postDelayed(::speaker, SPEAKER_DELAY_MS)
                    }
                    CallActionData.REJECT, CallActionData.HANG_UP -> {
                        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) telecom.endCall()
                    }
                }
            }.onFailure { Log.w(TAG, "call action failed", it) }
        }
    }

    /**
     * Only the dialer may route call audio (the system ignores other apps asking for the
     * speaker), so press its Speaker button through Pairly's accessibility service; without it,
     * ask the audio system anyway, which works on some phones.
     */
    private fun speaker() {
        val service = PairlyAccessibility.instance
        if (service != null) {
            Thread {
                if (!service.pressSpeaker()) {
                    Log.w(TAG, "couldn't find the dialer's Speaker button")
                    main.post(::askAudioForSpeaker)
                }
            }.start()
        } else {
            askAudioForSpeaker()
        }
    }

    private fun askAudioForSpeaker() {
        val audio = context.getSystemService(AudioManager::class.java) ?: return
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            audio.availableCommunicationDevices
                .firstOrNull { it.type == AudioDeviceInfo.TYPE_BUILTIN_SPEAKER }
                ?.let(audio::setCommunicationDevice)
        } else {
            @Suppress("DEPRECATION")
            audio.isSpeakerphoneOn = true
        }
    }

    @SuppressLint("MissingPermission") // checked first
    override fun dial(fromName: String, number: String) {
        if (!granted(Manifest.permission.CALL_PHONE)) {
            Pairly.say(context.getString(R.string.calls_need_permission))
            return
        }
        val telecom = context.getSystemService(TelecomManager::class.java) ?: return
        main.post {
            runCatching { telecom.placeCall(Uri.fromParts("tel", number, null), Bundle()) }
                .onFailure { Log.w(TAG, "can't place the call", it) }
        }
    }

    companion object {
        private const val TAG = "CallControl"
        private const val SPEAKER_DELAY_MS = 500L
    }
}

/** The phone's contacts for paired PCs (names and numbers). */
class PhoneContacts(context: Context) : ContactsHandler {
    private val context = context.applicationContext

    override fun contacts(): List<ContactData> {
        if (ContextCompat.checkSelfPermission(context, Manifest.permission.READ_CONTACTS) !=
            PackageManager.PERMISSION_GRANTED
        ) {
            throw PairlyException.Failed("Allow Pairly to read contacts on the phone first")
        }
        val byId = LinkedHashMap<Long, Pair<String, MutableList<String>>>()
        val columns = arrayOf(
            ContactsContract.CommonDataKinds.Phone.CONTACT_ID,
            ContactsContract.CommonDataKinds.Phone.DISPLAY_NAME,
            ContactsContract.CommonDataKinds.Phone.NUMBER,
        )
        context.contentResolver.query(
            ContactsContract.CommonDataKinds.Phone.CONTENT_URI,
            columns,
            null,
            null,
            "${ContactsContract.CommonDataKinds.Phone.DISPLAY_NAME} ASC",
        )?.use { c ->
            while (c.moveToNext()) {
                val number = c.getString(2)?.trim().orEmpty()
                if (number.isEmpty()) continue
                val entry = byId.getOrPut(c.getLong(0)) { (c.getString(1).orEmpty()) to mutableListOf() }
                // The same number is often stored twice (with and without the country code).
                if (entry.second.none { it.filter(Char::isDigit).endsWith(number.filter(Char::isDigit).takeLast(9)) }) {
                    entry.second += number
                }
            }
        }
        return byId.values.map { (name, numbers) -> ContactData(name.ifEmpty { numbers.first() }, numbers) }
    }
}
