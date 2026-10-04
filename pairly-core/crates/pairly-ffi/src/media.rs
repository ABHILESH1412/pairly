//! Media control for Kotlin: the phone's MediaSessions go to peers, and a peer's players come
//! back to be shown as a media notification on the phone.

use std::sync::Arc;

use pairly_core::PeerInfo;
use pairly_plugins::media::{MediaAction, MediaHost, PlayerState};

#[derive(Debug, Clone, uniffi::Record)]
pub struct PlayerData {
    pub id: String,
    pub name: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    /// Artwork key: `https://…` to fetch, or an opaque key whose bytes arrive separately.
    pub art: Option<String>,
    pub length_ms: Option<u64>,
    pub position_ms: Option<u64>,
    pub playing: bool,
    pub can_play: bool,
    pub can_pause: bool,
    pub can_next: bool,
    pub can_previous: bool,
    pub can_seek: bool,
    /// 0–100.
    pub volume: Option<u8>,
}

impl From<PlayerData> for PlayerState {
    fn from(p: PlayerData) -> Self {
        Self {
            id: p.id,
            name: p.name,
            title: p.title,
            artist: p.artist,
            album: p.album,
            art: p.art,
            length_ms: p.length_ms,
            position_ms: p.position_ms,
            playing: p.playing,
            can_play: p.can_play,
            can_pause: p.can_pause,
            can_next: p.can_next,
            can_previous: p.can_previous,
            can_seek: p.can_seek,
            volume: p.volume,
        }
    }
}

impl From<&PlayerState> for PlayerData {
    fn from(p: &PlayerState) -> Self {
        Self {
            id: p.id.clone(),
            name: p.name.clone(),
            title: p.title.clone(),
            artist: p.artist.clone(),
            album: p.album.clone(),
            art: p.art.clone(),
            length_ms: p.length_ms,
            position_ms: p.position_ms,
            playing: p.playing,
            can_play: p.can_play,
            can_pause: p.can_pause,
            can_next: p.can_next,
            can_previous: p.can_previous,
            can_seek: p.can_seek,
            volume: p.volume,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum MediaActionData {
    Play,
    Pause,
    PlayPause,
    Stop,
    Next,
    Previous,
    Seek { offset_ms: i64 },
    SetPosition { position_ms: u64 },
    SetVolume { percent: u8 },
}

impl From<MediaActionData> for MediaAction {
    fn from(a: MediaActionData) -> Self {
        match a {
            MediaActionData::Play => Self::Play,
            MediaActionData::Pause => Self::Pause,
            MediaActionData::PlayPause => Self::PlayPause,
            MediaActionData::Stop => Self::Stop,
            MediaActionData::Next => Self::Next,
            MediaActionData::Previous => Self::Previous,
            MediaActionData::Seek { offset_ms } => Self::Seek(offset_ms),
            MediaActionData::SetPosition { position_ms } => Self::SetPosition(position_ms),
            MediaActionData::SetVolume { percent } => Self::SetVolume(percent),
        }
    }
}

impl From<MediaAction> for MediaActionData {
    fn from(a: MediaAction) -> Self {
        match a {
            MediaAction::Play => Self::Play,
            MediaAction::Pause => Self::Pause,
            MediaAction::PlayPause => Self::PlayPause,
            MediaAction::Stop => Self::Stop,
            MediaAction::Next => Self::Next,
            MediaAction::Previous => Self::Previous,
            MediaAction::Seek(offset_ms) => Self::Seek { offset_ms },
            MediaAction::SetPosition(position_ms) => Self::SetPosition { position_ms },
            MediaAction::SetVolume(percent) => Self::SetVolume { percent },
        }
    }
}

/// Implemented in Kotlin. Called on Rust threads: hand off quickly.
#[uniffi::export(with_foreign)]
pub trait MediaHandler: Send + Sync {
    /// The phone's players right now (needs notification access).
    fn players(&self) -> Vec<PlayerData>;
    /// Artwork bytes for a key from `players()`.
    fn artwork(&self, key: String) -> Option<Vec<u8>>;
    /// A paired device's players (empty: nothing to show).
    fn peer_players(&self, from_id: String, from_name: String, players: Vec<PlayerData>);
    fn peer_artwork(&self, from_id: String, key: String, data: Vec<u8>);
    /// A paired device asks to control one of the phone's players.
    fn command(&self, player: String, action: MediaActionData);
}

pub(crate) struct ForeignMediaHost(pub Arc<dyn MediaHandler>);

impl MediaHost for ForeignMediaHost {
    fn players(&self) -> Vec<PlayerState> {
        self.0.players().into_iter().map(Into::into).collect()
    }
    fn artwork(&self, key: &str) -> Option<Vec<u8>> {
        self.0.artwork(key.to_owned())
    }
    fn peer_players(&self, from: &PeerInfo, players: &[PlayerState]) {
        self.0.peer_players(
            from.id.to_string(),
            from.name.clone(),
            players.iter().map(Into::into).collect(),
        );
    }
    fn peer_artwork(&self, from: &PeerInfo, key: &str, data: &[u8]) {
        self.0
            .peer_artwork(from.id.to_string(), key.to_owned(), data.to_vec());
    }
    fn command(&self, _from: &PeerInfo, player: &str, action: MediaAction) {
        self.0.command(player.to_owned(), action.into());
    }
}
