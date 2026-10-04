package dev.pairly.android.device

import android.accessibilityservice.AccessibilityService
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.provider.Settings
import android.util.Log
import android.view.accessibility.AccessibilityEvent
import android.view.accessibility.AccessibilityNodeInfo
import android.view.accessibility.AccessibilityWindowInfo
import dev.pairly.core.ffi.PairlyException
import dev.pairly.core.ffi.PowerActionData
import dev.pairly.core.ffi.PowerHandler

/**
 * Pairly's accessibility service, which does two things only:
 *
 * - Locks the screen or powers the phone off / restarts it when a paired PC asks. Android lets
 *   only accessibility services lock the screen without device-admin rights, and powering off is
 *   done by opening the power menu and pressing its button.
 * - Notices when something is copied and sends it to connected PCs. Apps can't read the
 *   clipboard in the background, so the text is taken from what's on screen, with nothing
 *   opening: the text that was selected when "Copy" was pressed, or the system's "Copied"
 *   overlay's preview (shown when the keyboard is closed). Only when neither has the whole
 *   text (long or hidden text, a copy button with nothing selected) does [ClipboardActivity]
 *   read the clipboard: a bound accessibility service may briefly open an activity, which may.
 * - Presses the dialer's Speaker button when a PC answers a call on speaker: only the dialer
 *   may route call audio.
 *
 * It reads no screen content beyond finding those buttons and overlays.
 */
class PairlyAccessibility : AccessibilityService() {
    private val main = Handler(Looper.getMainLooper())
    private var lastCopyCheck = 0L
    /** The "Copied" overlay was up at the last look: only its appearing counts as a copy. */
    private var overlayUp = false
    /** When the overlay last appeared, so a "Copy" click it covers isn't read twice. */
    private var overlayAt = 0L
    /** When a copy was last sent from the selection: the overlay that follows is the same copy. */
    private var sentAt = 0L

    /** The latest non-empty text selection, and while it lasted. */
    private class Selection(val pkg: String, val text: String) {
        var active = true
        var endedAt = 0L
    }
    private var selection: Selection? = null

    override fun onServiceConnected() {
        instance = this
    }

    override fun onUnbind(intent: Intent?): Boolean {
        instance = null
        return super.onUnbind(intent)
    }

    override fun onDestroy() {
        instance = null
        super.onDestroy()
    }

    override fun onInterrupt() {}

    override fun onAccessibilityEvent(event: AccessibilityEvent) {
        if (!ClipboardSync.auto(this)) return
        val type = event.eventType
        if (type == AccessibilityEvent.TYPE_VIEW_TEXT_SELECTION_CHANGED) {
            noteSelection(event)
        } else if (type == AccessibilityEvent.TYPE_VIEW_CLICKED || type == AccessibilityEvent.TYPE_VIEW_LONG_CLICKED) {
            if (saysCopy(event.text?.joinToString(" ")) || saysCopy(event.contentDescription)) {
                val clickedAt = SystemClock.elapsedRealtime()
                if (sendSelection()) {
                    sentAt = clickedAt
                } else {
                    // The overlay normally follows; only without it read the clipboard directly.
                    main.postDelayed({ if (overlayAt < clickedAt) readClipboard() }, NO_OVERLAY_MS)
                }
            }
        } else if (event.packageName == SYSTEM_UI) {
            // A window came or changed (we only subscribe to those): is it the "Copied" overlay?
            main.postDelayed({
                val overlay = clipboardOverlay()
                if (overlay != null && !overlayUp) {
                    overlayAt = SystemClock.elapsedRealtime()
                    if (overlayAt - sentAt > SAME_COPY_MS) fromOverlay(overlay, retry = true)
                }
                overlayUp = overlay != null
            }, OVERLAY_DELAY_MS)
        }
    }

    /** Remember what's selected (never in password fields), for the "Copy" that may follow. */
    private fun noteSelection(event: AccessibilityEvent) {
        val text = event.text?.firstOrNull()?.toString()
        val from = minOf(event.fromIndex, event.toIndex)
        val to = maxOf(event.fromIndex, event.toIndex)
        if (event.isPassword || text == null || from < 0 || to > text.length) {
            selection = null
        } else if (to > from) {
            selection = Selection(event.packageName?.toString().orEmpty(), text.substring(from, to))
        } else {
            // Collapsed (often by the Copy itself, just before the overlay shows): keep it briefly.
            selection?.takeIf { it.active }?.let {
                it.active = false
                it.endedAt = SystemClock.elapsedRealtime()
            }
        }
    }

    /** Send the selected text, if a selection in the app on screen is (or just was) current. */
    private fun sendSelection(): Boolean {
        val sel = selection ?: return false
        val recent = sel.active || SystemClock.elapsedRealtime() - sel.endedAt < SELECTION_GRACE_MS
        if (!recent || rootInActiveWindow?.packageName?.toString() != sel.pkg) return false
        Log.i(TAG, "copy: sending the selection (${sel.text.length} chars)")
        send(sel.text)
        return true
    }

