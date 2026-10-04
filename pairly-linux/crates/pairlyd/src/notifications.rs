//! Desktop notifications on Linux, all through `org.freedesktop.Notifications` (works with
//! swaync, mako, dunst, GNOME and Plasma).
//!
//! * **Mirrors**: notifications from paired devices are shown with their action buttons and a
//!   reply (inline if the server supports `inline-reply`, otherwise via a small `pairly-gtk`
//!   dialog). User dismissals, actions and replies are sent back to the source device.
//! * **Monitor**: a second connection becomes a D-Bus monitor and watches every `Notify` call,
//!   its reply (the id) and `NotificationClosed`, to forward this PC's notifications. Calls from
//!   pairlyd's own connection are skipped, so mirrors never echo back.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures_util::StreamExt;
use pairly_core::{DeviceId, PeerInfo};
use pairly_plugins::notification::{Notification, NotificationHost, NotificationPlugin};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use zbus::message::Type as MessageType;
use zbus::zvariant::{OwnedValue, Value};
use zbus::{Connection, MatchRule, MessageStream};

const SERVICE: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";
const IFACE: &str = "org.freedesktop.Notifications";
/// Hint we put on mirrors so no notification tool ever forwards them again.
const ORIGIN_HINT: &str = "x-pairly-origin";
/// `NotificationClosed` reasons (freedesktop spec).
const CLOSED_EXPIRED: u32 = 1;
const CLOSED_BY_USER: u32 = 2;
const CLOSED_BY_CALL: u32 = 3;

/// How replies to mirrored notifications are typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReplyMode {
    /// Inline if the server supports it and hasn't turned it off, otherwise a dialog.
    Auto,
    /// A text field inside the notification (`inline-reply`).
    Inline,
    /// A "Reply…" button that opens a small `pairly-gtk` window.
    Dialog,
}

#[derive(Debug, Clone)]
pub struct Settings {
    /// Forward this PC's notifications to paired devices.
    pub send: bool,
    /// Show notifications from paired devices.
    pub show: bool,
    /// App names (as sent by the app, case-insensitive) never forwarded.
    pub ignore_apps: Vec<String>,
    pub reply: ReplyMode,
    /// Closing a mirrored notification here clears it on the phone too.
    pub dismiss_on_phone: bool,
}

#[derive(Debug)]
pub enum Cmd {
    Show(PeerInfo, Notification),
    Remove(DeviceId, String),
    Sync(DeviceId, Vec<String>),
    DismissLocal(String),
}

/// The plugin's view of the desktop: forwards everything to the [`run`] task.
pub struct LinuxNotificationHost {
    tx: mpsc::UnboundedSender<Cmd>,
}

impl LinuxNotificationHost {
    pub fn new() -> (Self, mpsc::UnboundedReceiver<Cmd>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self { tx }, rx)
    }
}

impl NotificationHost for LinuxNotificationHost {
    fn show(&self, from: &PeerInfo, notification: &Notification) {
        let _ = self.tx.send(Cmd::Show(from.clone(), notification.clone()));
    }

    fn remove(&self, from: &PeerInfo, id: &str) {
        let _ = self.tx.send(Cmd::Remove(from.id, id.to_owned()));
    }

    fn sync(&self, from: &PeerInfo, active: &[String]) {
        let _ = self.tx.send(Cmd::Sync(from.id, active.to_vec()));
    }

    fn dismiss_local(&self, id: &str) {
        let _ = self.tx.send(Cmd::DismissLocal(id.to_owned()));
    }

    fn action_local(&self, id: &str, action: &str) {
        // The spec gives no way to trigger another app's notification action.
        debug!(id, action, "actions on PC notifications aren't supported");
    }

    fn reply_local(&self, id: &str, _text: &str) {
        debug!(id, "replies to PC notifications aren't supported");
    }
}

