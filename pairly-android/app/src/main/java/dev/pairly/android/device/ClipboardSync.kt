package dev.pairly.android.device

import android.content.Context
import androidx.core.content.edit

/**
 * Copying on the phone sends to connected PCs on its own, when Pairly's accessibility service
 * is on (it notices the copy) and this setting allows it. Remembers the last text either way,
 * so a clip that came from a PC isn't sent straight back.
 */
object ClipboardSync {
    private const val PREFS = "clipboard"
    private const val KEY_AUTO = "auto_send"

    @Volatile
    var last: String? = null

    /** Read once, then kept: the accessibility service asks on every screen event. */
    @Volatile
    private var autoCached: Boolean? = null

    fun auto(context: Context): Boolean = autoCached
        ?: context.getSharedPreferences(PREFS, Context.MODE_PRIVATE).getBoolean(KEY_AUTO, true).also { autoCached = it }

    fun setAuto(context: Context, on: Boolean) {
        autoCached = on
        context.getSharedPreferences(PREFS, Context.MODE_PRIVATE).edit { putBoolean(KEY_AUTO, on) }
    }
}