    private fun send(text: String) {
        if (text != ClipboardSync.last) {
            ClipboardSync.last = text
            dev.pairly.android.Pairly.sendClipboardQuietly(text)
        }
    }

    /** Send the overlay's preview text, or read the clipboard if the preview isn't all of it. */
    private fun fromOverlay(overlay: AccessibilityNodeInfo, retry: Boolean) {
        val preview = findById(overlay, TEXT_PREVIEW, 0)?.takeIf { it.isVisibleToUser }
        val text = preview?.text?.toString()
        Log.i(TAG, "copy seen: preview ${if (preview == null) "missing" else "${text?.length ?: 0} chars"}")
        when {
            text != null && text.isNotEmpty() && text.length < PREVIEW_LIMIT -> send(text)
            // No preview (the keyboard is up, so the overlay is just an icon): what was selected.
            sendSelection() -> {}
            // The preview may still be filling in.
            preview == null && retry -> main.postDelayed({
                clipboardOverlay()?.let { fromOverlay(it, retry = false) } ?: readClipboard()
            }, OVERLAY_DELAY_MS * 2)
            // Long (cut short), hidden (a password) or not text: ask the clipboard itself.
            findById(overlay, IMAGE_PREVIEW, 0)?.isVisibleToUser != true -> readClipboard()
        }
    }