/// Show a plain notification of our own (ping, pairing) without waiting for the server.
pub fn show_simple(conn: &Connection, summary: impl Into<String>, body: impl Into<String>) {
    let (conn, summary, body) = (conn.clone(), summary.into(), body.into());
    tokio::spawn(async move { notify(&conn, &summary, &body, &[], false).await });
}

/// Show a notification of our own with `(key, label)` buttons. A `persistent` one stays until
/// answered. Returns the server's id for matching `ActionInvoked`.
pub async fn notify(
    conn: &Connection,
    summary: &str,
    body: &str,
    actions: &[(&str, &str)],
    persistent: bool,
) -> Option<u32> {
    let hints: HashMap<&str, Value<'_>> = HashMap::new();
    let actions: Vec<&str> = actions.iter().flat_map(|(k, l)| [*k, *l]).collect();
    let timeout = if persistent { 0i32 } else { -1 };
    let reply = conn
        .call_method(
            Some(SERVICE),
            PATH,
            Some(IFACE),
            "Notify",
            &(
                "Pairly",
                0u32,
                "io.github.abhilesh1412.Pairly",
                summary,
                body,
                actions,
                hints,
                timeout,
            ),
        )
        .await;
    match reply.and_then(|r| r.body().deserialize::<u32>()) {
        Ok(id) => Some(id),
        Err(e) => {
            debug!(error = %e, "could not show notification");
            None
        }
    }
}

