//! Clipboard sync (text). Each device remembers the last text it sent or received, so applying
//! a remote clipboard locally doesn't bounce straight back.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use pairly_core::{
    DeviceId, Envelope, OutboundPacket, PacketBody, PeerInfo, Plugin, PluginCtx, Priority, Result,
};
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::peers::{Peers, lock};

/// Larger content belongs to file sharing (Phase 7).
pub const MAX_TEXT_BYTES: usize = 512 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardSet {
    pub text: String,
}

impl PacketBody for ClipboardSet {
    const TYPE: &'static str = "clipboard.set";
}

pub trait ClipboardHost: Send + Sync + 'static {
    /// Put text from `from` on the local clipboard.
    fn set_clipboard(&self, from: &PeerInfo, text: &str);
}

pub struct ClipboardPlugin {
    host: Arc<dyn ClipboardHost>,
    peers: Peers,
    last: Mutex<Option<u64>>,
}

fn digest(text: &str) -> u64 {
    let mut h = DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

fn usable(text: &str) -> bool {
    !text.is_empty() && text.len() <= MAX_TEXT_BYTES
}

impl ClipboardPlugin {
    pub fn new(host: Arc<dyn ClipboardHost>) -> Arc<Self> {
        Arc::new(Self {
            host,
            peers: Peers::default(),
            last: Mutex::default(),
        })
    }

    /// Record `text` as the current shared clipboard. Returns false if it already was.
    fn remember(&self, text: &str) -> bool {
        let d = digest(text);
        lock(&self.last).replace(d) != Some(d)
    }

    fn packet(text: &str) -> Result<OutboundPacket> {
        OutboundPacket::unreliable(
            &ClipboardSet {
                text: text.to_owned(),
            },
            Priority::Interactive,
        )
    }

    /// The local clipboard changed (automatic sync): send it to every connected device, unless
    /// it is what we just received or sent.
    pub fn local_changed(&self, text: &str) {
        if !usable(text) || !self.remember(text) {
            return;
        }
        if let Ok(packet) = Self::packet(text) {
            self.peers.broadcast(&packet);
        }
    }

    /// Explicitly send `text` to one device (always sent).
    pub fn send_to(&self, peer: DeviceId, text: &str) -> Result<()> {
        if !usable(text) {
            return Err(pairly_core::CoreError::Violation(
                "clipboard is empty or too large",
            ));
        }
        self.remember(text);
        self.peers.send(peer, Self::packet(text)?)
    }
}

#[async_trait]
impl Plugin for ClipboardPlugin {
    fn id(&self) -> &'static str {
        "clipboard"
    }

    fn incoming(&self) -> &'static [&'static str] {
        &[ClipboardSet::TYPE]
    }

    fn outgoing(&self) -> &'static [&'static str] {
        &[ClipboardSet::TYPE]
    }

    async fn on_connected(&self, ctx: &PluginCtx) {
        self.peers.insert(ctx);
    }

    async fn on_packet(&self, ctx: &PluginCtx, packet: Envelope) {
        match packet.body::<ClipboardSet>() {
            Ok(set) if usable(&set.text) => {
                self.remember(&set.text);
                self.host.set_clipboard(&ctx.peer_info(), &set.text);
            }
            Ok(_) => {}
            Err(e) => debug!(peer = %ctx.peer(), error = %e, "bad clipboard packet"),
        }
    }

    async fn on_disconnected(&self, peer: DeviceId) {
        self.peers.remove(peer);
    }
}