    /** Read the clipboard through [ClipboardActivity] (once, however many signs of a copy arrive). */
    private fun readClipboard() {
        Log.i(TAG, "copy: reading the clipboard through the activity")
        val now = SystemClock.elapsedRealtime()
        if (now - lastCopyCheck < COPY_DEBOUNCE_MS) return
        lastCopyCheck = now
        main.postDelayed({
            runCatching {
                startActivity(
                    Intent(this, ClipboardActivity::class.java)
                        .putExtra(ClipboardActivity.EXTRA_AUTO, true)
                        .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_NO_ANIMATION),
                )
            }
        }, READ_DELAY_MS)
    }

    /** The system's "Copied" overlay, if it's showing. */
    private fun clipboardOverlay(): AccessibilityNodeInfo? = windows.firstNotNullOfOrNull { w ->
        w.root?.takeIf { it.packageName == SYSTEM_UI && findById(it, "clipboard", 0) != null }
    }

    /** The first node whose view id contains `part`. */
    private fun findById(node: AccessibilityNodeInfo, part: String, depth: Int): AccessibilityNodeInfo? {
        if (node.viewIdResourceName?.contains(part) == true) return node
        if (depth >= MAX_DEPTH) return null
        for (i in 0 until node.childCount) {
            val child = node.getChild(i) ?: continue
            findById(child, part, depth + 1)?.let { return it }
        }
        return null
    }

    // ----- calls -----------------------------------------------------------------------------

    /**
     * Put the call that was just answered on speaker by pressing the dialer's Speaker button
     * (through its audio-output menu when a Bluetooth device is connected). Runs on a background
     * thread; true once speaker is on.
     */
    fun pressSpeaker(): Boolean {
        val deadline = SystemClock.elapsedRealtime() + SPEAKER_WAIT_MS
        while (SystemClock.elapsedRealtime() < deadline) {
            Thread.sleep(POLL_MS)
            val speaker = find(SPEAKER_LABELS)
            if (speaker != null) {
                Log.i(TAG, "speaker button found (on: ${checked(speaker)})")
                if (checked(speaker)) return true
                if (click(speaker)) return true
                continue
            }
            // With a headset connected, Speaker is inside an "Audio" menu.
            val menu = find { it.startsWith("audio", ignoreCase = true) } ?: continue
            Log.i(TAG, "opening the audio menu for the speaker")
            if (click(menu)) {
                Thread.sleep(MENU_WAIT_MS)
                find(SPEAKER_LABELS)?.let { if (click(it)) return true }
            }
        }
        return false
    }

    /** The node, or the button it's in, is switched on. */
    private fun checked(node: AccessibilityNodeInfo): Boolean {
        var n: AccessibilityNodeInfo? = node
        while (n != null) {
            if (n.isCheckable) return n.isChecked
            if (n.isClickable) return n.isSelected
            n = n.parent
        }
        return false
    }

    // ----- power -----------------------------------------------------------------------------

    fun lock() {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.P) throw PairlyException.Failed("locking needs Android 9 or newer")
        if (!performGlobalAction(GLOBAL_ACTION_LOCK_SCREEN)) throw PairlyException.Failed("the phone refused to lock")
    }

    /**
     * Open the power menu and press the button labelled one of `labels` (again, if the phone
     * asks to confirm). Runs on a background thread; waits a few seconds at most.
     */
    fun pressInPowerMenu(labels: List<String>, what: String) {
        if (!performGlobalAction(GLOBAL_ACTION_POWER_DIALOG)) throw PairlyException.Failed("couldn't open the power menu")
        val deadline = SystemClock.elapsedRealtime() + POWER_MENU_WAIT_MS
        var pressed = 0
        while (SystemClock.elapsedRealtime() < deadline) {
            Thread.sleep(POLL_MS)
            val button = find(labels) ?: if (pressed > 0) return else continue
            if (!click(button)) continue
            pressed++
            if (pressed >= 2) return
            // Some phones ask again ("Tap to power off"); give it a moment to show.
            Thread.sleep(CONFIRM_WAIT_MS)
        }
        if (pressed == 0) {
            throw PairlyException.Failed("couldn't find the $what button; the power menu is open on the phone")
        }
    }

    private fun find(labels: List<String>): AccessibilityNodeInfo? = find { label -> labels.any { it.equals(label, ignoreCase = true) } }

    /** The first node on screen whose text or description (trimmed) matches. */
    private fun find(matches: (String) -> Boolean): AccessibilityNodeInfo? {
        for (w in windows) {
            if (w.type == AccessibilityWindowInfo.TYPE_ACCESSIBILITY_OVERLAY) continue
            val root = w.root ?: continue
            findIn(root, matches, 0)?.let { return it }
        }
        return null
    }

    private fun findIn(node: AccessibilityNodeInfo, matches: (String) -> Boolean, depth: Int): AccessibilityNodeInfo? {
        val text = node.text?.toString()?.trim()
        val desc = node.contentDescription?.toString()?.trim()
        if ((text != null && matches(text)) || (desc != null && matches(desc))) return node
        if (depth >= MAX_DEPTH) return null
        for (i in 0 until node.childCount) {
            val child = node.getChild(i) ?: continue
            findIn(child, matches, depth + 1)?.let { return it }
        }
        return null
    }

    /** Click the node, or the nearest clickable parent (labels are often inside the button). */
    private fun click(node: AccessibilityNodeInfo): Boolean {
        var n: AccessibilityNodeInfo? = node
        while (n != null) {
            if (n.isClickable) return n.performAction(AccessibilityNodeInfo.ACTION_CLICK)
            n = n.parent
        }
        return false
    }

    companion object {
        private const val TAG = "PairlyAccessibility"
        private const val SYSTEM_UI = "com.android.systemui"
        private const val OVERLAY_DELAY_MS = 150L
        private const val READ_DELAY_MS = 250L
        /** After a "Copy" click, how long to wait for the overlay before reading directly. */
        private const val NO_OVERLAY_MS = 900L
        /** An overlay this soon after a copy sent from the selection is that same copy. */
        private const val SAME_COPY_MS = 2_000L
        /** How long a selection still counts once it collapsed (Copy collapses it). */
        private const val SELECTION_GRACE_MS = 3_000L
        private const val TEXT_PREVIEW = "text_preview"
        private const val IMAGE_PREVIEW = "image_preview"
        /** The overlay shows at most this many characters. */
        private const val PREVIEW_LIMIT = 500
        private const val SPEAKER_WAIT_MS = 6_000L
        private const val MENU_WAIT_MS = 600L
        private val SPEAKER_LABELS = listOf("Speaker", "Speakerphone", "Phone speaker")
        private const val COPY_DEBOUNCE_MS = 1_000L
        private const val MAX_DEPTH = 25
        private const val POWER_MENU_WAIT_MS = 5_000L
        private const val POLL_MS = 250L
        private const val CONFIRM_WAIT_MS = 1_000L

        @Volatile
        var instance: PairlyAccessibility? = null
            private set

        private fun saysCopy(s: CharSequence?): Boolean = s?.toString()?.trim()?.lowercase()?.let {
            it == "copy" || it.startsWith("copy ") || it == "copy text" || it == "copied"
        } == true

        fun enabled(context: Context): Boolean {
            val me = ComponentName(context, PairlyAccessibility::class.java).flattenToString()
            val on = Settings.Secure.getString(context.contentResolver, Settings.Secure.ENABLED_ACCESSIBILITY_SERVICES).orEmpty()
            return on.split(':').any { it.equals(me, ignoreCase = true) }
        }

        fun settingsIntent(): Intent = Intent(Settings.ACTION_ACCESSIBILITY_SETTINGS).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
    }
}

/** A PC asks to lock, power off or restart this phone. */
class PhonePower : PowerHandler {
    override fun act(from: String, action: PowerActionData) {
        val service = PairlyAccessibility.instance
            ?: throw PairlyException.Failed("turn on Pairly in the phone's Settings → Accessibility first")
        when (action) {
            PowerActionData.LOCK -> service.lock()
            PowerActionData.POWER_OFF -> service.pressInPowerMenu(listOf("Power off", "Shut down", "Turn off"), "Power off")
            PowerActionData.RESTART -> service.pressInPowerMenu(listOf("Restart", "Reboot"), "Restart")
        }
    }
}