pub async fn supports_markup(conn: &Connection) -> bool {
    let caps: Vec<String> = match conn
        .call_method(Some(SERVICE), PATH, Some(IFACE), "GetCapabilities", &())
        .await
    {
        Ok(reply) => reply.body().deserialize().unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    caps.iter().any(|c| c == "body-markup")
}

pub fn escape_markup(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Plain text from a notification body that may contain the spec's small markup subset.
fn strip_markup(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

struct Mirror {
    peer: DeviceId,
    id: String,
    title: String,
    /// A button or reply was used: the server closes the popup afterwards, which isn't a
    /// dismissal (the phone's app updates or clears its notification itself).
    answered: bool,
}

struct Mirrors {
    conn: Connection,
    plugin: Arc<NotificationPlugin>,
    markup: bool,
    inline_reply: bool,
    dismiss_on_phone: bool,
    by_key: HashMap<(DeviceId, String), u32>,
    by_server: HashMap<u32, Mirror>,
}

impl Mirrors {
    async fn show(&mut self, from: &PeerInfo, n: &Notification) {
        let key = (from.id, n.id.clone());
        let replaces = self.by_key.get(&key).copied().unwrap_or(0);
        let mut actions: Vec<String> = Vec::new();
        for a in &n.actions {
            actions.push(format!("a:{}", a.key));
            actions.push(a.label.clone());
        }
        if n.can_reply {
            if self.inline_reply {
                actions.extend(["inline-reply".to_owned(), "Reply".to_owned()]);
            } else {
                actions.extend(["reply".to_owned(), "Reply…".to_owned()]);
            }
        }
        let mut hints: HashMap<&str, Value<'_>> = HashMap::new();
        hints.insert(ORIGIN_HINT, Value::from(from.id.to_string()));
        if n.silent {
            hints.insert("suppress-sound", Value::from(true));
        }
        if let Some(icon) = &n.icon {
            let (w, h) = (i32::from(icon.width), i32::from(icon.height));
            hints.insert(
                "image-data",
                Value::from((w, h, w * 4, true, 8i32, 4i32, icon.rgba.clone())),
            );
        }
        let app = format!("{} · {}", n.app, from.name);
        let body = if self.markup {
            escape_markup(&n.text)
        } else {
            n.text.clone()
        };
        let reply = self
            .conn
            .call_method(
                Some(SERVICE),
                PATH,
                Some(IFACE),
                "Notify",
                &(
                    app.as_str(),
                    replaces,
                    "",
                    n.title.as_str(),
                    body.as_str(),
                    actions,
                    hints,
                    -1i32,
                ),
            )
            .await;
        match reply.and_then(|m| m.body().deserialize::<u32>()) {
            Ok(server_id) => {
                if replaces != 0 && replaces != server_id {
                    self.by_server.remove(&replaces);
                }
                self.by_key.insert(key, server_id);
                self.by_server.insert(
                    server_id,
                    Mirror {
                        peer: from.id,
                        id: n.id.clone(),
                        title: n.title.clone(),
                        answered: false,
                    },
                );
            }
            Err(e) => warn!(error = %e, "could not show mirrored notification"),
        }
    }

    async fn close(&mut self, peer: DeviceId, id: &str) {
        if let Some(server_id) = self.by_key.remove(&(peer, id.to_owned())) {
            self.by_server.remove(&server_id);
            close(&self.conn, server_id).await;
        }
    }

    async fn sync(&mut self, peer: DeviceId, active: &[String]) {
        let stale: Vec<String> = self
            .by_key
            .keys()
            .filter(|(p, id)| *p == peer && !active.contains(id))
            .map(|(_, id)| id.clone())
            .collect();
        for id in stale {
            self.close(peer, &id).await;
        }
    }

    fn on_action(&mut self, server_id: u32, key: &str) {
        let Some(m) = self.by_server.get_mut(&server_id) else {
            return;
        };
        m.answered = true;
        if let Some(action) = key.strip_prefix("a:") {
            let _ = self.plugin.request_action(m.peer, &m.id, action);
        } else if key == "reply" {
            open_reply_dialog(m);
        }
    }

    fn on_reply(&mut self, server_id: u32, text: &str) {
        if let Some(m) = self.by_server.get_mut(&server_id) {
            m.answered = true;
            match self.plugin.request_reply(m.peer, &m.id, text) {
                Ok(()) => info!(peer = %m.peer, "inline reply sent"),
                Err(e) => warn!(peer = %m.peer, error = %e, "inline reply failed"),
            }
        }
    }

    fn on_closed(&mut self, server_id: u32, reason: u32) {
        let Some(m) = self.by_server.remove(&server_id) else {
            return;
        };
        self.by_key.remove(&(m.peer, m.id.clone()));
        // Only a deliberate dismissal propagates: an expired popup, or one closed because a
        // button was used, must not clear the phone.
        if reason == CLOSED_BY_USER && !m.answered && self.dismiss_on_phone {
            let _ = self.plugin.request_dismiss(m.peer, &m.id);
        }
    }
}

pub async fn close(conn: &Connection, server_id: u32) {
    if let Err(e) = conn
        .call_method(
            Some(SERVICE),
            PATH,
            Some(IFACE),
            "CloseNotification",
            &(server_id,),
        )
        .await
    {
        debug!(server_id, error = %e, "CloseNotification failed");
    }
}

/// Fallback for servers without inline replies: a small `pairly-gtk` window.
fn open_reply_dialog(m: &Mirror) {
    let exe = std::env::current_exe()
        .ok()
        .map(|p| p.with_file_name("pairly-gtk"))
        .filter(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from("pairly-gtk"));
    let spawned = std::process::Command::new(exe)
        .args(["--reply", &m.peer.to_string(), &m.id, &m.title])
        .spawn();
    match spawned {
        Ok(mut child) => drop(std::thread::spawn(move || child.wait())),
        Err(e) => warn!(error = %e, "could not open the reply dialog"),
    }
}

/// Mirror remote notifications and forward local ones until the connection closes.
pub async fn run(
    conn: Connection,
    plugin: Arc<NotificationPlugin>,
    mut cmds: mpsc::UnboundedReceiver<Cmd>,
    settings: Settings,
) {
    let caps: Vec<String> = match conn
        .call_method(Some(SERVICE), PATH, Some(IFACE), "GetCapabilities", &())
        .await
    {
        Ok(reply) => reply.body().deserialize().unwrap_or_default(),
        Err(e) => {
            warn!(error = %e, "no notification server; notifications are off until it appears");
            Vec::new()
        }
    };
    let inline_reply = match settings.reply {
        ReplyMode::Inline => true,
        ReplyMode::Dialog => false,
        ReplyMode::Auto => {
            caps.iter().any(|c| c == "inline-reply") && !server_disables_inline_replies(&conn).await
        }
    };
    let mut mirrors = Mirrors {
        conn: conn.clone(),
        plugin: plugin.clone(),
        markup: caps.iter().any(|c| c == "body-markup"),
        inline_reply,
        dismiss_on_phone: settings.dismiss_on_phone,
        by_key: HashMap::new(),
        by_server: HashMap::new(),
    };
    info!(
        inline_reply = mirrors.inline_reply,
        send = settings.send,
        show = settings.show,
        "notifications ready"
    );

    if settings.send
        && let Some(own) = conn.unique_name().map(|n| n.to_string())
    {
        let (plugin, settings) = (plugin.clone(), settings.clone());
        tokio::spawn(async move {
            if let Err(e) = monitor(own, plugin, settings).await {
                warn!(error = %e, "can't watch this PC's notifications");
            }
        });
    }

    let signals = async {
        let proxy = zbus::Proxy::new(&conn, SERVICE, PATH, IFACE).await?;
        let actions = proxy.receive_signal("ActionInvoked").await?;
        let replies = proxy.receive_signal("NotificationReplied").await?;
        let closed = proxy.receive_signal("NotificationClosed").await?;
        Ok::<_, zbus::Error>((actions, replies, closed))
    };
    let (mut actions, mut replies, mut closed) = match signals.await {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "can't listen to notification signals");
            return;
        }
    };

    loop {
        tokio::select! {
            cmd = cmds.recv() => match cmd {
                Some(Cmd::Show(from, n)) if settings.show => mirrors.show(&from, &n).await,
                Some(Cmd::Show(..)) => {}
                Some(Cmd::Remove(peer, id)) => mirrors.close(peer, &id).await,
                Some(Cmd::Sync(peer, active)) => mirrors.sync(peer, &active).await,
                Some(Cmd::DismissLocal(id)) => {
                    if let Ok(server_id) = id.parse::<u32>() {
                        close(&conn, server_id).await;
                    }
                }
                None => return,
            },
            Some(m) = actions.next() => {
                if let Ok((id, key)) = m.body().deserialize::<(u32, String)>() {
                    mirrors.on_action(id, &key);
                }
            }
            Some(m) = replies.next() => {
                if let Ok((id, text)) = m.body().deserialize::<(u32, String)>() {
                    mirrors.on_reply(id, &text);
                }
            }
            Some(m) = closed.next() => {
                if let Ok((id, reason)) = m.body().deserialize::<(u32, u32)>() {
                    mirrors.on_closed(id, reason);
                }
            }
        }
    }
}

/// swaync advertises `inline-reply` even when its config turns the reply field off, and then
/// hides the action entirely. Read its config so we fall back to the dialog instead.
async fn server_disables_inline_replies(conn: &Connection) -> bool {
    let info = conn
        .call_method(
            Some(SERVICE),
            PATH,
            Some(IFACE),
            "GetServerInformation",
            &(),
        )
        .await;
    let Ok((name, ..)) =
        info.and_then(|m| m.body().deserialize::<(String, String, String, String)>())
    else {
        return false;
    };
    if name != "SwayNotificationCenter" {
        return false;
    }
    let config_home = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
        });
    let Ok(text) = std::fs::read_to_string(config_home.join("swaync/config.json")) else {
        return false;
    };
    let disabled = serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|v| {
            v.get("notification-inline-replies")
                .and_then(serde_json::Value::as_bool)
        })
        .is_some_and(|enabled| !enabled);
    if disabled {
        info!("swaync has inline replies turned off; using a reply dialog");
    }
    disabled
}

