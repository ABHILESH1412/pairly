package dev.pairly.android.share

import android.content.Context
import androidx.core.content.edit
import dev.pairly.android.Pairly
import dev.pairly.core.ffi.ShareHandler
import dev.pairly.core.ffi.TransferData

/** Something the Rust core reported about sharing, handled in order by [Pairly]. */
sealed interface ShareEvent {
    data class Offered(val transfer: TransferData) : ShareEvent
    data class Changed(val transfer: TransferData) : ShareEvent
    data class Text(val fromName: String, val text: String, val url: Boolean) : ShareEvent
}

/** Called on Rust threads (including per-file I/O threads): only hands events over. */
class ShareFeatures : ShareHandler {
    override fun fileOffered(transfer: TransferData) = Pairly.onShare(ShareEvent.Offered(transfer))

    override fun transferChanged(transfer: TransferData) = Pairly.onShare(ShareEvent.Changed(transfer))

    override fun textReceived(fromId: String, fromName: String, text: String, url: Boolean) =
        Pairly.onShare(ShareEvent.Text(fromName, text, url))
}

object SharePrefs {
    private const val KEY_ASK = "ask_before_receiving"

    private fun prefs(context: Context) = context.getSharedPreferences("share", Context.MODE_PRIVATE)

    /** Off by default: paired devices are trusted, as in KDE Connect. */
    fun askBeforeReceiving(context: Context): Boolean = prefs(context).getBoolean(KEY_ASK, false)

    fun setAskBeforeReceiving(context: Context, ask: Boolean) = prefs(context).edit { putBoolean(KEY_ASK, ask) }

    /** `http(s)` links open in a browser; anything else is text (mirrors the Rust check). */
    fun isWebUrl(text: String): Boolean {
        val t = text.trim()
        return (t.startsWith("https://", ignoreCase = true) || t.startsWith("http://", ignoreCase = true)) &&
            t.none(Char::isWhitespace)
    }
}
