//! Screen mirroring: one device shows its screen to another as an H.264 stream, and the viewer
//! controls it: taps, swipes, keys and text for a phone's screen; the pointer, mouse buttons,
//! wheel and key combinations for a PC's (a PC watching another PC).
//!
//! Over the local network or the internet relay, not Bluetooth (far too slow for video). Over
//! the relay the sharer sends a lighter stream (it asks [`ScreenPlugin::over_internet`]) and the
//! sound goes mono at half the rate. A stream keeps going when its session moves between the
//! LAN and the relay, and stops if it moves to Bluetooth.
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

use crate::input::{ButtonAction, Modifiers, MouseButton, SpecialKey};
use crate::peers::Peers;

/// The biggest single frame (a key frame of a detailed screen is a few hundred KB).
pub const MAX_FRAME: usize = 900 * 1024;
/// Points in one swipe.
pub const MAX_SWIPE_POINTS: usize = 256;
pub const MAX_TEXT: usize = 1000;
/// Frames waiting to go out before the sharer skips ahead (about 200 ms of video): beyond
/// this the link is slower than the stream, and waiting would only make the picture lag.
pub const MAX_VIDEO_BACKLOG: usize = 6;
/// Sound packets (20 ms each) allowed to wait before newer ones are dropped.
pub const MAX_AUDIO_BACKLOG: usize = 10;

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

/// Sharer → viewer: a moment of the shared device's sound, as 16-bit signed little-endian
/// samples, channels interleaved. Uncompressed: on the local network its 1.5 Mbit/s (48 kHz
/// stereo) is less than the video's; over the relay it's sent mono at half the rate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenAudio {
    pub rate: u32,
    pub channels: u8,
    #[serde(with = "serde_bytes")]
    pub data: Vec<u8>,
}

impl PacketBody for ScreenAudio {
    const TYPE: &'static str = "screen.audio";
}

/// The most audio in one packet (about a third of a second at 48 kHz stereo).
pub const MAX_AUDIO: usize = 64 * 1024;

impl ScreenAudio {
    fn usable(&self) -> bool {
        (8_000..=96_000).contains(&self.rate)
            && (1..=2).contains(&self.channels)
            && !self.data.is_empty()
            && self.data.len() <= MAX_AUDIO
            && self
                .data
                .len()
                .is_multiple_of(2 * usize::from(self.channels))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScreenKey {
    Back,
    Home,
    Recents,
    Enter,
    Backspace,
    /// Removes the character after the cursor.
    Delete,
    /// The arrows move the cursor in the focused text field.
    Left,
    Right,
    Up,
    Down,
}

/// Which way a touchpad swipe's fingers moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwipeDirection {
    Left,
    Right,
    Up,
    Down,
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
    /// A PC's screen: the pointer moved here (sent unreliably: the next move overtakes a lost
    /// one).
    Pointer {
        x: f32,
        y: f32,
    },
    /// A PC's screen: a mouse button pressed, released or clicked here.
    Button {
        x: f32,
        y: f32,
        button: MouseButton,
        action: ButtonAction,
    },
    /// A PC's screen: mouse wheel steps (positive: down, right).
    Wheel {
        dx: f32,
        dy: f32,
    },
    /// A PC's screen: a key or text with modifiers held (shortcuts, function keys). With
    /// neither, the modifiers alone are tapped (the Windows key opens the Start menu).
    Combo {
        key: Option<SpecialKey>,
        text: Option<String>,
        #[serde(default)]
        modifiers: Modifiers,
    },
    /// A PC's screen: a touchpad swipe with several fingers (in Control mode, where the
    /// viewer's own desktop leaves its gestures alone). The sharer does what its desktop does
    /// for that swipe: switch workspace or virtual desktop, show them all…
    Gesture {
        fingers: u8,
        direction: SwipeDirection,
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
            Self::Pointer { x, y } => Self::Pointer {
                x: unit(x)?,
                y: unit(y)?,
            },
            Self::Button {
                x,
                y,
                button,
                action,
            } => Self::Button {
                x: unit(x)?,
                y: unit(y)?,
                button,
                action,
            },
            Self::Wheel { dx, dy } => {
                let steps = |v: f32| v.is_finite().then(|| v.clamp(-20.0, 20.0));
                Self::Wheel {
                    dx: steps(dx)?,
                    dy: steps(dy)?,
                }
            }
            Self::Gesture { fingers, direction } if (3..=5).contains(&fingers) => {
                Self::Gesture { fingers, direction }
            }
            Self::Gesture { .. } => return None,
            Self::Combo { text: Some(t), .. } if t.len() > MAX_TEXT => return None,
            Self::Combo {
                key,
                text,
                modifiers,
            } => Self::Combo {
                key: key.filter(|k| !matches!(k, SpecialKey::F(n) if !(1..=24).contains(n))),
                text: text.filter(|t| !t.is_empty()),
                modifiers,
            },
        })
    }

    /// Only worth sending while fresh (pointer moves).
    fn is_motion(&self) -> bool {
        matches!(self, Self::Pointer { .. })
    }
}