type NotifyArgs = (
    String,
    u32,
    String,
    String,
    String,
    Vec<String>,
    HashMap<String, OwnedValue>,
    i32,
);

fn hint_bool(hints: &HashMap<String, OwnedValue>, key: &str) -> bool {
    hints
        .get(key)
        .and_then(|v| bool::try_from(v).ok())
        .unwrap_or(false)
}

/// Forward this PC's notifications. Runs on its own connection: a monitor can't send.
async fn monitor(
    own: String,
    plugin: Arc<NotificationPlugin>,
    settings: Settings,
) -> zbus::Result<()> {
    let conn = zbus::connection::Builder::session()?.build().await?;
    let rules = [
        "type='method_call',interface='org.freedesktop.Notifications',member='Notify'",
        // The reply carries the notification's id; matched to its call below.
        "type='method_return'",
        "type='signal',interface='org.freedesktop.Notifications',member='NotificationClosed'",
    ]
    .into_iter()
    .map(MatchRule::try_from)
    .collect::<Result<Vec<_>, _>>()?;
    zbus::fdo::MonitoringProxy::new(&conn)
        .await?
        .become_monitor(&rules, 0)
        .await?;
    info!("watching this PC's notifications");

    let ignore: Vec<String> = settings
        .ignore_apps
        .iter()
        .map(|a| a.to_lowercase())
        .collect();
    // (caller, call serial) -> notification waiting for its id.
    let mut pending: HashMap<(String, u32), (Instant, Notification)> = HashMap::new();
    let mut stream = MessageStream::from(&conn);
    while let Some(msg) = stream.next().await {
        let msg = msg?;
        let header = msg.header();
        match header.message_type() {
            MessageType::MethodCall => {
                let Some(sender) = header.sender().map(|s| s.to_string()) else {
                    continue;
                };
                if sender == own {
                    continue; // our own mirrors and pings
                }
                let Ok((app, _replaces, _icon, summary, body, _actions, hints, _timeout)) =
                    msg.body().deserialize::<NotifyArgs>()
                else {
                    continue;
                };
                let skip = hints.contains_key(ORIGIN_HINT)
                    || hint_bool(&hints, "transient")
                    // On-screen displays (volume, brightness) replace themselves constantly.
                    || hints.contains_key("x-canonical-private-synchronous")
                    || hints.contains_key("synchronous")
                    || ignore.contains(&app.to_lowercase())
                    || (summary.trim().is_empty() && body.trim().is_empty());
                if skip {
                    continue;
                }
                let n = Notification {
                    id: String::new(),
                    app: if app.is_empty() {
                        "Linux".to_owned()
                    } else {
                        app
                    },
                    title: summary,
                    text: strip_markup(&body),
                    time: now_ms(),
                    actions: Vec::new(),
                    can_reply: false,
                    icon: None,
                    silent: hint_bool(&hints, "suppress-sound"),
                };
                let serial = msg.primary_header().serial_num().get();
                pending.retain(|_, (t, _)| t.elapsed() < Duration::from_secs(10));
                pending.insert((sender, serial), (Instant::now(), n));
            }
            MessageType::MethodReturn => {
                let (Some(dest), Some(serial)) = (header.destination(), header.reply_serial())
                else {
                    continue;
                };
                let Some((_, mut n)) = pending.remove(&(dest.to_string(), serial.get())) else {
                    continue;
                };
                if let Ok(id) = msg.body().deserialize::<u32>() {
                    n.id = id.to_string();
                    plugin.posted(n);
                }
            }
            MessageType::Signal => {
                if let Ok((id, reason)) = msg.body().deserialize::<(u32, u32)>()
                    && matches!(reason, CLOSED_BY_USER | CLOSED_BY_CALL)
                {
                    plugin.removed(&id.to_string());
                } else {
                    let _ = CLOSED_EXPIRED; // expired popups stay in the center: keep the mirror
                }
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markup_round_trips() {
        assert_eq!(
            strip_markup("<b>Build</b> &amp; <a href=\"x\">deploy</a> &lt;ok&gt;"),
            "Build & deploy <ok>"
        );
        assert_eq!(escape_markup("a < b & c"), "a &lt; b &amp; c");
    }
}
