package dev.pairly.android.device

import android.app.Activity
import android.content.ClipboardManager
import android.os.Build
import android.os.Bundle
import dev.pairly.android.Pairly

/**
 * Invisible, momentary activity: Android lets apps read the clipboard only while they have
 * focus, so the Quick Settings tile and the notification action open this to send it, and so
 * does [PairlyAccessibility] when it notices a copy ([EXTRA_AUTO]: quietly, and only new text).
 */
class ClipboardActivity : Activity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            overrideActivityTransition(OVERRIDE_TRANSITION_OPEN, 0, 0)
            overrideActivityTransition(OVERRIDE_TRANSITION_CLOSE, 0, 0)
        } else {
            @Suppress("DEPRECATION")
            overridePendingTransition(0, 0)
        }
    }

    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        if (!hasFocus) return
        val clip = getSystemService(ClipboardManager::class.java)?.primaryClip
        val text = clip?.takeIf { it.itemCount > 0 }?.getItemAt(0)?.coerceToText(this)?.toString()
        if (intent.getBooleanExtra(EXTRA_AUTO, false)) {
            if (!text.isNullOrEmpty() && text != ClipboardSync.last) {
                ClipboardSync.last = text
                Pairly.sendClipboardQuietly(text)
            }
        } else {
            ClipboardSync.last = text
            Pairly.sendClipboardToAll(text)
        }
        finish()
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            @Suppress("DEPRECATION")
            overridePendingTransition(0, 0)
        }
    }

    companion object {
        const val EXTRA_AUTO = "auto"
    }
}
