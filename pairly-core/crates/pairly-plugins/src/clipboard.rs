//! Clipboard sync: text, and images (a screenshot copied on one device pastes on the other).
//! Each device remembers the last thing it sent or received, so applying a remote clipboard
//! locally doesn't bounce straight back.
//!
//! An image can be bigger than one packet, so it travels in pieces of [`IMAGE_PIECE`] bytes,
//! reassembled on arrival; only then does it go on the clipboard.

use std::collections::HashMap;
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
/// The biggest image passed through the clipboard (a 4K screenshot is a few MB).
pub const MAX_IMAGE_BYTES: usize = 32 * 1024 * 1024;
/// An image's bytes per packet (packets are at most 1 MB).
pub const IMAGE_PIECE: usize = 768 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardSet {
    pub text: String,
}

impl PacketBody for ClipboardSet {
    const TYPE: &'static str = "clipboard.set";
}

/// A piece of a copied image: `data` goes at `offset` of `total` bytes. Pieces of one image
/// share an `id` and arrive in order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardImage {
    pub id: u64,
    /// `image/png`, `image/jpeg`…
    pub mime: String,
    pub total: u64,
    pub offset: u64,
    #[serde(with = "serde_bytes")]
    pub data: Vec<u8>,
}

impl PacketBody for ClipboardImage {
    const TYPE: &'static str = "clipboard.image";
}

pub trait ClipboardHost: Send + Sync + 'static {
    /// Put text from `from` on the local clipboard.
    fn set_clipboard(&self, from: &PeerInfo, text: &str);

    /// Put an image from `from` on the local clipboard (`mime` like `image/png`).
    fn set_clipboard_image(&self, _from: &PeerInfo, _mime: &str, _data: &[u8]) {}
}

/// An image arriving piece by piece.
struct Incoming {
    id: u64,
    mime: String,
    total: usize,
    data: Vec<u8>,
}

pub struct ClipboardPlugin {
    host: Arc<dyn ClipboardHost>,
    peers: Peers,
    last: Mutex<Option<u64>>,
    next_image: std::sync::atomic::AtomicU64,
    incoming: Mutex<HashMap<DeviceId, Incoming>>,
}

fn digest(text: &str) -> u64 {
    let mut h = DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

fn usable(text: &str) -> bool {
    !text.is_empty() && text.len() <= MAX_TEXT_BYTES
}

fn image_digest(data: &[u8]) -> u64 {
    let mut h = DefaultHasher::new();
    data.hash(&mut h);
    h.finish()
}

/// An image file's type from its first bytes, for a copied file (file managers copy its path,
/// not the picture): PNG, JPEG, GIF, WebP or BMP.
pub fn image_type(data: &[u8]) -> Option<&'static str> {
    match data {
        [0x89, b'P', b'N', b'G', ..] => Some("image/png"),
        [0xFF, 0xD8, 0xFF, ..] => Some("image/jpeg"),
        [b'G', b'I', b'F', b'8', ..] => Some("image/gif"),
        [
            b'R',
            b'I',
            b'F',
            b'F',
            _,
            _,
            _,
            _,
            b'W',
            b'E',
            b'B',
            b'P',
            ..,
        ] => Some("image/webp"),
        [b'B', b'M', ..] => Some("image/bmp"),
        _ => None,
    }
}

fn usable_image(mime: &str, data: &[u8]) -> bool {
    mime.starts_with("image/")
        && mime.len() <= 64
        && !data.is_empty()
        && data.len() <= MAX_IMAGE_BYTES
}

impl ClipboardPlugin {
    pub fn new(host: Arc<dyn ClipboardHost>) -> Arc<Self> {
        Arc::new(Self {
            host,
            peers: Peers::default(),
            last: Mutex::default(),
            next_image: std::sync::atomic::AtomicU64::new(1),
            incoming: Mutex::default(),
        })
    }

    /// Record a digest as the current shared clipboard. Returns false if it already was.
    fn remember_digest(&self, d: u64) -> bool {
        lock(&self.last).replace(d) != Some(d)
    }

    /// An image's pieces, ready to send.
    fn image_packets(&self, mime: &str, data: &[u8]) -> Result<Vec<OutboundPacket>> {
        let id = self
            .next_image
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let total = data.len() as u64;
        data.chunks(IMAGE_PIECE)
            .enumerate()
            .map(|(i, piece)| {
                OutboundPacket::reliable(
                    &ClipboardImage {
                        id,
                        mime: mime.to_owned(),
                        total,
                        offset: (i * IMAGE_PIECE) as u64,
                        data: piece.to_vec(),
                    },
                    Priority::Bulk,
                )
            })
            .collect()
    }

    /// The local clipboard now holds an image (automatic sync): send it to every connected
    /// device, unless it is what we just received or sent.
    pub fn local_image_changed(&self, mime: &str, data: &[u8]) {
        if !usable_image(mime, data) || !self.remember_digest(image_digest(data)) {
            return;
        }
        if let Ok(packets) = self.image_packets(mime, data) {
            for packet in &packets {
                self.peers.broadcast(packet);
            }
        }
    }

    /// Explicitly send an image to one device (always sent).
    pub fn send_image_to(&self, peer: DeviceId, mime: &str, data: &[u8]) -> Result<()> {
        if !usable_image(mime, data) {
            return Err(pairly_core::CoreError::Violation(
                "not an image, or too large",
            ));
        }
        self.remember_digest(image_digest(data));
        for packet in self.image_packets(mime, data)? {
            self.peers.send(peer, packet)?;
        }
        Ok(())
    }

    /// A piece of an image arrived: the whole image when it was the last.
    fn piece(&self, peer: DeviceId, piece: ClipboardImage) -> Option<(String, Vec<u8>)> {
        let total = usize::try_from(piece.total).ok()?;
        let mut incoming = lock(&self.incoming);
        if piece.offset == 0 {
            if !piece.mime.starts_with("image/")
                || piece.mime.len() > 64
                || total == 0
                || total > MAX_IMAGE_BYTES
            {
                return None;
            }
            incoming.insert(
                peer,
                Incoming {
                    id: piece.id,
                    mime: piece.mime,
                    total,
                    data: Vec::with_capacity(total),
                },
            );
        }
        let image = incoming.get_mut(&peer)?;
        // Out of order or from another image: give this one up.
        if image.id != piece.id
            || piece.offset != image.data.len() as u64
            || image.data.len() + piece.data.len() > image.total
        {
            incoming.remove(&peer);
            return None;
        }
        image.data.extend_from_slice(&piece.data);
        if image.data.len() < image.total {
            return None;
        }
        incoming.remove(&peer).map(|i| (i.mime, i.data))
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
        &[ClipboardSet::TYPE, ClipboardImage::TYPE]
    }

    fn outgoing(&self) -> &'static [&'static str] {
        &[ClipboardSet::TYPE, ClipboardImage::TYPE]
    }

    async fn on_connected(&self, ctx: &PluginCtx) {
        self.peers.insert(ctx);
    }

    async fn on_packet(&self, ctx: &PluginCtx, packet: Envelope) {
        if packet.ty == ClipboardImage::TYPE {
            match packet.body::<ClipboardImage>() {
                Ok(piece) => {
                    if let Some((mime, data)) = self.piece(ctx.peer(), piece) {
                        self.remember_digest(image_digest(&data));
                        self.host
                            .set_clipboard_image(&ctx.peer_info(), &mime, &data);
                    }
                }
                Err(e) => debug!(peer = %ctx.peer(), error = %e, "bad clipboard image"),
            }
            return;
        }
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
        lock(&self.incoming).remove(&peer);
    }
}
