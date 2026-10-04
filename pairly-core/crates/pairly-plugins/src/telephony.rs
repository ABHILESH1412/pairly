//! Calls: the phone reports ringing, answered, missed and ended calls; the PC shows them,
//! pauses its media while a call rings or is active, and can answer, reject, hang up or dial
//! through the phone.

use std::sync::Arc;

use async_trait::async_trait;
use pairly_core::{
    DeviceId, Envelope, OutboundPacket, PacketBody, PeerInfo, Plugin, PluginCtx, Priority, Result,
};
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::peers::Peers;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallState {
    Ringing,
    /// Answered (or an outgoing call).
    Talking,
    /// Rang and stopped without being answered.
    Missed,
    /// Hung up after talking.
    Ended,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallEvent {
    pub state: CallState,
    /// The caller's number, if the phone may read it.
    pub number: Option<String>,
    /// Contact name for the number, if known.
    pub contact: Option<String>,
}

impl PacketBody for CallEvent {
    const TYPE: &'static str = "telephony.event";
}

impl CallEvent {
    /// The best name to show for the caller.
    pub fn caller(&self) -> &str {
        self.contact
            .as_deref()
            .or(self.number.as_deref())
            .unwrap_or("Unknown number")
    }
}

/// PC → phone: act on the current call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallAction {
    Answer,
    /// Answer and switch to the loudspeaker.
    AnswerOnSpeaker,
    Reject,
    HangUp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallControl {
    pub action: CallAction,
}

impl PacketBody for CallControl {
    const TYPE: &'static str = "telephony.control";
}

/// PC → phone: place a call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dial {
    pub number: String,
}

impl PacketBody for Dial {
    const TYPE: &'static str = "telephony.dial";
}

pub trait TelephonyHost: Send + Sync + 'static {
    /// A peer's call changed (the PC side).
    fn call(&self, _from: &PeerInfo, _event: &CallEvent) {}
    /// A peer asks us to act on our current call (the phone side).
    fn control(&self, _from: &PeerInfo, _action: CallAction) {}
    /// A peer asks us to call `number` (the phone side).
    fn dial(&self, _from: &PeerInfo, _number: &str) {}
}

pub struct TelephonyPlugin {
    host: Arc<dyn TelephonyHost>,
    peers: Peers,
}

impl TelephonyPlugin {
    pub fn new(host: Arc<dyn TelephonyHost>) -> Arc<Self> {
        Arc::new(Self {
            host,
            peers: Peers::default(),
        })
    }

    /// Ask a phone to answer, reject or end its current call.
    pub fn control(&self, peer: DeviceId, action: CallAction) -> Result<()> {
        self.peers.send(
            peer,
            OutboundPacket::reliable(&CallControl { action }, Priority::Interactive)?,
        )
    }

    /// Ask a phone to call `number`.
    pub fn dial(&self, peer: DeviceId, number: &str) -> Result<()> {
        let number = number.trim();
        if number.is_empty() || number.len() > 64 {
            return Err(pairly_core::CoreError::Violation("bad phone number"));
        }
        let dial = Dial {
            number: number.to_owned(),
        };
        self.peers.send(
            peer,
            OutboundPacket::reliable(&dial, Priority::Interactive)?,
        )
    }

    /// A call on this device changed: tell every connected peer.
    pub fn report(&self, event: &CallEvent) -> Result<()> {
        let mut event = event.clone();
        for field in [&mut event.number, &mut event.contact] {
            if field.as_ref().is_some_and(|s| s.len() > 128) {
                *field = None;
            }
        }
        self.peers
            .broadcast(&OutboundPacket::reliable(&event, Priority::Interactive)?);
        Ok(())
    }
}

#[async_trait]
impl Plugin for TelephonyPlugin {
    fn id(&self) -> &'static str {
        "telephony"
    }

    fn incoming(&self) -> &'static [&'static str] {
        &[CallEvent::TYPE, CallControl::TYPE, Dial::TYPE]
    }

    fn outgoing(&self) -> &'static [&'static str] {
        self.incoming()
    }

    async fn on_connected(&self, ctx: &PluginCtx) {
        self.peers.insert(ctx);
    }

    async fn on_packet(&self, ctx: &PluginCtx, packet: Envelope) {
        let from = ctx.peer_info();
        let result = match packet.ty.as_str() {
            CallEvent::TYPE => packet
                .body::<CallEvent>()
                .map(|e| self.host.call(&from, &e)),
            CallControl::TYPE => packet
                .body::<CallControl>()
                .map(|c| self.host.control(&from, c.action)),
            Dial::TYPE => packet.body::<Dial>().map(|d| {
                if !d.number.is_empty() && d.number.len() <= 64 {
                    self.host.dial(&from, &d.number);
                }
            }),
            _ => Ok(()),
        };
        if let Err(e) = result {
            debug!(peer = %from.id, error = %e, "bad telephony packet");
        }
    }

    async fn on_disconnected(&self, peer: DeviceId) {
        self.peers.remove(peer);
    }
}
