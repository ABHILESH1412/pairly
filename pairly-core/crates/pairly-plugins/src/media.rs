//! Media control both ways: each device publishes its players (what's playing, position,
//! controls) and accepts commands for them. The PC shows the phone's player as an MPRIS player;
//! the phone shows the PC's as a media notification.
//!
//! Artwork travels once per track: [`PlayerState::art`] is a key, and the bytes follow in a
//! `media.art` packet the first time a peer needs them (`https` keys are fetched by the
//! receiver instead).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use pairly_core::{
    DeviceId, Envelope, OutboundPacket, PacketBody, PeerInfo, Plugin, PluginCtx, Priority, Result,
};
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::peers::{Peers, lock};

/// Larger artwork is not sent.
pub const MAX_ART_BYTES: usize = 512 * 1024;
const MAX_PLAYERS: usize = 16;
const MAX_TEXT: usize = 512;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayerState {
    /// Stable id on the sending device (MPRIS bus name, Android package).
    pub id: String,
    /// Human name of the player ("Firefox", "Spotify").
    pub name: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    /// Artwork key (see the module docs), if any.
    pub art: Option<String>,
    pub length_ms: Option<u64>,
    /// Position when this state was sent.
    pub position_ms: Option<u64>,
    pub playing: bool,
    pub can_play: bool,
    pub can_pause: bool,
    pub can_next: bool,
    pub can_previous: bool,
    pub can_seek: bool,
    /// 0–100, if the player has a volume.
    pub volume: Option<u8>,
}

