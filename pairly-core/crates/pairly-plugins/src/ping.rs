//! Ping: a tiny "are you there?" that pops a notification on the other device.

use async_trait::async_trait;
use pairly_core::{Envelope, OutboundPacket, PacketBody, Plugin, PluginCtx, Priority, Result};
use serde::{Deserialize, Serialize};
use tracing::debug;

/// Longest message forwarded to the platform.
const MAX_MESSAGE_CHARS: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ping {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl PacketBody for Ping {
    const TYPE: &'static str = "ping";
}

/// Build a ping packet to send with [`pairly_core::PairlyNode::send`].
pub fn packet(message: Option<String>) -> Result<OutboundPacket> {
    OutboundPacket::reliable(&Ping { message }, Priority::Interactive)
}

pub struct PingPlugin;

#[async_trait]
impl Plugin for PingPlugin {
    fn id(&self) -> &'static str {
        "ping"
    }

    fn incoming(&self) -> &'static [&'static str] {
        &[Ping::TYPE]
    }

    fn outgoing(&self) -> &'static [&'static str] {
        &[Ping::TYPE]
    }

    async fn on_packet(&self, ctx: &PluginCtx, packet: Envelope) {
        let Ok(ping) = packet.body::<Ping>() else {
            debug!(peer = %ctx.peer(), "malformed ping");
            return;
        };
        let message = ping.message.map(|m| {
            let clean: String = m
                .chars()
                .filter(|c| !c.is_control())
                .take(MAX_MESSAGE_CHARS)
                .collect();
            clean
        });
        ctx.platform().ping_received(
            &ctx.peer_info(),
            message.as_deref().filter(|m| !m.is_empty()),
        );
    }
}
