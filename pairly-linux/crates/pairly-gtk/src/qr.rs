//! Render a QR code into a crisp GDK texture (dark modules on white, with a quiet zone).

use qrcode::{Color, EcLevel, QrCode};
use relm4::gtk::{gdk, glib};

/// Pixels per module, chosen so a typical pairing code renders at roughly 300 px.
const SCALE: usize = 6;
/// Blank border in modules; scanners need it.
const QUIET: usize = 4;

pub fn texture(data: &str) -> Option<gdk::MemoryTexture> {
    let code = QrCode::with_error_correction_level(data, EcLevel::M).ok()?;
    let modules = code.width();
    let colors = code.to_colors();
    let size = (modules + 2 * QUIET) * SCALE;
    let mut rgb = vec![0xffu8; size * size * 3];
    for (i, color) in colors.iter().enumerate() {
        if *color != Color::Dark {
            continue;
        }
        let (mx, my) = (i % modules + QUIET, i / modules + QUIET);
        for y in my * SCALE..(my + 1) * SCALE {
            let row = &mut rgb[(y * size + mx * SCALE) * 3..(y * size + (mx + 1) * SCALE) * 3];
            row.fill(0);
        }
    }
    let side = i32::try_from(size).ok()?;
    Some(gdk::MemoryTexture::new(
        side,
        side,
        gdk::MemoryFormat::R8g8b8,
        &glib::Bytes::from_owned(rgb),
        size * 3,
    ))
}
