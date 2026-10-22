//! The clipboard: text, images and copied image files go to connected devices (when that's on), and
//! theirs land here. Windows numbers every clipboard change; that number is checked twice a
//! second (a cheap call), and the clipboard is only read when it moved.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

/// What was copied.
pub enum Content {
    Text(String),
    /// (MIME type, an image file's bytes): PNG for copied pictures, or a copied file's own type.
    Image(&'static str, Vec<u8>),
}

/// The last thing we put on (or saw on) the clipboard, so our own writes aren't sent back. For
/// images it's the pixels: a PNG decoded and encoded again has other bytes.
#[derive(Default)]
pub struct Clipboard {
    last: Mutex<Option<u64>>,
}

fn digest(data: &[u8]) -> u64 {
    let mut h = DefaultHasher::new();
    data.hash(&mut h);
    h.finish()
}

/// Windows' clipboard change number (elsewhere: always changed).
fn sequence() -> u32 {
    #[cfg(windows)]
    {
        // SAFETY: no arguments, no state.
        unsafe { windows::Win32::System::DataExchange::GetClipboardSequenceNumber() }
    }
    #[cfg(not(windows))]
    {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        N.fetch_add(1, Ordering::Relaxed)
    }
}

/// RGBA pixels as a PNG file.
fn encode_png(width: usize, height: usize, rgba: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(
        &mut out,
        u32::try_from(width).ok()?,
        u32::try_from(height).ok()?,
    );
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::Fast);
    encoder.write_header().ok()?.write_image_data(rgba).ok()?;
    Some(out)
}

/// A PNG file as (width, height, RGBA pixels).
fn decode_png(data: &[u8]) -> Option<(usize, usize, Vec<u8>)> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(data));
    // Palettes, grey and 16-bit become plain 8-bit colour.
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    buf.truncate(info.buffer_size());
    let (w, h) = (info.width as usize, info.height as usize);
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => buf
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        png::ColorType::GrayscaleAlpha => buf
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[0], p[0], p[1]])
            .collect(),
        png::ColorType::Grayscale => buf.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::Indexed => return None,
    };
    Some((w, h, rgba))
}

impl Clipboard {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn remember(&self, d: u64) -> bool {
        self.last
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .replace(d)
            != Some(d)
    }

    /// Put `text` on the clipboard (from another device).
    pub fn set(&self, text: &str) {
        self.remember(digest(text.as_bytes()));
        match arboard::Clipboard::new().and_then(|mut c| c.set_text(text.to_owned())) {
            Ok(()) => {}
            Err(e) => tracing::warn!(error = %e, "can't set the clipboard"),
        }
    }

    /// Put an image on the clipboard (from another device): PNG, or a copied JPEG, GIF, WebP or
    /// BMP file.
    pub fn set_image(&self, mime: &str, data: &[u8]) {
        let decoded = if mime == "image/png" {
            decode_png(data)
        } else {
            image::load_from_memory(data).ok().map(|i| {
                let i = i.into_rgba8();
                (i.width() as usize, i.height() as usize, i.into_raw())
            })
        };
        let Some((width, height, rgba)) = decoded else {
            tracing::warn!("couldn't read the copied image");
            return;
        };
        self.remember(digest(&rgba));
        let image = arboard::ImageData {
            width,
            height,
            bytes: rgba.into(),
        };
        if let Err(e) = arboard::Clipboard::new().and_then(|mut c| c.set_image(image)) {
            tracing::warn!(error = %e, "can't put the image on the clipboard");
        }
    }

    /// The clipboard's text, or else its image, now.
    pub fn get() -> Option<Content> {
        Self::read().map(|(content, _)| content)
    }

    /// What's on the clipboard, and its digest (pixels for images).
    fn read() -> Option<(Content, u64)> {
        let mut clipboard = arboard::Clipboard::new().ok()?;
        // Explorer copies a file as its path: send the picture of one copied image file.
        if let Ok(files) = clipboard.get().file_list()
            && let [file] = files.as_slice()
            && let Some(data) = std::fs::metadata(file)
                .ok()
                .filter(|m| m.len() <= pairly_plugins::clipboard::MAX_IMAGE_BYTES as u64)
                .and_then(|_| std::fs::read(file).ok())
            && let Some(mime) = pairly_plugins::clipboard::image_type(&data)
        {
            let d = digest(&data);
            return Some((Content::Image(mime, data), d));
        }
        if let Some(text) = clipboard.get_text().ok().filter(|t| !t.is_empty()) {
            let d = digest(text.as_bytes());
            return Some((Content::Text(text), d));
        }
        let image = clipboard.get_image().ok()?;
        let d = digest(&image.bytes);
        let png = encode_png(image.width, image.height, &image.bytes)?;
        (png.len() <= pairly_plugins::clipboard::MAX_IMAGE_BYTES)
            .then_some((Content::Image("image/png", png), d))
    }

    /// Call `changed` with each new text or image copied on this PC, for as long as the app
    /// runs.
    pub fn watch(self: Arc<Self>, changed: impl Fn(Content) + Send + 'static) {
        std::thread::Builder::new()
            .name("pairly-clipboard".into())
            .spawn(move || {
                // What's there at start isn't a new copy.
                if let Some((_, d)) = Self::read() {
                    self.remember(d);
                }
                let mut seen = sequence();
                loop {
                    std::thread::sleep(Duration::from_millis(500));
                    let now = sequence();
                    if now == seen {
                        continue;
                    }
                    seen = now;
                    if let Some((content, d)) = Self::read()
                        && self.remember(d)
                    {
                        changed(content);
                    }
                }
            })
            .expect("can start the clipboard thread");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_round_trip() {
        let rgba: Vec<u8> = (0..(7 * 5 * 4)).map(|i| (i * 3 % 256) as u8).collect();
        let png = encode_png(7, 5, &rgba).expect("encodes");
        assert_eq!(decode_png(&png), Some((7, 5, rgba)));
    }

    #[test]
    fn other_image_types_decode() {
        let mut bmp = Vec::new();
        image::RgbaImage::from_pixel(3, 2, image::Rgba([10, 20, 30, 255]))
            .write_to(&mut std::io::Cursor::new(&mut bmp), image::ImageFormat::Bmp)
            .expect("encodes");
        assert_eq!(
            pairly_plugins::clipboard::image_type(&bmp),
            Some("image/bmp")
        );
        let i = image::load_from_memory(&bmp).expect("decodes").into_rgba8();
        assert_eq!((i.width(), i.height()), (3, 2));
    }
}
