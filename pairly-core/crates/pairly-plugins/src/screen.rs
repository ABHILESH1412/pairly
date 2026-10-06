//! Screen mirroring: one device shows its screen to another as an H.264 stream, and the viewer
//! controls it (taps, swipes, keys, text).
//!
//! Only over the local network: video needs its bandwidth, and Bluetooth or a relay would
//! make it unusable (or expensive). Both sides check the link, and a stream stops if its
//! session moves off the LAN.
//!
//! Flow: the viewer sends `screen.request {Start}`; the sharing device asks its user, then sends
//! `screen.started`, a stream of `screen.frame`s (unreliable: a late frame is useless), and
//! `screen.stopped` at the end. The viewer sends `screen.input` while watching and
//! `screen.request {Stop}` to end it.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use pairly_core::{
    CoreError, DeviceId, Envelope, OutboundPacket, PacketBody, PeerInfo, Plugin, PluginCtx,
    Priority, Result, TransportKind,
};
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::peers::Peers;

/// The biggest single frame (a key frame of a detailed screen is a few hundred KB).
pub const MAX_FRAME: usize = 900 * 1024;
/// Points in one swipe.
pub const MAX_SWIPE_POINTS: usize = 256;
pub const MAX_TEXT: usize = 1000;
/// Frames waiting to go out before the sharer skips ahead (about 200 ms of video): beyond
/// this the link is slower than the stream, and waiting would only make the picture lag.
pub const MAX_VIDEO_BACKLOG: usize = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScreenAction {
    Start,
    Stop,
}

/// Viewer → sharer: start or stop sharing your screen with me.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenRequest {
    pub action: ScreenAction,
}

impl PacketBody for ScreenRequest {
    const TYPE: &'static str = "screen.request";
}

/// Sharer → viewer: the stream begins (video size in pixels).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenStarted {
    pub width: u32,
    pub height: u32,
}

impl PacketBody for ScreenStarted {
    const TYPE: &'static str = "screen.started";
}

/// Sharer → viewer: the stream ended (or never started), and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenStopped {
    pub reason: String,
}

impl PacketBody for ScreenStopped {
    const TYPE: &'static str = "screen.stopped";
}

/// Sharer → viewer: one H.264 access unit (Annex B byte stream).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenFrame {
    #[serde(with = "serde_bytes")]
    pub data: Vec<u8>,
    /// Decoding can start here.
    pub key: bool,
    /// Codec setup (SPS/PPS) rather than a picture.
    #[serde(default)]
    pub config: bool,
}

impl PacketBody for ScreenFrame {
    const TYPE: &'static str = "screen.frame";
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScreenKey {
    Back,
    Home,
    Recents,
    Enter,
    Backspace,
}

/// Viewer → sharer: control the shared screen. Positions are fractions (0–1) of the video.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ScreenInput {
    Tap {
        x: f32,
        y: f32,
    },
    LongPress {
        x: f32,
        y: f32,
    },
    /// A drag through `points`, taking `duration_ms`.
    Swipe {
        points: Vec<(f32, f32)>,
        duration_ms: u32,
    },
    Key {
        key: ScreenKey,
    },
    Text {
        text: String,
    },
    /// Scroll by a fraction of the screen (two fingers on a touch screen).
    Scroll {
        dx: f32,
        dy: f32,
    },
}

impl PacketBody for ScreenInput {
    const TYPE: &'static str = "screen.input";
}

impl ScreenInput {
    /// Positions inside the screen, a sane swipe, bounded text; `None` if it can't be fixed.
    fn sanitized(self) -> Option<Self> {
        let unit = |v: f32| v.is_finite().then(|| v.clamp(0.0, 1.0));
        Some(match self {
            Self::Tap { x, y } => Self::Tap {
                x: unit(x)?,
                y: unit(y)?,
            },
            Self::LongPress { x, y } => Self::LongPress {
                x: unit(x)?,
                y: unit(y)?,
            },
            Self::Swipe {
                points,
                duration_ms,
            } => {
                let points: Vec<(f32, f32)> = points
                    .into_iter()
                    .take(MAX_SWIPE_POINTS)
                    .map(|(x, y)| Some((unit(x)?, unit(y)?)))
                    .collect::<Option<_>>()?;
                if points.len() < 2 {
                    return None;
                }
                Self::Swipe {
                    points,
                    duration_ms: duration_ms.clamp(10, 10_000),
                }
            }
            Self::Key { key } => Self::Key { key },
            Self::Scroll { dx, dy } => {
                let step = |v: f32| v.is_finite().then(|| v.clamp(-2.0, 2.0));
                Self::Scroll {
                    dx: step(dx)?,
                    dy: step(dy)?,
                }
            }
            Self::Text { text } if text.len() <= MAX_TEXT => Self::Text { text },
            Self::Text { .. } => return None,
        })
    }
}

