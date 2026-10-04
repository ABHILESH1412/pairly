//! Feature plugins. Packets are routed to the plugin that declared their type in
//! [`Plugin::incoming`]; a plugin may only send types the peer declared it can handle.

use std::sync::Arc;

use async_trait::async_trait;
use pairly_crypto::DeviceId;
use pairly_proto::Envelope;
use pairly_proto::packets::Identity;

use crate::platform::{PeerInfo, Platform};
use crate::session::{OutboundPacket, Session};
use crate::{CoreError, Result};

#[async_trait]
pub trait Plugin: Send + Sync + 'static {
    fn id(&self) -> &'static str;
    /// Packet types this plugin handles.
    fn incoming(&self) -> &'static [&'static str];
    /// Packet types this plugin may send.
    fn outgoing(&self) -> &'static [&'static str];

    async fn on_connected(&self, _ctx: &PluginCtx) {}
    async fn on_packet(&self, ctx: &PluginCtx, packet: Envelope);
    async fn on_disconnected(&self, _peer: DeviceId) {}
}

/// A plugin's handle on one connected peer.
#[derive(Clone)]
pub struct PluginCtx {
    pub(crate) peer: DeviceId,
    pub(crate) identity: Arc<Identity>,
    pub(crate) session: Arc<Session>,
    pub(crate) platform: Arc<dyn Platform>,
}

impl PluginCtx {
    pub fn peer(&self) -> DeviceId {
        self.peer
    }

    pub fn peer_name(&self) -> &str {
        &self.identity.name
    }

    pub fn peer_info(&self) -> PeerInfo {
        PeerInfo {
            id: self.peer,
            name: self.identity.name.clone(),
        }
    }

    pub fn platform(&self) -> &Arc<dyn Platform> {
        &self.platform
    }

    pub fn peer_accepts(&self, ty: &str) -> bool {
        accepts(&self.identity, ty)
    }

    /// Queue a packet to the peer. Returns its id, or `None` if it was unreliable and dropped.
    pub fn send(&self, packet: OutboundPacket) -> Result<Option<u64>> {
        if !self.peer_accepts(&packet.ty) {
            return Err(CoreError::Unsupported(packet.ty));
        }
        self.session.send(packet)
    }
}

pub(crate) fn accepts(identity: &Identity, ty: &str) -> bool {
    identity.incoming.iter().any(|t| t == ty)
}
