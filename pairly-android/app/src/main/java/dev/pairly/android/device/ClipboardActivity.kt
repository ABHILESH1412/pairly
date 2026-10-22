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
        val copied = ClipboardSync.read(this, getSystemService(ClipboardManager::class.java)?.primaryClip)
        if (intent.getBooleanExtra(EXTRA_AUTO, false)) {
            if (copied != null && ClipboardSync.key(copied) != ClipboardSync.last) {
                ClipboardSync.last = ClipboardSync.key(copied)
                Pairly.sendClipboardQuietly(copied)
            }
        } else {
            ClipboardSync.last = copied?.let(ClipboardSync::key)
            Pairly.sendClipboardToAll(copied)
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
