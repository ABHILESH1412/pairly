//! Remote input: the phone as the PC's touchpad and keyboard, and as a presentation remote.
//!
//! Pointer motion is unreliable (a lost delta doesn't matter and must never queue up behind a
//! reconnect); buttons and keys are reliable so a click is never lost or repeated.

use std::sync::Arc;

use async_trait::async_trait;
use pairly_core::{
    DeviceId, Envelope, OutboundPacket, PacketBody, PeerInfo, Plugin, PluginCtx, Priority, Result,
};
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::peers::Peers;

/// Relative pointer movement and scrolling, in (scaled) pixels.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PointerMotion {
    pub dx: f32,
    pub dy: f32,
    pub scroll_x: f32,
    pub scroll_y: f32,
}

impl PacketBody for PointerMotion {
    const TYPE: &'static str = "input.pointer";
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ButtonAction {
    Click,
    Press,
    Release,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PointerButton {
    pub button: MouseButton,
    pub action: ButtonAction,
}

impl PacketBody for PointerButton {
    const TYPE: &'static str = "input.button";
}

/// Keys that aren't text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpecialKey {
    Enter,
    Backspace,
    Delete,
    Tab,
    Escape,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    Space,
    F(u8),
    VolumeUp,
    VolumeDown,
    Mute,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Modifiers {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    #[serde(rename = "super")]
    pub logo: bool,
}

/// Either text to type, or one special key, with modifiers held.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyInput {
    pub text: Option<String>,
    pub key: Option<SpecialKey>,
    pub modifiers: Modifiers,
}

impl PacketBody for KeyInput {
    const TYPE: &'static str = "input.key";
}

pub const MAX_TEXT: usize = 1000;

/// Applies input on this device (the PC).
pub trait InputHost: Send + Sync + 'static {
    fn pointer(&self, from: &PeerInfo, motion: PointerMotion);
    fn button(&self, from: &PeerInfo, button: PointerButton);
    fn key(&self, from: &PeerInfo, key: &KeyInput);
}

pub struct InputPlugin {
    host: Arc<dyn InputHost>,
    peers: Peers,
}

impl InputPlugin {
    pub fn new(host: Arc<dyn InputHost>) -> Arc<Self> {
        Arc::new(Self {
            host,
            peers: Peers::default(),
        })
    }

    pub fn pointer(&self, peer: DeviceId, motion: PointerMotion) -> Result<()> {
        self.peers.send(
            peer,
            OutboundPacket::unreliable(&motion, Priority::Interactive)?,
        )
    }

    pub fn button(&self, peer: DeviceId, button: PointerButton) -> Result<()> {
        self.peers.send(
            peer,
            OutboundPacket::reliable(&button, Priority::Interactive)?,
        )
    }

    pub fn key(&self, peer: DeviceId, key: &KeyInput) -> Result<()> {
        if key.text.as_ref().is_some_and(|t| t.len() > MAX_TEXT) {
            return Err(pairly_core::CoreError::Violation("text too long"));
        }
        self.peers
            .send(peer, OutboundPacket::reliable(key, Priority::Interactive)?)
    }
}

fn finite(v: f32) -> f32 {
    if v.is_finite() {
        v.clamp(-10_000.0, 10_000.0)
    } else {
        0.0
    }
}

#[async_trait]
impl Plugin for InputPlugin {
    fn id(&self) -> &'static str {
        "input"
    }

    fn incoming(&self) -> &'static [&'static str] {
        &[PointerMotion::TYPE, PointerButton::TYPE, KeyInput::TYPE]
    }

    fn outgoing(&self) -> &'static [&'static str] {
        self.incoming()
    }

    async fn on_connected(&self, ctx: &PluginCtx) {
        self.peers.insert(ctx);
    }

    async fn on_packet(&self, ctx: &PluginCtx, packet: Envelope) {
        let from = ctx.peer_info();
        match packet.ty.as_str() {
            PointerMotion::TYPE => match packet.body::<PointerMotion>() {
                Ok(m) => self.host.pointer(
                    &from,
                    PointerMotion {
                        dx: finite(m.dx),
                        dy: finite(m.dy),
                        scroll_x: finite(m.scroll_x),
                        scroll_y: finite(m.scroll_y),
                    },
                ),
                Err(e) => debug!(peer = %from.id, error = %e, "bad input.pointer"),
            },
            PointerButton::TYPE => match packet.body::<PointerButton>() {
                Ok(b) => self.host.button(&from, b),
                Err(e) => debug!(peer = %from.id, error = %e, "bad input.button"),
            },
            KeyInput::TYPE => match packet.body::<KeyInput>() {
                Ok(k) if k.text.as_ref().is_none_or(|t| t.len() <= MAX_TEXT) => {
                    self.host.key(&from, &k)
                }
                Ok(_) => {}
                Err(e) => debug!(peer = %from.id, error = %e, "bad input.key"),
            },
            _ => {}
        }
    }

    async fn on_disconnected(&self, peer: DeviceId) {
        self.peers.remove(peer);
    }
}