/// Each side implements its half: the sharer the first three, the viewer the rest.
pub trait ScreenHost: Send + Sync + 'static {
    /// A viewer asks to see this screen: ask the user, then call [`ScreenPlugin::started`].
    fn start_sharing(&self, _from: &PeerInfo) -> std::result::Result<(), String> {
        Err("this device can't share its screen".into())
    }
    fn stop_sharing(&self, _from: &PeerInfo) {}
    fn input(&self, _from: &PeerInfo, _input: ScreenInput) {}

    fn started(&self, _from: &PeerInfo, _width: u32, _height: u32) {}
    fn frame(&self, _from: &PeerInfo, _frame: ScreenFrame) {}
    /// Sound to play along with the picture.
    fn audio(&self, _from: &PeerInfo, _audio: ScreenAudio) {}
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

/// Links fast enough for video: the local network (or the in-process test network) and the
/// internet relay. Not Bluetooth.
fn can_stream(kind: Option<TransportKind>) -> bool {
    matches!(
        kind,
        Some(TransportKind::Lan | TransportKind::Memory | TransportKind::Relay)
    )
}

const NOT_ON_BLUETOOTH: &str = "screen sharing doesn't work over Bluetooth (too slow): use the \
     same Wi-Fi or the internet relay";

/// The sound for a slower link: mono, and half the rate when it's 32 kHz or more (each output
/// sample averages the frames it replaces). A quarter of the data for 48 kHz stereo.
fn lighter(audio: &ScreenAudio) -> ScreenAudio {
    let channels = usize::from(audio.channels.max(1));
    let step = if audio.rate >= 32_000 { 2 } else { 1 };
    let samples: Vec<i32> = audio
        .data
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| i32::from(i16::from_le_bytes(*b)))
        .collect();
    let mut data = Vec::with_capacity(samples.len() / (channels * step) * 2);
    for group in samples.chunks_exact(channels * step) {
        let mean = group.iter().sum::<i32>() / group.len() as i32;
        data.extend_from_slice(&(mean as i16).to_le_bytes());
    }
    ScreenAudio {
        rate: audio.rate / step as u32,
        channels: 1,
        data,
    }
}

impl ScreenPlugin {
    pub fn new(host: Arc<dyn ScreenHost>) -> Arc<Self> {
        Arc::new(Self {
            host,
            peers: Peers::default(),
            behind: Mutex::default(),
        })
    }

    fn require_link(&self, peer: DeviceId) -> Result<()> {
        if can_stream(self.peers.transport(peer)) {
            Ok(())
        } else {
            Err(CoreError::Transport(NOT_ON_BLUETOOTH.into()))
        }
    }

