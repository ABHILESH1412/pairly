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
