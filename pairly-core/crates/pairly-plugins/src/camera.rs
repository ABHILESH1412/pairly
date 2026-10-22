//! The phone's camera as the PC's webcam: the PC asks, the phone films with its front or back
//! camera and streams H.264, and the PC offers the picture to its apps as a camera.
//!
//! Like screen sharing it only runs on the local network (video needs the bandwidth), with
//! late frames dropped rather than queued.
//!
//! Flow: the PC sends `camera.request {Start}`; the phone asks its user, then sends
//! `camera.started`, a stream of `camera.frame`s and `camera.stopped` at the end. The PC sends
//! `camera.request {Stop}` to end it.

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
use crate::screen::{MAX_FRAME, MAX_VIDEO_BACKLOG, ScreenFrame};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Facing {
    Front,
    Back,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "action")]
pub enum CameraRequest {
    Start { facing: Facing },
    Stop,
}

impl PacketBody for CameraRequest {
    const TYPE: &'static str = "camera.request";
}

/// Phone → PC: the picture begins (its size in pixels).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CameraStarted {
    pub width: u32,
    pub height: u32,
}

impl PacketBody for CameraStarted {
    const TYPE: &'static str = "camera.started";
}

/// Phone → PC: one H.264 access unit (the same as a screen frame).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CameraFrame(pub ScreenFrame);

impl PacketBody for CameraFrame {
    const TYPE: &'static str = "camera.frame";
}

/// Phone → PC: the camera stopped (or never started), and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CameraStopped {
    pub reason: String,
}

impl PacketBody for CameraStopped {
    const TYPE: &'static str = "camera.stopped";
}

/// The phone implements the first two, the PC the rest.
pub trait CameraHost: Send + Sync + 'static {
    /// The PC asks for the camera: ask the user, then call [`CameraPlugin::started`].
    fn start_camera(&self, _from: &PeerInfo, _facing: Facing) -> std::result::Result<(), String> {
        Err("this device has no camera to share".into())
    }
    fn stop_camera(&self, _from: &PeerInfo) {}

    fn camera_started(&self, _from: &PeerInfo, _width: u32, _height: u32) {}
    fn camera_frame(&self, _from: &PeerInfo, _frame: ScreenFrame) {}
    fn camera_stopped(&self, _from: &PeerInfo, _reason: &str) {}
    /// The device went away: end any stream with it.
    fn disconnected(&self, _peer: DeviceId) {}
}

pub struct CameraPlugin {
    host: Arc<dyn CameraHost>,
    peers: Peers,
    /// PCs the stream fell behind for: frames to them are skipped until a key frame.
    behind: Mutex<HashSet<DeviceId>>,
}

fn is_local(kind: Option<TransportKind>) -> bool {
    matches!(kind, Some(TransportKind::Lan | TransportKind::Memory))
}

impl CameraPlugin {
    pub fn new(host: Arc<dyn CameraHost>) -> Arc<Self> {
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
                "the phone's camera works only when both devices are on the same Wi-Fi".into(),
            ))
        }
    }

    /// PC: ask the phone to start (or stop) its camera.
    pub fn request(&self, peer: DeviceId, request: CameraRequest) -> Result<()> {
        if matches!(request, CameraRequest::Start { .. }) {
            self.require_lan(peer)?;
        }
        self.peers.send(
            peer,
            OutboundPacket::reliable(&request, Priority::Interactive)?,
        )
    }

    /// Phone: the camera is filming.
    pub fn started(&self, peer: DeviceId, width: u32, height: u32) -> Result<()> {
        self.require_lan(peer)?;
        self.peers.send(
            peer,
            OutboundPacket::reliable(&CameraStarted { width, height }, Priority::Interactive)?,
        )
    }

    /// Phone: one frame. Returns true when the PC fell behind and needs a key frame next.
    pub fn frame(&self, peer: DeviceId, frame: &ScreenFrame) -> Result<bool> {
        if frame.data.len() > MAX_FRAME {
            return Err(CoreError::Violation("camera frame too large"));
        }
        self.require_lan(peer)?;
        let packet = CameraFrame(frame.clone());
        if frame.config {
            self.peers
                .send(peer, OutboundPacket::reliable(&packet, Priority::Video)?)?;
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
            debug!(%peer, backlog, "camera stream behind: skipping to the next key frame");
            behind.insert(peer);
            self.peers.drop_unreliable(peer, Priority::Video);
            return Ok(true);
        }
        drop(behind);
        self.peers
            .send(peer, OutboundPacket::unreliable(&packet, Priority::Video)?)?;
        Ok(false)
    }

    /// Phone: the camera stopped (tell the PC why).
    pub fn stopped(&self, peer: DeviceId, reason: &str) -> Result<()> {
        self.behind
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&peer);
        let stopped = CameraStopped {
            reason: reason.to_owned(),
        };
        self.peers.send(
            peer,
            OutboundPacket::reliable(&stopped, Priority::Interactive)?,
        )
    }
}

#[async_trait]
impl Plugin for CameraPlugin {
    fn id(&self) -> &'static str {
        "camera"
    }

    fn incoming(&self) -> &'static [&'static str] {
        &[
            CameraRequest::TYPE,
            CameraStarted::TYPE,
            CameraFrame::TYPE,
            CameraStopped::TYPE,
        ]
    }

    fn outgoing(&self) -> &'static [&'static str] {
        self.incoming()
    }

    async fn on_connected(&self, ctx: &PluginCtx) {
        // A session that moved off the LAN mid-stream ends it on both sides.
        if !is_local(ctx.transport()) {
            let from = ctx.peer_info();
            self.host.stop_camera(&from);
            self.host
                .camera_stopped(&from, "the devices left the shared Wi-Fi");
        }
        self.peers.insert(ctx);
    }

    async fn on_packet(&self, ctx: &PluginCtx, packet: Envelope) {
        let from = ctx.peer_info();
        let local = is_local(ctx.transport());
        match packet.ty.as_str() {
            CameraRequest::TYPE => match packet.body::<CameraRequest>() {
                Ok(CameraRequest::Start { facing }) if local => {
                    if let Err(reason) = self.host.start_camera(&from, facing) {
                        let _ = self.stopped(from.id, &reason);
                    }
                }
                Ok(CameraRequest::Start { .. }) => {
                    let _ = self.stopped(from.id, "the devices aren't on the same Wi-Fi");
                }
                Ok(CameraRequest::Stop) => self.host.stop_camera(&from),
                Err(e) => debug!(peer = %from.id, error = %e, "bad camera.request"),
            },
            CameraStarted::TYPE => match packet.body::<CameraStarted>() {
                Ok(s) if local => self.host.camera_started(&from, s.width, s.height),
                Ok(_) => {}
                Err(e) => debug!(peer = %from.id, error = %e, "bad camera.started"),
            },
            CameraFrame::TYPE => match packet.body::<CameraFrame>() {
                Ok(CameraFrame(f)) if f.data.len() <= MAX_FRAME && local => {
                    self.host.camera_frame(&from, f);
                }
                Ok(_) => {}
                Err(e) => debug!(peer = %from.id, error = %e, "bad camera.frame"),
            },
            CameraStopped::TYPE => match packet.body::<CameraStopped>() {
                Ok(s) => self.host.camera_stopped(&from, &s.reason),
                Err(e) => debug!(peer = %from.id, error = %e, "bad camera.stopped"),
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
