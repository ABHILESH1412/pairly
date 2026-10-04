package dev.pairly.android.notifications

import android.content.Context
import androidx.core.content.edit
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow

data class AppEntry(val packageName: String, val label: String, val muted: Boolean)

/** Which apps' notifications go to paired devices. Apps appear here once they post something. */
class NotificationPrefs(context: Context) {
    private val prefs = context.getSharedPreferences("notifications", Context.MODE_PRIVATE)
    private val _apps = MutableStateFlow(load())
    val apps: StateFlow<List<AppEntry>> = _apps.asStateFlow()

    fun isMuted(packageName: String): Boolean = packageName in prefs.getStringSet(KEY_MUTED, emptySet()).orEmpty()

    fun setMuted(packageName: String, muted: Boolean) {
        val current = prefs.getStringSet(KEY_MUTED, emptySet()).orEmpty().toMutableSet()
        if (muted) current += packageName else current -= packageName
        prefs.edit { putStringSet(KEY_MUTED, current) }
        _apps.value = load()
    }

    /** Remember an app the first time it posts, so it shows up in the picker. */
    fun noteSeen(packageName: String, label: String) {
        if (prefs.getString(SEEN_PREFIX + packageName, null) == label) return
        prefs.edit { putString(SEEN_PREFIX + packageName, label) }
        _apps.value = load()
    }

    private fun load(): List<AppEntry> {
        val muted = prefs.getStringSet(KEY_MUTED, emptySet()).orEmpty()
        return prefs.all.keys
            .filter { it.startsWith(SEEN_PREFIX) }
            .map { key ->
                val pkg = key.removePrefix(SEEN_PREFIX)
                AppEntry(pkg, prefs.getString(key, pkg) ?: pkg, pkg in muted)
            }
            .sortedBy { it.label.lowercase() }
    }

    private companion object {
        const val KEY_MUTED = "muted"
        const val SEEN_PREFIX = "seen:"
    }
}
