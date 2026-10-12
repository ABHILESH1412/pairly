//! The app's preferences, kept as `settings.json` in its data folder.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// "system", "light" or "dark".
    pub theme: String,
    /// The name paired devices see (None: the computer's name).
    pub name: Option<String>,
    /// Pairly is switched on (off: no connections at all, until switched back on).
    pub enabled: bool,
    /// Install new versions by itself.
    pub auto_update: bool,
    /// Send what's copied on this PC to connected devices straight away.
    pub clipboard_auto: bool,
    /// The device list shows beside the device (wide windows).
    pub sidebar: bool,
    /// A relay for reaching devices over the internet (`pairly-relay://…`); paired phones
    /// learn it from this PC.
    pub relay: Option<String>,
    /// Pad what goes through the relay to 256-byte steps (hides exact message sizes).
    pub relay_padding: bool,
    /// Reach bonded devices over Bluetooth when there is no shared network.
    pub bluetooth: bool,
    /// Show the phone's notifications here.
    pub show_notifications: bool,
    /// Closing a phone's notification here clears it on the phone.
    pub dismiss_on_phone: bool,
    /// Send this PC's notifications to paired phones.
    pub send_notifications: bool,
    /// Apps whose notifications are never sent.
    pub muted_apps: Vec<String>,
    /// Pause this PC's music while the phone rings or is in a call.
    pub pause_media_for_calls: bool,
    /// Paired phones may lock, restart or power off this PC.
    pub power_from_phone: bool,
    /// Paired phones may view and control this screen (same network).
    pub share_screen: bool,
    /// Open links sent from the phone straight away (else: a notification to open them).
    pub open_links: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: "system".into(),
            name: None,
            enabled: true,
            auto_update: true,
            clipboard_auto: true,
            sidebar: true,
            relay: None,
            relay_padding: true,
            bluetooth: true,
            show_notifications: true,
            dismiss_on_phone: true,
            send_notifications: true,
            muted_apps: Vec::new(),
            pause_media_for_calls: true,
            power_from_phone: true,
            share_screen: true,
            open_links: true,
        }
    }
}

fn file(dir: &Path) -> PathBuf {
    dir.join("settings.json")
}

impl Settings {
    pub fn load(dir: &Path) -> Self {
        std::fs::read(file(dir))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, dir: &Path) {
        if let Ok(json) = serde_json::to_vec_pretty(self) {
            let _ = std::fs::create_dir_all(dir);
            let _ = std::fs::write(file(dir), json);
        }
    }
}

/// The computer's own name.
pub fn computer_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|s| s.trim().to_owned())
        })
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "Windows PC".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_settings_files_still_load() {
        let old = r#"{"theme":"dark","name":null,"enabled":true,"auto_update":false,
            "clipboard_auto":true,"sidebar":true}"#;
        let s: Settings = serde_json::from_str(old).unwrap_or_default();
        assert_eq!(s.theme, "dark");
        assert!(!s.auto_update);
        assert!(s.show_notifications && s.share_screen && s.relay.is_none());
    }
}
