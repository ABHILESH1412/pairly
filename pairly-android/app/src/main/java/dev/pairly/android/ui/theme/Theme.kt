package dev.pairly.android.ui.theme

import android.graphics.Color as AndroidColor
import android.os.Build
import androidx.activity.ComponentActivity
import androidx.activity.compose.LocalActivity
import androidx.activity.SystemBarStyle
import androidx.activity.enableEdgeToEdge
import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Shapes
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.dynamicDarkColorScheme
import androidx.compose.material3.dynamicLightColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.staticCompositionLocalOf
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import dev.pairly.android.AppSettings

/** Whether the app is showing its dark look (the user's choice, or the system's). */
val LocalDarkTheme = staticCompositionLocalOf { false }

/** Generous, soft corners: the "expressive" Material look. */
private val PairlyShapes = Shapes(
    extraSmall = RoundedCornerShape(8.dp),
    small = RoundedCornerShape(12.dp),
    medium = RoundedCornerShape(20.dp),
    large = RoundedCornerShape(28.dp),
    extraLarge = RoundedCornerShape(36.dp),
)

/** Light or dark, as chosen in Settings (or following the system). */
@Composable
fun pairlyDarkTheme(): Boolean {
    val choice by AppSettings.theme.collectAsStateWithLifecycle()
    return when (choice) {
        AppSettings.Theme.SYSTEM -> isSystemInDarkTheme()
        AppSettings.Theme.LIGHT -> false
        AppSettings.Theme.DARK -> true
    }
}

/**
 * The app's look: the wallpaper's colours on Android 12+ (Material You), the Material baseline
 * otherwise, in light or dark as chosen in Settings. [systemBars]: also colour the status and
 * navigation bar icons to match (activities that called enableEdgeToEdge).
 */
@Composable
fun PairlyTheme(systemBars: Boolean = false, content: @Composable () -> Unit) {
    val darkTheme = pairlyDarkTheme()
    val colorScheme = when {
        Build.VERSION.SDK_INT >= Build.VERSION_CODES.S -> {
            val context = LocalContext.current
            if (darkTheme) dynamicDarkColorScheme(context) else dynamicLightColorScheme(context)
        }
        darkTheme -> darkColorScheme()
        else -> lightColorScheme()
    }
    // Status and navigation bar icons follow the app's look, not only the system's (for
    // full-screen activities that draw behind the bars).
    (LocalActivity.current as? ComponentActivity)?.takeIf { systemBars }?.let { activity ->
        DisposableEffect(activity, darkTheme) {
            activity.enableEdgeToEdge(
                statusBarStyle = SystemBarStyle.auto(AndroidColor.TRANSPARENT, AndroidColor.TRANSPARENT) { darkTheme },
                navigationBarStyle = SystemBarStyle.auto(LIGHT_SCRIM, DARK_SCRIM) { darkTheme },
            )
            onDispose {}
        }
    }
    CompositionLocalProvider(LocalDarkTheme provides darkTheme) {
        MaterialTheme(colorScheme = colorScheme, shapes = PairlyShapes, content = content)
    }
}

// The scrims enableEdgeToEdge uses by default (for 3-button navigation).
private val LIGHT_SCRIM = AndroidColor.argb(0xe6, 0xFF, 0xFF, 0xFF)
private val DARK_SCRIM = AndroidColor.argb(0x80, 0x1b, 0x1b, 0x1b)
