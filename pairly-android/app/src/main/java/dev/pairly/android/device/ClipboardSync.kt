package dev.pairly.android.device

import android.content.ClipData
import android.content.Context
import android.net.Uri
import androidx.core.content.edit

/**
 * Copying on the phone sends to connected PCs on its own, when Pairly's accessibility service
 * is on (it notices the copy) and this setting allows it. Remembers the last text either way,
 * so a clip that came from a PC isn't sent straight back.
 */
object ClipboardSync {
    /** What was copied: text, or an image (from a screenshot, the gallery, a browser…). */
    sealed interface Copied {
        data class Text(val text: String) : Copied
        data class Image(val uri: Uri) : Copied
    }

    /** What `clip` holds, if anything. */
    fun read(context: Context, clip: ClipData?): Copied? {
        val item = clip?.takeIf { it.itemCount > 0 }?.getItemAt(0) ?: return null
        val description = clip.description
        val image = (0 until description.mimeTypeCount).any { description.getMimeType(it).startsWith("image/") }
        val uri = item.uri
        if (uri != null && (image || context.contentResolver.getType(uri)?.startsWith("image/") == true)) {
            return Copied.Image(uri)
        }
        return item.coerceToText(context)?.toString()?.takeIf { it.isNotEmpty() }?.let { Copied.Text(it) }
    }

    /** What [last] holds for `copied`, so the same copy isn't sent twice. */
    fun key(copied: Copied): String = when (copied) {
        is Copied.Text -> copied.text
        is Copied.Image -> "image:${copied.uri}"
    }

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
