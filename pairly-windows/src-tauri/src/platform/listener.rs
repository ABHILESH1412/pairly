//! This PC's notifications, for paired phones: Windows' notification listener is read every
//! two seconds, new notifications go to the phones and gone ones are taken down there. A phone
//! dismissing one removes it here too. Windows asks the user once whether Pairly may read
//! notifications; if it says no (or this Windows doesn't let this kind of app), the reason shows
//! in Settings.

use std::sync::{Mutex, PoisonError, Weak};

use crate::backend::Backend;

static STATUS: Mutex<String> = Mutex::new(String::new());

/// "on", or why this PC's notifications can't be sent.
pub fn status() -> String {
    let s = STATUS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    if s.is_empty() { "starting".into() } else { s }
}

fn set_status(s: impl Into<String>) {
    *STATUS.lock().unwrap_or_else(PoisonError::into_inner) = s.into();
}

/// Ids we send are "win-<number>".
const PREFIX: &str = "win-";

pub fn start(backend: Weak<Backend>) {
    let spawned = std::thread::Builder::new()
        .name("pairly-notifications".into())
        .spawn(move || imp::run(&backend));
    if let Err(e) = spawned {
        set_status(format!("can't start: {e}"));
    }
}

/// A phone dismissed one of this PC's notifications.
pub fn dismiss(id: &str) {
    if let Some(n) = id.strip_prefix(PREFIX).and_then(|n| n.parse::<u32>().ok()) {
        imp::remove(n);
    }
}

#[cfg(windows)]
mod imp {
    use std::collections::HashMap;
    use std::sync::Weak;
    use std::time::Duration;

    use pairly_plugins::notification::Notification;
    use windows::UI::Notifications::Management::{
        UserNotificationListener, UserNotificationListenerAccessStatus,
    };
    use windows::UI::Notifications::{KnownNotificationBindings, NotificationKinds};

    use super::{PREFIX, set_status};
    use crate::backend::Backend;

    pub fn remove(id: u32) {
        if let Ok(listener) = UserNotificationListener::Current() {
            let _ = listener.RemoveNotification(id);
        }
    }

    /// (app, title, text) of one notification.
    fn read(n: &windows::UI::Notifications::UserNotification) -> Option<(String, String, String)> {
        let app = n
            .AppInfo()
            .and_then(|a| a.DisplayInfo())
            .and_then(|d| d.DisplayName())
            .map(|h| h.to_string())
            .unwrap_or_default();
        let binding = n
            .Notification()
            .ok()?
            .Visual()
            .ok()?
            .GetBinding(&KnownNotificationBindings::ToastGeneric().ok()?)
            .ok()?;
        let texts: Vec<String> = binding
            .GetTextElements()
            .ok()?
            .into_iter()
            .filter_map(|t| t.Text().ok().map(|h| h.to_string()))
            .filter(|t| !t.trim().is_empty())
            .collect();
        let mut texts = texts.into_iter();
        let title = texts.next().unwrap_or_default();
        let body = texts.collect::<Vec<_>>().join("\n");
        Some((app, title, body))
    }

    pub fn run(backend: &Weak<Backend>) {
        let listener = match UserNotificationListener::Current() {
            Ok(l) => l,
            Err(e) => {
                tracing::info!(error = %e, "can't read this PC's notifications");
                set_status("Windows doesn't let Pairly read this PC's notifications");
                return;
            }
        };
        let access = listener.RequestAccessAsync().and_then(|op| op.join());
        match access {
            Ok(UserNotificationListenerAccessStatus::Allowed) => set_status("on"),
            Ok(UserNotificationListenerAccessStatus::Denied) => {
                set_status(
                    "Not allowed: turn on Notifications access for Pairly in Windows' privacy settings",
                );
                return;
            }
            Ok(_) => {
                set_status("Windows didn't say whether Pairly may read notifications");
                return;
            }
            Err(e) => {
                tracing::info!(error = %e, "can't read this PC's notifications");
                set_status("Windows only lets Store apps read this PC's notifications");
                return;
            }
        }
        // What phones have: id → (app, title, text).
        let mut sent: HashMap<u32, (String, String, String)> = HashMap::new();
        let mut first = true;
        loop {
            std::thread::sleep(Duration::from_secs(2));
            let Some(b) = backend.upgrade() else { return };
            let settings = b.settings();
            let Ok(list) = listener
                .GetNotificationsAsync(NotificationKinds::Toast)
                .and_then(|op| op.join())
            else {
                continue;
            };
            let mut now = HashMap::new();
            for n in list {
                let Ok(id) = n.Id() else { continue };
                let Some(content) = read(&n) else { continue };
                // Not our own (the phone's, mirrored), and not muted apps.
                if content.0 == "Pairly" {
                    continue;
                }
                b.note_app(&content.0);
                if settings.muted_apps.contains(&content.0) {
                    continue;
                }
                let time = n
                    .CreationTime()
                    .ok()
                    .and_then(|t| {
                        u64::try_from((t.UniversalTime - 116_444_736_000_000_000) / 10_000).ok()
                    })
                    .unwrap_or_else(crate::backend::now_ms);
                now.insert(id, (content, time));
            }
            let Some((_, f)) = b.running() else {
                sent.clear();
                continue;
            };
            // What was there at start isn't new: don't flood the phone with old ones.
            if first {
                first = false;
                sent = now.into_iter().map(|(id, (c, _))| (id, c)).collect();
                continue;
            }
            // Off: what shows meanwhile counts as seen, so switching back on sends only new ones.
            if !settings.send_notifications {
                sent = now.into_iter().map(|(id, (c, _))| (id, c)).collect();
                continue;
            }
            for (id, ((app, title, text), time)) in &now {
                if sent.get(id) == Some(&(app.clone(), title.clone(), text.clone())) {
                    continue;
                }
                sent.insert(*id, (app.clone(), title.clone(), text.clone()));
                f.notifications.posted(Notification {
                    id: format!("{PREFIX}{id}"),
                    app: app.clone(),
                    title: title.clone(),
                    text: text.clone(),
                    time: *time,
                    actions: Vec::new(),
                    can_reply: false,
                    icon: None,
                    silent: false,
                });
            }
            let gone: Vec<u32> = sent
                .keys()
                .filter(|id| !now.contains_key(id))
                .copied()
                .collect();
            for id in gone {
                sent.remove(&id);
                f.notifications.removed(&format!("{PREFIX}{id}"));
            }
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use std::sync::Weak;

    use crate::backend::Backend;

    pub fn remove(_id: u32) {}

    pub fn run(_backend: &Weak<Backend>) {
        super::set_status("only on Windows");
    }
}