impl PlayerState {
    fn clamp(mut self) -> Self {
        for s in [
            &mut self.id,
            &mut self.name,
            &mut self.title,
            &mut self.artist,
            &mut self.album,
        ] {
            if s.len() > MAX_TEXT {
                let mut end = MAX_TEXT;
                while !s.is_char_boundary(end) {
                    end -= 1;
                }
                s.truncate(end);
            }
        }
        if self.art.as_ref().is_some_and(|a| a.len() > 2048) {
            self.art = None;
        }
        self.volume = self.volume.map(|v| v.min(100));
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaPlayers {
    pub players: Vec<PlayerState>,
}

impl PacketBody for MediaPlayers {
    const TYPE: &'static str = "media.players";
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaArt {
    pub key: String,
    #[serde(with = "serde_bytes")]
    pub data: Vec<u8>,
}

impl PacketBody for MediaArt {
    const TYPE: &'static str = "media.art";
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaAction {
    Play,
    Pause,
    PlayPause,
    Stop,
    Next,
    Previous,
    /// Relative, in milliseconds.
    Seek(i64),
    SetPosition(u64),
    /// 0–100.
    SetVolume(u8),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaCommand {
    pub player: String,
    pub action: MediaAction,
}

impl PacketBody for MediaCommand {
    const TYPE: &'static str = "media.command";
}

pub trait MediaHost: Send + Sync + 'static {
    /// The players on this device right now.
    fn players(&self) -> Vec<PlayerState>;
    /// Artwork bytes for a key from [`MediaHost::players`] (at most [`MAX_ART_BYTES`]).
    fn artwork(&self, key: &str) -> Option<Vec<u8>>;
    /// A peer's players changed (an empty list: nothing is playing there, or it disconnected).
    fn peer_players(&self, from: &PeerInfo, players: &[PlayerState]);
    /// Artwork bytes for one of a peer's art keys.
    fn peer_artwork(&self, from: &PeerInfo, key: &str, data: &[u8]);
    /// A peer asks to control one of our players.
    fn command(&self, from: &PeerInfo, player: &str, action: MediaAction);
}

pub struct MediaPlugin {
    host: Arc<dyn MediaHost>,
    peers: Peers,
    /// Art keys each peer already has.
    art_sent: Mutex<HashMap<DeviceId, HashSet<String>>>,
    /// Last players each peer reported.
    remote: Mutex<HashMap<DeviceId, Vec<PlayerState>>>,
}

impl MediaPlugin {
    pub fn new(host: Arc<dyn MediaHost>) -> Arc<Self> {
        Arc::new(Self {
            host,
            peers: Peers::default(),
            art_sent: Mutex::default(),
            remote: Mutex::default(),
        })
    }

    fn players_packet(players: &[PlayerState]) -> Result<OutboundPacket> {
        let players = players
            .iter()
            .take(MAX_PLAYERS)
            .cloned()
            .map(PlayerState::clamp)
            .collect();
        OutboundPacket::reliable(&MediaPlayers { players }, Priority::Interactive)
    }

    /// Send artwork a peer hasn't got yet.
    fn send_art(&self, peer: DeviceId, players: &[PlayerState]) {
        for key in players.iter().filter_map(|p| p.art.as_deref()) {
            if key.starts_with("https://") {
                continue;
            }
            let fresh = lock(&self.art_sent)
                .entry(peer)
                .or_default()
                .insert(key.to_owned());
            if !fresh {
                continue;
            }
            let Some(data) = self.host.artwork(key).filter(|d| d.len() <= MAX_ART_BYTES) else {
                continue;
            };
            let art = MediaArt {
                key: key.to_owned(),
                data,
            };
            if let Ok(packet) = OutboundPacket::reliable(&art, Priority::Bulk) {
                let _ = self.peers.send(peer, packet);
            }
        }
    }

    /// This device's players changed: tell every connected peer.
    pub fn local_changed(&self) {
        let players = self.host.players();
        let Ok(packet) = Self::players_packet(&players) else {
            return;
        };
        for peer in self.peers.ids() {
            if self.peers.send(peer, packet.clone()).is_ok() {
                self.send_art(peer, &players);
            }
        }
    }

    /// Control a peer's player.
    pub fn command(&self, peer: DeviceId, player: &str, action: MediaAction) -> Result<()> {
        let cmd = MediaCommand {
            player: player.to_owned(),
            action,
        };
        self.peers
            .send(peer, OutboundPacket::reliable(&cmd, Priority::Interactive)?)
    }

    /// The players a peer last reported.
    pub fn peer_players(&self, peer: DeviceId) -> Vec<PlayerState> {
        lock(&self.remote).get(&peer).cloned().unwrap_or_default()
    }
}

#[async_trait]
impl Plugin for MediaPlugin {
    fn id(&self) -> &'static str {
        "media"
    }

    fn incoming(&self) -> &'static [&'static str] {
        &[MediaPlayers::TYPE, MediaArt::TYPE, MediaCommand::TYPE]
    }

    fn outgoing(&self) -> &'static [&'static str] {
        self.incoming()
    }

    async fn on_connected(&self, ctx: &PluginCtx) {
        self.peers.insert(ctx);
        // A new connection may be a restarted peer: it has no artwork yet.
        lock(&self.art_sent).remove(&ctx.peer());
        let players = self.host.players();
        if let Ok(packet) = Self::players_packet(&players) {
            let _ = ctx.send(packet);
            self.send_art(ctx.peer(), &players);
        }
    }

    async fn on_packet(&self, ctx: &PluginCtx, packet: Envelope) {
        let from = ctx.peer_info();
        match packet.ty.as_str() {
            MediaPlayers::TYPE => match packet.body::<MediaPlayers>() {
                Ok(p) => {
                    let players: Vec<PlayerState> = p
                        .players
                        .into_iter()
                        .take(MAX_PLAYERS)
                        .map(PlayerState::clamp)
                        .collect();
                    lock(&self.remote).insert(from.id, players.clone());
                    self.host.peer_players(&from, &players);
                }
                Err(e) => debug!(peer = %from.id, error = %e, "bad media.players"),
            },
            MediaArt::TYPE => match packet.body::<MediaArt>() {
                Ok(a) if a.data.len() <= MAX_ART_BYTES => {
                    self.host.peer_artwork(&from, &a.key, &a.data);
                }
                Ok(_) => {}
                Err(e) => debug!(peer = %from.id, error = %e, "bad media.art"),
            },
            MediaCommand::TYPE => match packet.body::<MediaCommand>() {
                Ok(c) => self.host.command(&from, &c.player, c.action),
                Err(e) => debug!(peer = %from.id, error = %e, "bad media.command"),
            },
            _ => {}
        }
    }

    async fn on_disconnected(&self, peer: DeviceId) {
        self.peers.remove(peer);
        let had = lock(&self.remote).remove(&peer).is_some();
        if had {
            let info = PeerInfo {
                id: peer,
                name: String::new(),
            };
            self.host.peer_players(&info, &[]);
        }
    }
}
