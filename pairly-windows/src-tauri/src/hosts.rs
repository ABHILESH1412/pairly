//! The node's view of this PC: every feature's hooks, answered by the backend.

use std::path::PathBuf;
use std::sync::{Arc, Weak};
use std::time::Duration;

use pairly_core::{DeviceId, PeerInfo, Platform};
use pairly_plugins::battery::{BatteryHost, BatteryState};
use pairly_plugins::clipboard::ClipboardHost;
use pairly_plugins::contacts::ContactsHost;
use pairly_plugins::files::FilesHost;
use pairly_plugins::findmy::FindMyHost;
use pairly_plugins::input::{InputHost, KeyInput, LaserPointer, PointerButton, PointerMotion};
use pairly_plugins::media::{MediaAction, MediaHost, PlayerState};
use pairly_plugins::notification::{Notification, NotificationHost};
use pairly_plugins::power::{PowerAction, PowerHost};
use pairly_plugins::screen::{ScreenFrame, ScreenHost, ScreenInput};
use pairly_plugins::share::{ShareHost, Transfer, TransferState};
use pairly_plugins::sms::{Message, SmsHost};
use pairly_plugins::telephony::{CallEvent, TelephonyHost};
use tauri::Emitter;

use crate::backend::{Backend, data_url, lock, now_ms};
use crate::platform::{self, toast};

/// The PC has no contacts of its own; it only asks phones for theirs.
pub struct NoShare;
impl ContactsHost for NoShare {}

/// The user's own folder (`C:\\Users\\name`).
pub fn home_folder() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map_or_else(|| PathBuf::from("."), PathBuf::from)
}

pub struct Host(pub Weak<Backend>);

/// The folder this PC shares with paired devices (off unless switched on in Settings;
/// read-only unless changes are allowed; hidden and system files are never shared).
impl FilesHost for Host {
    fn root(&self) -> Result<PathBuf, String> {
        let settings = self.0.upgrade().ok_or("Pairly is stopping")?.settings();
        if !settings.share_files {
            return Err(
                "this PC doesn't share its files (turn it on in Pairly's settings there)".into(),
            );
        }
        let folder = settings
            .share_folder
            .map_or_else(home_folder, PathBuf::from);
        if !folder.is_dir() {
            return Err(format!("the shared folder {} is gone", folder.display()));
        }
        Ok(folder)
    }

    fn allow_changes(&self) -> bool {
        self.0.upgrade().is_some_and(|b| b.settings().share_changes)
    }

    fn show_hidden(&self) -> bool {
        false
    }
}

impl Host {
    fn with(&self, f: impl FnOnce(&Arc<Backend>)) {
        if let Some(b) = self.0.upgrade() {
            f(&b);
        }
    }
}

impl Platform for Host {
    fn ping_received(&self, from: &PeerInfo, message: Option<&str>) {
        self.with(|b| b.notify(&format!("Ping from {}", from.name), message.unwrap_or("")));
    }
}

impl ClipboardHost for Host {
    fn set_clipboard(&self, _from: &PeerInfo, text: &str) {
        self.with(|b| b.clipboard.set(text));
    }
}

impl BatteryHost for Host {
    fn current(&self) -> Option<BatteryState> {
        platform::battery()
    }

    fn peer_changed(&self, from: &PeerInfo, state: BatteryState, previous: Option<BatteryState>) {
        self.with(|b| {
            let was_ok = previous.is_none_or(|p| p.percent > 15 || p.charging);
            if was_ok && state.percent <= 15 && !state.charging {
                b.notify(
                    &format!("{} battery low", from.name),
                    &format!("{}% left", state.percent),
                );
            }
            b.emit();
        });
    }
}

impl FindMyHost for Host {
    fn ring(&self, from: &PeerInfo, on: bool) {
        self.with(|b| {
            *lock(&b.rung) = on;
            if !on {
                return;
            }
            b.notify(
                &format!("{} is looking for this PC", from.name),
                "Ringing for 30 seconds",
            );
            b.show_window();
            // A beep every second or so, for half a minute or until stopped.
            let weak = Arc::downgrade(b);
            std::thread::spawn(move || {
                for _ in 0..25 {
                    match weak.upgrade() {
                        Some(b) if *lock(&b.rung) => platform::beep(),
                        _ => return,
                    }
                    std::thread::sleep(Duration::from_millis(1200));
                }
            });
        });
    }
}

