//! Notification sync in both directions (`plan.md` §8).
//!
//! The device a notification belongs to (its *source*) sends `posted` / `removed` / `active`.
//! A device showing a *mirror* sends requests back: `dismiss`, `action`, `reply`. Only the
//! source ever says a notification is gone, so dismissals can't loop.
//!
//! On every (re)connect the source resends what is currently showing, then `active` with the
//! full id list, so mirrors of notifications that went away while offline are cleaned up.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use async_trait::async_trait;
use pairly_core::{
    CoreError, DeviceId, Envelope, OutboundPacket, PacketBody, PeerInfo, Plugin, PluginCtx,
    Priority, Result,
};
use serde::{Deserialize, Serialize};
use tracing::debug;

/// How many of our own notifications we remember for resending after a reconnect.
const MAX_ACTIVE: usize = 50;
const MAX_ICON_SIDE: u16 = 128;
const MAX_ACTIONS: usize = 5;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationAction {
    /// Opaque to the mirror; handed back in [`Action`].
    pub key: String,
    pub label: String,
}

/// Raw RGBA pixels, row-major, no padding (`width * height * 4` bytes). Raw so neither side
/// needs an image codec: Android bitmaps and the freedesktop `image-data` hint are both RGBA.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Icon {
    pub width: u16,
    pub height: u16,
    #[serde(with = "serde_bytes")]
    pub rgba: Vec<u8>,
}

impl std::fmt::Debug for Icon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Icon({}x{})", self.width, self.height)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notification {
    /// Stable per notification on its source device; updates reuse it.
    pub id: String,
    /// App display name, e.g. "WhatsApp".
    pub app: String,
    pub title: String,
    pub text: String,
    /// Unix milliseconds.
    pub time: u64,
    #[serde(default)]
    pub actions: Vec<NotificationAction>,
    /// The source accepts a text reply ([`Reply`]).
    #[serde(default)]
    pub can_reply: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<Icon>,
    /// Show without sound/heads-up (e.g. an update to an existing conversation).
    #[serde(default)]
    pub silent: bool,
}

impl PacketBody for Notification {
    const TYPE: &'static str = "notification.posted";
}

fn truncate(s: &mut String, max_chars: usize) {
    if let Some((i, _)) = s.char_indices().nth(max_chars) {
        s.truncate(i);
    }
}

impl Notification {
    /// Clamp an untrusted notification to sane sizes instead of rejecting it outright.
    fn sanitize(mut self) -> Option<Self> {
        if self.id.is_empty() || self.id.len() > 256 {
            return None;
        }
        truncate(&mut self.app, 64);
        truncate(&mut self.title, 256);
        truncate(&mut self.text, 4096);
        self.actions.truncate(MAX_ACTIONS);
        for a in &mut self.actions {
            truncate(&mut a.key, 64);
            truncate(&mut a.label, 64);
        }
        if let Some(icon) = &self.icon {
            let ok = icon.width > 0
                && icon.height > 0
                && icon.width <= MAX_ICON_SIDE
                && icon.height <= MAX_ICON_SIDE
                && icon.rgba.len() == usize::from(icon.width) * usize::from(icon.height) * 4;
            if !ok {
                self.icon = None;
            }
        }
        Some(self)
    }
}

/// The source removed one of its notifications.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Removed {
    pub id: String,
}
impl PacketBody for Removed {
    const TYPE: &'static str = "notification.removed";
}

/// The source's complete list of showing notifications (sent after each connect).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Active {
    pub ids: Vec<String>,
}
impl PacketBody for Active {
    const TYPE: &'static str = "notification.active";
}

/// A mirror asks the source to dismiss the original.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dismiss {
    pub id: String,
}
impl PacketBody for Dismiss {
    const TYPE: &'static str = "notification.dismiss";
}

/// A mirror asks the source to run one of the notification's actions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Action {
    pub id: String,
    pub action: String,
}
impl PacketBody for Action {
    const TYPE: &'static str = "notification.action";
}

/// A mirror sends a text reply through the source's notification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reply {
    pub id: String,
    pub text: String,
}
impl PacketBody for Reply {
    const TYPE: &'static str = "notification.reply";
}

/// What the notification plugin needs from the OS. Calls arrive on Rust worker threads.
pub trait NotificationHost: Send + Sync + 'static {
    /// Show or update a notification mirrored from `from`.
    fn show(&self, from: &PeerInfo, notification: &Notification);
    /// Remove a mirrored notification.
    fn remove(&self, from: &PeerInfo, id: &str);
    /// `from` has exactly these notifications showing: drop any other mirrors from it.
    fn sync(&self, from: &PeerInfo, active: &[String]);
    /// A peer asks to dismiss one of our own notifications.
    fn dismiss_local(&self, id: &str);
    /// A peer asks to run an action on one of our own notifications.
    fn action_local(&self, id: &str, action: &str);
    /// A peer sends a reply through one of our own notifications.
    fn reply_local(&self, id: &str, text: &str);
}

pub struct NotificationPlugin {
    host: Arc<dyn NotificationHost>,
    peers: Mutex<HashMap<DeviceId, PluginCtx>>,
    /// Our own showing notifications, oldest first.
    active: Mutex<VecDeque<Notification>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn reliable<T: PacketBody>(body: &T) -> Result<OutboundPacket> {
    OutboundPacket::reliable(body, Priority::Interactive)
}

impl NotificationPlugin {
    pub fn new(host: Arc<dyn NotificationHost>) -> Arc<Self> {
        Arc::new(Self {
            host,
            peers: Mutex::default(),
            active: Mutex::default(),
        })
    }