/// Each side implements its half: the sharer (phone) the first three, the viewer (PC) the rest.
pub trait ScreenHost: Send + Sync + 'static {
    /// A viewer asks to see this screen: ask the user, then call [`ScreenPlugin::started`].
    fn start_sharing(&self, _from: &PeerInfo) -> std::result::Result<(), String> {
        Err("this device can't share its screen".into())
    }
    fn stop_sharing(&self, _from: &PeerInfo) {}
    fn input(&self, _from: &PeerInfo, _input: ScreenInput) {}

    fn started(&self, _from: &PeerInfo, _width: u32, _height: u32) {}
    fn frame(&self, _from: &PeerInfo, _frame: ScreenFrame) {}
    fn stopped(&self, _from: &PeerInfo, _reason: &str) {}
    /// The device went away: end any stream with it (either side).
    fn disconnected(&self, _peer: DeviceId) {}
}

pub struct ScreenPlugin {
    host: Arc<dyn ScreenHost>,
    peers: Peers,
    /// Viewers the stream fell behind for: frames to them are skipped until a key frame.
    behind: Mutex<HashSet<DeviceId>>,
}

/// The local network (or the in-process test network).
fn is_local(kind: Option<TransportKind>) -> bool {
    matches!(kind, Some(TransportKind::Lan | TransportKind::Memory))
}

fn lan_only(ctx: &PluginCtx) -> bool {
    is_local(ctx.transport())
}

impl ScreenPlugin {
    pub fn new(host: Arc<dyn ScreenHost>) -> Arc<Self> {
        Arc::new(Self {
            host,
            peers: Peers::default(),
            behind: Mutex::default(),
        })
    }

    fn require_lan(&self, peer: DeviceId) -> Result<()> {
        if is_local(self.peers.transport(peer)) {
            Ok(())
        } else {
            Err(CoreError::Transport(
                "screen sharing works only when both devices are on the same Wi-Fi".into(),
            ))
        }
    }

    /// Viewer: ask `peer` to share its screen (or stop).
    pub fn request(&self, peer: DeviceId, action: ScreenAction) -> Result<()> {
        if action == ScreenAction::Start {
            self.require_lan(peer)?;
        }
        self.peers.send(
            peer,
            OutboundPacket::reliable(&ScreenRequest { action }, Priority::Control)?,
        )
    }

    /// Viewer: control the shared screen.
    pub fn input(&self, peer: DeviceId, input: ScreenInput) -> Result<()> {
        let input = input
            .sanitized()
            .ok_or(CoreError::Violation("bad screen input"))?;
        self.peers.send(
            peer,
            OutboundPacket::reliable(&input, Priority::Interactive)?,
        )
    }

    /// Sharer: the stream starts.
    pub fn started(&self, peer: DeviceId, width: u32, height: u32) -> Result<()> {
        self.require_lan(peer)?;
        self.peers.send(
            peer,
            OutboundPacket::reliable(&ScreenStarted { width, height }, Priority::Control)?,
        )
    }