/// A notification's icon as a PNG file for Windows to show (written once per picture).
fn icon_file(b: &Backend, n: &Notification) -> Option<std::path::PathBuf> {
    use std::hash::{Hash, Hasher};
    let icon = n.icon.as_ref()?;
    let (w, h) = (u32::from(icon.width), u32::from(icon.height));
    if w == 0 || h == 0 || icon.rgba.len() != (w * h * 4) as usize {
        return None;
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    icon.rgba.hash(&mut hasher);
    let dir = b.cache_dir.join("icons");
    let path = dir.join(format!("{:016x}.png", hasher.finish()));
    if path.exists() {
        return Some(path);
    }
    std::fs::create_dir_all(&dir).ok()?;
    let file = std::fs::File::create(&path).ok()?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let written = encoder
        .write_header()
        .and_then(|mut w| w.write_image_data(&icon.rgba));
    if written.is_err() {
        let _ = std::fs::remove_file(&path);
        return None;
    }
    Some(path)
}

impl NotificationHost for Host {
    fn show(&self, from: &PeerInfo, n: &Notification) {
        self.with(|b| {
            if !b.settings().show_notifications {
                return;
            }
            let tag = toast::short_tag(&n.id);
            let group = from.id.to_string();
            lock(&b.shown_notes).insert((from.id, n.id.clone()), tag.clone());
            let title = if n.title.is_empty() {
                n.app.clone()
            } else {
                n.title.clone()
            };
            let spec = toast::Toast {
                title,
                body: n.text.clone(),
                attribution: format!("{} · {}", n.app, from.name),
                icon: icon_file(b, n),
                buttons: n
                    .actions
                    .iter()
                    .map(|a| toast::Button {
                        id: a.key.clone(),
                        label: a.label.clone(),
                    })
                    .collect(),
                reply: n.can_reply,
                silent: n.silent,
                quiet: n.silent,
                tag,
                group,
                ..toast::Toast::default()
            };
            let (weak, peer, id) = (Arc::downgrade(b), from.id, n.id.clone());
            let shown = toast::show(
                &spec,
                Some(Box::new(move |event| {
                    let Some(b) = weak.upgrade() else { return };
                    let Ok(f) = b.features() else { return };
                    let result = match event {
                        toast::ToastEvent::Clicked(action) => {
                            f.notifications.request_action(peer, &id, &action)
                        }
                        toast::ToastEvent::Reply(text) => {
                            f.notifications.request_reply(peer, &id, &text)
                        }
                        toast::ToastEvent::Dismissed => {
                            lock(&b.shown_notes).remove(&(peer, id.clone()));
                            if b.settings().dismiss_on_phone {
                                f.notifications.request_dismiss(peer, &id)
                            } else {
                                Ok(())
                            }
                        }
                        toast::ToastEvent::Opened => {
                            b.show_window();
                            Ok(())
                        }
                    };
                    if let Err(e) = result {
                        tracing::debug!(error = %e, "can't reach the phone for a notification");
                    }
                })),
            );
            if !shown && !n.silent {
                b.notify(&format!("{} · {}", spec.title, n.app), &n.text);
            }
        });
    }

    fn remove(&self, from: &PeerInfo, id: &str) {
        self.with(|b| {
            if let Some(tag) = lock(&b.shown_notes).remove(&(from.id, id.to_owned())) {
                toast::remove(&from.id.to_string(), &tag);
            }
        });
    }

    fn sync(&self, from: &PeerInfo, active: &[String]) {
        self.with(|b| {
            let gone: Vec<String> = lock(&b.shown_notes)
                .iter()
                .filter(|((d, id), _)| *d == from.id && !active.contains(id))
                .map(|(_, tag)| tag.clone())
                .collect();
            lock(&b.shown_notes).retain(|(d, id), _| *d != from.id || active.contains(id));
            for tag in gone {
                toast::remove(&from.id.to_string(), &tag);
            }
        });
    }

    // A phone acting on this PC's notifications.
    fn dismiss_local(&self, id: &str) {
        crate::platform::listener::dismiss(id);
    }

    fn action_local(&self, _id: &str, _action: &str) {}

    fn reply_local(&self, _id: &str, _text: &str) {}
}

impl ShareHost for Host {
    fn file_offered(&self, transfer: &Transfer) {
        self.with(|b| {
            // Files from paired devices are taken straight away, as on Linux.
            if let Err(e) = b.accept_file(transfer) {
                tracing::warn!(error = %e, "can't receive {}", transfer.name);
                if let Ok(f) = b.features() {
                    let _ = f.share.cancel(transfer.id);
                }
            }
            b.emit();
        });
    }

    fn transfer_changed(&self, t: &Transfer) {
        self.with(|b| {
            if t.state.is_finished() {
                if let Some((part, target)) = lock(&b.receiving).remove(&t.id) {
                    if t.state == TransferState::Done {
                        let _ = std::fs::rename(&part, &target);
                        let opened = target.clone();
                        let shown = toast::show(
                            &toast::Toast {
                                title: format!("Received {}", t.name),
                                body: format!(
                                    "From {} · in {}",
                                    t.peer_name,
                                    b.downloads.display()
                                ),
                                buttons: vec![
                                    toast::Button {
                                        id: "open".into(),
                                        label: "Open".into(),
                                    },
                                    toast::Button {
                                        id: "folder".into(),
                                        label: "Show in Folder".into(),
                                    },
                                ],
                                ..toast::Toast::default()
                            },
                            Some(Box::new(move |event| match event {
                                toast::ToastEvent::Clicked(a) if a == "folder" => {
                                    platform::show_in_folder(&opened);
                                }
                                toast::ToastEvent::Clicked(_) | toast::ToastEvent::Opened => {
                                    platform::open(&opened.to_string_lossy());
                                }
                                _ => {}
                            })),
                        );
                        if !shown {
                            b.notify(&format!("Received {}", t.name), &t.peer_name);
                        }
                    } else {
                        let _ = std::fs::remove_file(&part);
                    }
                } else if !t.incoming && t.state == TransferState::Done {
                    b.notify(&format!("Sent {}", t.name), &format!("To {}", t.peer_name));
                }
            }
            b.emit();
        });
    }

    fn text_received(&self, from: &PeerInfo, text: &str, url: bool) {
        self.with(|b| {
            if url && b.settings().open_links {
                platform::open(text);
                b.notify(&format!("Link from {}", from.name), text);
            } else if url {
                let link = text.to_owned();
                toast::show(
                    &toast::Toast {
                        title: format!("Link from {}", from.name),
                        body: text.to_owned(),
                        buttons: vec![toast::Button {
                            id: "open".into(),
                            label: "Open".into(),
                        }],
                        ..toast::Toast::default()
                    },
                    Some(Box::new(move |event| {
                        if matches!(
                            event,
                            toast::ToastEvent::Clicked(_) | toast::ToastEvent::Opened
                        ) {
                            platform::open(&link);
                        }
                    })),
                );
            } else {
                b.clipboard.set(text);
                b.notify(
                    &format!("Text from {}", from.name),
                    "Copied to the clipboard",
                );
            }
        });
    }
}

impl PowerHost for Host {
    fn act(&self, from: &PeerInfo, action: PowerAction) -> std::result::Result<(), String> {
        tracing::info!(device = %from.name, ?action, "power request");
        let allowed = self
            .0
            .upgrade()
            .is_some_and(|b| b.settings().power_from_phone);
        if !allowed {
            return Err("this PC doesn't take power commands from phones".into());
        }
        platform::power(action)
    }
}

impl InputHost for Host {
    fn pointer(&self, _from: &PeerInfo, motion: PointerMotion) {
        self.with(|b| b.input.pointer(motion));
    }

    fn button(&self, _from: &PeerInfo, button: PointerButton) {
        self.with(|b| b.input.button(button));
    }

    fn key(&self, _from: &PeerInfo, key: &KeyInput) {
        self.with(|b| b.input.key(key.clone()));
    }

    fn laser(&self, _from: &PeerInfo, laser: LaserPointer) {
        self.with(|b| b.screens.laser(laser));
    }
}

impl MediaHost for Host {
    fn players(&self) -> Vec<PlayerState> {
        self.0
            .upgrade()
            .map(|b| b.media.players())
            .unwrap_or_default()
    }

    fn artwork(&self, key: &str) -> Option<Vec<u8>> {
        self.0.upgrade()?.media.artwork(key)
    }

    fn peer_players(&self, from: &PeerInfo, _players: &[PlayerState]) {
        self.with(|b| {
            lock(&b.players_at).insert(from.id, now_ms());
            b.emit();
        });
    }

    fn peer_artwork(&self, from: &PeerInfo, key: &str, data: &[u8]) {
        self.with(|b| {
            let mut art = lock(&b.peer_art);
            // Keep a few tracks' worth per device.
            if art.len() > 40 {
                art.clear();
            }
            art.insert((from.id, key.to_owned()), data_url(data));
            drop(art);
            let _ = b.app.emit_to("main", "artwork", (from.id.to_string(), key));
        });
    }

    fn command(&self, from: &PeerInfo, player: &str, action: MediaAction) {
        tracing::debug!(device = %from.id, player, ?action, "media command from a phone");
        self.with(|b| b.media.control(player, action));
    }
}

impl SmsHost for Host {
    fn received(&self, from: &PeerInfo, message: &Message, _name: Option<&str>) {
        self.with(|b| {
            // The phone's own notification is mirrored already; the window just refreshes.
            let _ = b
                .app
                .emit_to("main", "sms", (from.id.to_string(), message.thread_id));
        });
    }

    fn status(&self, from: &PeerInfo, ok: bool, detail: &str) {
        self.with(|b| {
            tracing::info!(device = %from.id, ok, detail, "message send result");
            if !ok {
                b.notify(
                    "Couldn't send the message",
                    &format!("{detail}\nOn {}", from.name),
                );
            }
            let _ = b
                .app
                .emit_to("main", "sms-status", (from.id.to_string(), ok, detail));
        });
    }
}

impl TelephonyHost for Host {
    fn call(&self, from: &PeerInfo, event: &CallEvent) {
        self.with(|b| {
            tracing::info!(device = %from.id, state = ?event.state, "call");
            b.call_changed(
                from.id,
                &from.name,
                event.state,
                event.caller(),
                event.number.as_deref().unwrap_or(""),
            );
        });
    }
}

impl ScreenHost for Host {
    // This PC's screen, for a phone or another PC.
    fn start_sharing(&self, from: &PeerInfo) -> std::result::Result<(), String> {
        let b = self.0.upgrade().ok_or("Pairly is stopping")?;
        if !b.settings().share_screen {
            return Err("this PC doesn't share its screen (see Pairly's settings)".into());
        }
        b.screens.start_cast(from)
    }

    fn stop_sharing(&self, from: &PeerInfo) {
        self.with(|b| b.screens.stop_cast(from.id, None));
    }

    fn input(&self, from: &PeerInfo, input: ScreenInput) {
        self.with(|b| b.screens.cast_input(from, input));
    }

    // A phone's (or another PC's) screen, on this PC.
    fn started(&self, from: &PeerInfo, width: u32, height: u32) {
        self.with(|b| b.screens.viewer_started(from.id, width, height));
    }

    fn frame(&self, from: &PeerInfo, frame: ScreenFrame) {
        self.with(|b| b.screens.viewer_frame(from.id, &frame));
    }

    fn stopped(&self, from: &PeerInfo, reason: &str) {
        self.with(|b| b.screens.viewer_stopped(from.id, reason));
    }

    fn disconnected(&self, peer: DeviceId) {
        self.with(|b| {
            b.screens.viewer_stopped(peer, "the phone disconnected");
            b.screens.stop_cast(peer, None);
        });
    }
}
