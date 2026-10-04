//! Find my device: make a paired device ring loudly until stopped.

use std::sync::Arc;

use async_trait::async_trait;
use pairly_core::{
    DeviceId, Envelope, OutboundPacket, PacketBody, PeerInfo, Plugin, PluginCtx, Priority, Result,
};
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::peers::Peers;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ring {
    /// Start (true) or stop (false) ringing.
    pub on: bool,
}

impl PacketBody for Ring {
    const TYPE: &'static str = "findmy.ring";
}

pub trait FindMyHost: Send + Sync + 'static {
    /// Start or stop ringing because `from` asked.
    fn ring(&self, from: &PeerInfo, on: bool);
}

pub struct FindMyPlugin {
    host: Arc<dyn FindMyHost>,
    peers: Peers,
}

impl FindMyPlugin {
    pub fn new(host: Arc<dyn FindMyHost>) -> Arc<Self> {
        Arc::new(Self {
            host,
            peers: Peers::default(),
        })
    }

    /// Ask `peer` to start or stop ringing.
    pub fn ring(&self, peer: DeviceId, on: bool) -> Result<()> {
        self.peers.send(
            peer,
            OutboundPacket::reliable(&Ring { on }, Priority::Control)?,
        )
    }
}

#[async_trait]
impl Plugin for FindMyPlugin {
    fn id(&self) -> &'static str {
        "findmy"
    }

    fn incoming(&self) -> &'static [&'static str] {
        &[Ring::TYPE]
    }

    fn outgoing(&self) -> &'static [&'static str] {
        &[Ring::TYPE]
    }

    async fn on_connected(&self, ctx: &PluginCtx) {
        self.peers.insert(ctx);
    }

    async fn on_packet(&self, ctx: &PluginCtx, packet: Envelope) {
        match packet.body::<Ring>() {
            Ok(Ring { on }) => self.host.ring(&ctx.peer_info(), on),
            Err(e) => debug!(peer = %ctx.peer(), error = %e, "bad ring packet"),
        }
    }

    async fn on_disconnected(&self, peer: DeviceId) {
        self.peers.remove(peer);
    }
}