    fn broadcast(&self, packet: &OutboundPacket) {
        for ctx in lock(&self.peers).values() {
            if ctx.peer_accepts(&packet.ty) {
                let _ = ctx.send(packet.clone());
            }
        }
    }

    fn to_peer(&self, peer: DeviceId, packet: OutboundPacket) -> Result<()> {
        let ctx = lock(&self.peers)
            .get(&peer)
            .cloned()
            .ok_or(CoreError::NotConnected(peer))?;
        ctx.send(packet)?;
        Ok(())
    }

    /// One of our notifications appeared or changed: mirror it on every connected peer.
    pub fn posted(&self, notification: Notification) {
        let Some(notification) = notification.sanitize() else {
            return;
        };
        {
            let mut active = lock(&self.active);
            active.retain(|n| n.id != notification.id);
            active.push_back(notification.clone());
            while active.len() > MAX_ACTIVE {
                active.pop_front();
            }
        }
        if let Ok(packet) = reliable(&notification) {
            self.broadcast(&packet);
        }
    }

    /// One of our notifications went away.
    pub fn removed(&self, id: &str) {
        let was_known = {
            let mut active = lock(&self.active);
            let before = active.len();
            active.retain(|n| n.id != id);
            active.len() != before
        };
        if was_known && let Ok(packet) = reliable(&Removed { id: id.to_owned() }) {
            self.broadcast(&packet);
        }
    }

    /// The user dismissed a mirror of `peer`'s notification here.
    pub fn request_dismiss(&self, peer: DeviceId, id: &str) -> Result<()> {
        self.to_peer(peer, reliable(&Dismiss { id: id.to_owned() })?)
    }

    pub fn request_action(&self, peer: DeviceId, id: &str, action: &str) -> Result<()> {
        self.to_peer(
            peer,
            reliable(&Action {
                id: id.to_owned(),
                action: action.to_owned(),
            })?,
        )
    }

    pub fn request_reply(&self, peer: DeviceId, id: &str, text: &str) -> Result<()> {
        self.to_peer(
            peer,
            reliable(&Reply {
                id: id.to_owned(),
                text: text.to_owned(),
            })?,
        )
    }

    fn on_packet_inner(&self, ctx: &PluginCtx, packet: &Envelope) -> Result<()> {
        let from = ctx.peer_info();
        match packet.ty.as_str() {
            Notification::TYPE => {
                if let Some(n) = packet.body::<Notification>()?.sanitize() {
                    self.host.show(&from, &n);
                }
            }
            Removed::TYPE => self.host.remove(&from, &packet.body::<Removed>()?.id),
            Active::TYPE => self.host.sync(&from, &packet.body::<Active>()?.ids),
            Dismiss::TYPE => self.host.dismiss_local(&packet.body::<Dismiss>()?.id),
            Action::TYPE => {
                let a = packet.body::<Action>()?;
                self.host.action_local(&a.id, &a.action);
            }
            Reply::TYPE => {
                let r = packet.body::<Reply>()?;
                self.host.reply_local(&r.id, &r.text);
            }
            _ => {}
        }
        Ok(())
    }
}

const TYPES: &[&str] = &[
    Notification::TYPE,
    Removed::TYPE,
    Active::TYPE,
    Dismiss::TYPE,
    Action::TYPE,
    Reply::TYPE,
];

#[async_trait]
impl Plugin for NotificationPlugin {
    fn id(&self) -> &'static str {
        "notification"
    }

    fn incoming(&self) -> &'static [&'static str] {
        TYPES
    }

    fn outgoing(&self) -> &'static [&'static str] {
        TYPES
    }

    async fn on_connected(&self, ctx: &PluginCtx) {
        lock(&self.peers).insert(ctx.peer(), ctx.clone());
        if !ctx.peer_accepts(Notification::TYPE) {
            return;
        }
        // Resync: everything still showing, then the full list so stale mirrors go away.
        let snapshot: Vec<Notification> = lock(&self.active).iter().cloned().collect();
        for n in &snapshot {
            if let Ok(packet) = reliable(n) {
                let _ = ctx.send(packet);
            }
        }
        let ids = snapshot.into_iter().map(|n| n.id).collect();
        if let Ok(packet) = reliable(&Active { ids }) {
            let _ = ctx.send(packet);
        }
    }

    async fn on_packet(&self, ctx: &PluginCtx, packet: Envelope) {
        if let Err(e) = self.on_packet_inner(ctx, &packet) {
            debug!(peer = %ctx.peer(), ty = %packet.ty, error = %e, "bad notification packet");
        }
    }

    async fn on_disconnected(&self, peer: DeviceId) {
        lock(&self.peers).remove(&peer);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(id: &str) -> Notification {
        Notification {
            id: id.into(),
            app: "Chat".into(),
            title: "Ana".into(),
            text: "hi".into(),
            time: 1,
            actions: vec![],
            can_reply: true,
            icon: None,
            silent: false,
        }
    }

    #[test]
    fn sanitize_clamps_untrusted_input() {
        let mut big = n("x");
        big.title = "t".repeat(1000);
        big.actions = (0..20)
            .map(|i| NotificationAction {
                key: i.to_string(),
                label: "a".into(),
            })
            .collect();
        big.icon = Some(Icon {
            width: 2,
            height: 2,
            rgba: vec![0; 3],
        }); // wrong length
        let clean = big.sanitize().unwrap();
        assert_eq!(clean.title.chars().count(), 256);
        assert_eq!(clean.actions.len(), MAX_ACTIONS);
        assert!(clean.icon.is_none());
        assert!(n("").sanitize().is_none());
        let mut multibyte = n("x");
        multibyte.title = "é".repeat(300);
        assert_eq!(multibyte.sanitize().unwrap().title.chars().count(), 256);
    }
}
