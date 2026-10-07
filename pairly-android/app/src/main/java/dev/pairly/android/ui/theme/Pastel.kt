package dev.pairly.android.ui.theme

import androidx.compose.runtime.Composable
import androidx.compose.runtime.Immutable
import androidx.compose.ui.graphics.Color

/** A soft colour for a card: its background, its text and icon, and a quieter hint colour. */
@Immutable
data class PastelColors(val background: Color, val content: Color, val hint: Color)

/**
 * The pastel palette (the same as the PC app's). Light: a pale tint with dark text; dark: a deep
 * shade of the same hue with light text.
 */
enum class Hue(private val light: PastelColors, private val dark: PastelColors) {
    PURPLE(PastelColors(Color(0xFFEEEDFE), Color(0xFF3C3489), Color(0xFF534AB7)), PastelColors(Color(0xFF3C3489), Color(0xFFCECBF6), Color(0xFFAFA9EC))),
    TEAL(PastelColors(Color(0xFFE1F5EE), Color(0xFF085041), Color(0xFF0F6E56)), PastelColors(Color(0xFF085041), Color(0xFF9FE1CB), Color(0xFF5DCAA5))),
    CORAL(PastelColors(Color(0xFFFAECE7), Color(0xFF712B13), Color(0xFF993C1D)), PastelColors(Color(0xFF712B13), Color(0xFFF5C4B3), Color(0xFFF0997B))),
    AMBER(PastelColors(Color(0xFFFAEEDA), Color(0xFF633806), Color(0xFF854F0B)), PastelColors(Color(0xFF633806), Color(0xFFFAC775), Color(0xFFEF9F27))),
    BLUE(PastelColors(Color(0xFFE6F1FB), Color(0xFF0C447C), Color(0xFF185FA5)), PastelColors(Color(0xFF0C447C), Color(0xFFB5D4F4), Color(0xFF85B7EB))),
    PINK(PastelColors(Color(0xFFFBEAF0), Color(0xFF72243E), Color(0xFF993556)), PastelColors(Color(0xFF72243E), Color(0xFFF4C0D1), Color(0xFFED93B1))),
    GREEN(PastelColors(Color(0xFFEAF3DE), Color(0xFF27500A), Color(0xFF3B6D11)), PastelColors(Color(0xFF27500A), Color(0xFFC0DD97), Color(0xFF97C459))),
    GRAY(PastelColors(Color(0xFFF1EFE8), Color(0xFF444441), Color(0xFF5F5E5A)), PastelColors(Color(0xFF444441), Color(0xFFD3D1C7), Color(0xFFB4B2A9))),
    RED(PastelColors(Color(0xFFFCEBEB), Color(0xFF791F1F), Color(0xFFA32D2D)), PastelColors(Color(0xFF791F1F), Color(0xFFF7C1C1), Color(0xFFF09595)));

    /** The colours for the current look (light or dark). */
    @Composable
    fun colors(): PastelColors = if (LocalDarkTheme.current) dark else light
}
