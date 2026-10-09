//! The clipboard: text copied here goes to the phone (when that's on), and text from the phone
//! lands here. Windows has no portable "clipboard changed" event for us, so it's checked twice a
//! second (a cheap call).

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

/// The text we last put on (or saw on) the clipboard, so our own writes aren't sent back.
#[derive(Default)]
pub struct Clipboard {
    last: Mutex<Option<String>>,
}

impl Clipboard {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn remember(&self, text: &str) -> bool {
        let mut last = self.last.lock().unwrap_or_else(PoisonError::into_inner);
        if last.as_deref() == Some(text) {
            return false;
        }
        *last = Some(text.to_owned());
        true
    }

    /// Put `text` on the clipboard (from the phone).
    pub fn set(&self, text: &str) {
        self.remember(text);
        match arboard::Clipboard::new().and_then(|mut c| c.set_text(text.to_owned())) {
            Ok(()) => {}
            Err(e) => tracing::warn!(error = %e, "can't set the clipboard"),
        }
    }

    /// The clipboard's text now.
    pub fn get() -> Option<String> {
        arboard::Clipboard::new()
            .and_then(|mut c| c.get_text())
            .ok()
            .filter(|t| !t.is_empty())
    }

    /// Call `changed` with each new text copied on this PC, for as long as the app runs.
    pub fn watch(self: Arc<Self>, changed: impl Fn(String) + Send + 'static) {
        std::thread::Builder::new()
            .name("pairly-clipboard".into())
            .spawn(move || {
                // What's there at start isn't a new copy.
                if let Some(text) = Self::get() {
                    self.remember(&text);
                }
                loop {
                    std::thread::sleep(Duration::from_millis(500));
                    if let Some(text) = Self::get()
                        && self.remember(&text)
                    {
                        changed(text);
                    }
                }
            })
            .expect("can start the clipboard thread");
    }
}
