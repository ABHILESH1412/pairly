//! Connected-peer bookkeeping shared by the plugins.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

use pairly_core::{CoreError, DeviceId, OutboundPacket, PluginCtx, Result};

pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The plugin contexts of currently connected peers.
#[derive(Default)]
pub(crate) struct Peers(Mutex<HashMap<DeviceId, PluginCtx>>);

impl Peers {
    pub fn insert(&self, ctx: &PluginCtx) {
        lock(&self.0).insert(ctx.peer(), ctx.clone());
    }

    pub fn remove(&self, peer: DeviceId) {
        lock(&self.0).remove(&peer);
    }

    pub fn name(&self, peer: DeviceId) -> Option<String> {
        lock(&self.0).get(&peer).map(|c| c.peer_name().to_owned())
    }

    /// Send to every connected peer that handles this packet type.
    pub fn broadcast(&self, packet: &OutboundPacket) {
        for ctx in lock(&self.0).values() {
            if ctx.peer_accepts(&packet.ty) {
                let _ = ctx.send(packet.clone());
            }
        }
    }

    pub fn send(&self, peer: DeviceId, packet: OutboundPacket) -> Result<()> {
        let ctx = lock(&self.0)
            .get(&peer)
            .cloned()
            .ok_or(CoreError::NotConnected(peer))?;
        ctx.send(packet)?;
        Ok(())
    }
}
