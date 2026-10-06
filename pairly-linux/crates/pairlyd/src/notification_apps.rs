//! Which of this PC's apps may send their notifications to paired devices.
//!
//! Every app that posts a notification is remembered (by the name it gives, e.g.
//! "Screenshot"), so it can be switched off in the GTK app; switched-off apps aren't forwarded.
//! Stored in `notification-apps.json` in the data directory. `[notifications] ignore_apps` in
//! the config still applies on top.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use serde::{Deserialize, Serialize};
use tracing::warn;

/// Apps remembered at most; the longest unseen go first.
const MAX_APPS: usize = 300;
/// Seen times are rounded to this, so a busy app doesn't rewrite the file each time.
const SEEN_PRECISION_SECS: u64 = 3600;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppState {
    pub muted: bool,
    /// Unix seconds, rounded down to the hour.
    pub last_seen: u64,
}

pub struct AppFilter {
    path: PathBuf,
    apps: Mutex<BTreeMap<String, AppState>>,
}

impl AppFilter {
    pub fn load(data_dir: &Path) -> Self {
        let path = data_dir.join("notification-apps.json");
        let apps = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Self {
            path,
            apps: Mutex::new(apps),
        }
    }

    fn lock(&self) -> MutexGuard<'_, BTreeMap<String, AppState>> {
        self.apps.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `app` posted a notification: remember it, and say whether to forward it.
    pub fn allows(&self, app: &str) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
            / SEEN_PRECISION_SECS
            * SEEN_PRECISION_SECS;
        let mut apps = self.lock();
        let entry = apps.entry(app.to_owned()).or_default();
        let muted = entry.muted;
        if entry.last_seen != now {
            entry.last_seen = now;
            self.trim(&mut apps);
            self.save(&apps);
        }
        !muted
    }

    /// Every app seen, by name.
    pub fn list(&self) -> Vec<(String, AppState)> {
        self.lock()
            .iter()
            .map(|(name, state)| (name.clone(), state.clone()))
            .collect()
    }

    pub fn set_muted(&self, app: &str, muted: bool) {
        let mut apps = self.lock();
        apps.entry(app.to_owned()).or_default().muted = muted;
        self.save(&apps);
    }

    fn trim(&self, apps: &mut BTreeMap<String, AppState>) {
        while apps.len() > MAX_APPS {
            // Forget the least recently seen, but never a muted app (it'd come back unmuted).
            let Some(oldest) = apps
                .iter()
                .filter(|(_, s)| !s.muted)
                .min_by_key(|(_, s)| s.last_seen)
                .map(|(name, _)| name.clone())
            else {
                return;
            };
            apps.remove(&oldest);
        }
    }

    fn save(&self, apps: &BTreeMap<String, AppState>) {
        let written = serde_json::to_vec_pretty(apps)
            .map_err(std::io::Error::other)
            .and_then(|json| std::fs::write(&self.path, json));
        if let Err(e) = written {
            warn!(file = %self.path.display(), error = %e, "can't save the notification app list");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remembers_and_mutes_apps() {
        let dir = tempfile::tempdir().unwrap();
        let filter = AppFilter::load(dir.path());
        assert!(filter.allows("Screenshot"));
        filter.set_muted("Screenshot", true);
        assert!(!filter.allows("Screenshot"));
        assert!(filter.allows("Firefox"));
        // Kept across restarts.
        let again = AppFilter::load(dir.path());
        let list = again.list();
        assert_eq!(list.len(), 2);
        assert!(list.iter().any(|(n, s)| n == "Screenshot" && s.muted));
        assert!(!again.allows("Screenshot"));
    }
}
