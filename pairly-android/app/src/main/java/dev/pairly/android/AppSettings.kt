package dev.pairly.android

import android.content.Context
import androidx.core.content.edit
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow

/** The app's own preferences: on or off, light or dark, and a name chosen for this phone. */
object AppSettings {
    enum class Theme { SYSTEM, LIGHT, DARK }

    private const val FILE = "app"
    private const val KEY_THEME = "theme"
    private const val KEY_NAME = "device_name"
    private const val KEY_ENABLED = "enabled"

    private fun prefs(context: Context) = context.getSharedPreferences(FILE, Context.MODE_PRIVATE)

    private val _theme = MutableStateFlow(Theme.SYSTEM)

    /** Every screen follows this, so a change applies at once. */
    val theme: StateFlow<Theme> = _theme.asStateFlow()

    private val _enabled = MutableStateFlow(true)

    /** Pairly is switched on. Off: the service stays stopped until it's switched back on. */
    val enabled: StateFlow<Boolean> = _enabled.asStateFlow()

    fun setEnabled(context: Context, on: Boolean) {
        _enabled.value = on
        prefs(context).edit { putBoolean(KEY_ENABLED, on) }
    }

    /** Read the saved choices (once, when the app starts). */
    fun load(context: Context) {
        _enabled.value = prefs(context).getBoolean(KEY_ENABLED, true)
        val saved = prefs(context).getString(KEY_THEME, null)
        _theme.value = Theme.entries.firstOrNull { it.name == saved } ?: Theme.SYSTEM
    }

    fun setTheme(context: Context, theme: Theme) {
        _theme.value = theme
        prefs(context).edit { putString(KEY_THEME, theme.name) }
    }

    /** The name chosen for this phone, or null for the phone's own name. */
    fun deviceName(context: Context): String? = prefs(context).getString(KEY_NAME, null)?.takeIf { it.isNotBlank() }

    fun setDeviceName(context: Context, name: String?) = prefs(context).edit {
        if (name.isNullOrBlank()) remove(KEY_NAME) else putString(KEY_NAME, name.trim())
    }
}