    /// Whether `peer` is reached through the internet relay: the sharer should send a lighter
    /// stream (smaller and fewer bits than on the local network).
    pub fn over_internet(&self, peer: DeviceId) -> bool {
        self.peers.transport(peer) == Some(TransportKind::Relay)
    }

    /// Viewer: ask `peer` to share its screen (or stop).
    pub fn request(&self, peer: DeviceId, action: ScreenAction) -> Result<()> {
        if action == ScreenAction::Start {
            self.require_link(peer)?;
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
        let packet = if input.is_motion() {
            OutboundPacket::unreliable(&input, Priority::Interactive)?
        } else {
            OutboundPacket::reliable(&input, Priority::Interactive)?
        };
        self.peers.send(peer, packet)
    }

    /// Sharer: the stream starts.
    pub fn started(&self, peer: DeviceId, width: u32, height: u32) -> Result<()> {
        self.require_link(peer)?;
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
    /// Sharer: a moment of sound for `peer`. Sound late by more than a moment is dropped
    /// rather than queued, so it never lags behind the picture.
    pub fn audio(&self, peer: DeviceId, audio: &ScreenAudio) -> Result<()> {
        if !audio.usable() {
            return Err(CoreError::Violation("bad screen audio"));
        }
        self.require_link(peer)?;
        if self.peers.queued(peer, Priority::Interactive).unwrap_or(0) > MAX_AUDIO_BACKLOG {
            return Ok(());
        }
        let packet = if self.over_internet(peer) {
            OutboundPacket::unreliable(&lighter(audio), Priority::Interactive)?
        } else {
            OutboundPacket::unreliable(audio, Priority::Interactive)?
        };
        self.peers.send(peer, packet)
    }

    pub fn frame(&self, peer: DeviceId, frame: &ScreenFrame) -> Result<bool> {
        if frame.data.len() > MAX_FRAME {
            return Err(CoreError::Violation("screen frame too large"));
        }
        self.require_link(peer)?;
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
            ScreenAudio::TYPE,
        ]
    }

    fn outgoing(&self) -> &'static [&'static str] {
        self.incoming()
    }

    async fn on_connected(&self, ctx: &PluginCtx) {
        // A session that moved to Bluetooth mid-stream ends it on both sides. Between the
        // LAN and the relay it carries on: falling behind skips to the next key frame.
        if !can_stream(ctx.transport()) {
            let from = ctx.peer_info();
            self.host.stop_sharing(&from);
            self.host.stopped(&from, NOT_ON_BLUETOOTH);
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
                    let refused = if can_stream(ctx.transport()) {
                        self.host.start_sharing(&from).err()
                    } else {
                        Some(NOT_ON_BLUETOOTH.to_owned())
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
                Ok(f) if f.data.len() <= MAX_FRAME && can_stream(ctx.transport()) => {
                    self.host.frame(&from, f)
                }
                Ok(_) => {}
                Err(e) => debug!(peer = %from.id, error = %e, "bad screen.frame"),
            },
            ScreenAudio::TYPE => match packet.body::<ScreenAudio>() {
                Ok(a) if a.usable() && can_stream(ctx.transport()) => self.host.audio(&from, a),
                Ok(_) => {}
                Err(e) => debug!(peer = %from.id, error = %e, "bad screen.audio"),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn internet_sound_is_mono_at_half_the_rate() {
        // 48 kHz stereo, two frames: (100, 300) and (500, 700).
        let data = [100_i16, 300, 500, 700]
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        let light = lighter(&ScreenAudio {
            rate: 48_000,
            channels: 2,
            data,
        });
        assert_eq!((light.rate, light.channels), (24_000, 1));
        assert_eq!(light.data, 400_i16.to_le_bytes());
        assert!(light.usable());
        // Already mono at a low rate: unchanged.
        let speech = lighter(&ScreenAudio {
            rate: 16_000,
            channels: 1,
            data: vec![1, 0, 2, 0],
        });
        assert_eq!((speech.rate, speech.data), (16_000, vec![1, 0, 2, 0]));
    }
}