    /// Sharer: one encoded frame. Returns `true` when the encoder should make a key frame
    /// now: the link fell behind, so frames are skipped until the next key frame (a picture
    /// can't be decoded without the ones before it).
    ///
    /// A key frame first drops the frames still waiting, so the viewer jumps to the present
    /// instead of playing out a backlog.
    pub fn frame(&self, peer: DeviceId, frame: &ScreenFrame) -> Result<bool> {
        if frame.data.len() > MAX_FRAME {
            return Err(CoreError::Violation("screen frame too large"));
        }
        self.require_lan(peer)?;
        if frame.config {
            // Codec setup must arrive.
            self.peers
                .send(peer, OutboundPacket::reliable(frame, Priority::Video)?)?;
            return Ok(false);
        }
        let backlog = self.peers.queued(peer, Priority::Video).unwrap_or(0);
        let mut behind = self.behind.lock().unwrap_or_else(PoisonError::into_inner);
        if frame.key {
            if backlog > 0 {
                self.peers.drop_unreliable(peer, Priority::Video);
            }
            behind.remove(&peer);
        } else if behind.contains(&peer) {
            return Ok(false);
        } else if backlog >= MAX_VIDEO_BACKLOG {
            debug!(%peer, backlog, "screen stream behind: skipping to the next key frame");
            behind.insert(peer);
            return Ok(true);
        }
        drop(behind);
        // Pictures are worthless late: never resent.
        self.peers
            .send(peer, OutboundPacket::unreliable(frame, Priority::Video)?)?;
        Ok(false)
    }

    /// Sharer: the stream ended.
    pub fn stopped(&self, peer: DeviceId, reason: &str) -> Result<()> {
        let body = ScreenStopped {
            reason: reason.chars().take(200).collect(),
        };
        self.peers
            .send(peer, OutboundPacket::reliable(&body, Priority::Control)?)
    }
}

#[async_trait]
impl Plugin for ScreenPlugin {
    fn id(&self) -> &'static str {
        "screen"
    }

    fn incoming(&self) -> &'static [&'static str] {
        &[
            ScreenRequest::TYPE,
            ScreenStarted::TYPE,
            ScreenStopped::TYPE,
            ScreenFrame::TYPE,
            ScreenInput::TYPE,
        ]
    }

    fn outgoing(&self) -> &'static [&'static str] {
        self.incoming()
    }

    async fn on_connected(&self, ctx: &PluginCtx) {
        // A session that moved off the LAN mid-stream ends it on both sides.
        if !lan_only(ctx) {
            let from = ctx.peer_info();
            self.host.stop_sharing(&from);
            self.host
                .stopped(&from, "the devices left the shared Wi-Fi");
        }
        self.peers.insert(ctx);
    }

    async fn on_packet(&self, ctx: &PluginCtx, packet: Envelope) {
        let from = ctx.peer_info();
        match packet.ty.as_str() {
            ScreenRequest::TYPE => match packet.body::<ScreenRequest>() {
                Ok(ScreenRequest {
                    action: ScreenAction::Start,
                }) => {
                    let refused = if lan_only(ctx) {
                        self.host.start_sharing(&from).err()
                    } else {
                        Some("screen sharing works only on the same Wi-Fi".to_owned())
                    };
                    if let Some(reason) = refused
                        && let Ok(p) =
                            OutboundPacket::reliable(&ScreenStopped { reason }, Priority::Control)
                    {
                        let _ = ctx.send(p);
                    }
                }
                Ok(ScreenRequest {
                    action: ScreenAction::Stop,
                }) => self.host.stop_sharing(&from),
                Err(e) => debug!(peer = %from.id, error = %e, "bad screen.request"),
            },
            ScreenInput::TYPE => match packet.body::<ScreenInput>() {
                Ok(input) => {
                    if let Some(input) = input.sanitized() {
                        self.host.input(&from, input);
                    }
                }
                Err(e) => debug!(peer = %from.id, error = %e, "bad screen.input"),
            },
            ScreenStarted::TYPE => match packet.body::<ScreenStarted>() {
                Ok(s) => self
                    .host
                    .started(&from, s.width.min(8192), s.height.min(8192)),
                Err(e) => debug!(peer = %from.id, error = %e, "bad screen.started"),
            },
            ScreenFrame::TYPE => match packet.body::<ScreenFrame>() {
                Ok(f) if f.data.len() <= MAX_FRAME && lan_only(ctx) => self.host.frame(&from, f),
                Ok(_) => {}
                Err(e) => debug!(peer = %from.id, error = %e, "bad screen.frame"),
            },
            ScreenStopped::TYPE => match packet.body::<ScreenStopped>() {
                Ok(s) => self.host.stopped(&from, &s.reason),
                Err(e) => debug!(peer = %from.id, error = %e, "bad screen.stopped"),
            },
            _ => {}
        }
    }

    async fn on_disconnected(&self, peer: DeviceId) {
        self.peers.remove(peer);
        self.behind
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&peer);
        self.host.disconnected(peer);
    }
}
