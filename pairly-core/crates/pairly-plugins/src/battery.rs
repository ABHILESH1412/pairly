//! Battery level: each device sends its state on connect and whenever it changes.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use pairly_core::{
    DeviceId, Envelope, OutboundPacket, PacketBody, PeerInfo, Plugin, PluginCtx, Priority,
};
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::peers::{Peers, lock};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatteryState {
    /// 0–100.
    pub percent: u8,
    pub charging: bool,
}

impl PacketBody for BatteryState {
    const TYPE: &'static str = "battery.state";
}

pub trait BatteryHost: Send + Sync + 'static {
    /// This device's battery, if it has one.
    fn current(&self) -> Option<BatteryState>;
    /// A peer reported its battery (`previous` is the last state we had, for crossing alerts).
    fn peer_changed(&self, from: &PeerInfo, state: BatteryState, previous: Option<BatteryState>);
}

pub struct BatteryPlugin {
    host: Arc<dyn BatteryHost>,
    peers: Peers,
    local: Mutex<Option<BatteryState>>,
    remote: Mutex<HashMap<DeviceId, BatteryState>>,
}

impl BatteryPlugin {
    pub fn new(host: Arc<dyn BatteryHost>) -> Arc<Self> {
        Arc::new(Self {
            host,
            peers: Peers::default(),
            local: Mutex::default(),
            remote: Mutex::default(),
        })
    }

    /// The local battery changed; repeated identical states are ignored.
    pub fn local_changed(&self, state: BatteryState) {
        let state = BatteryState {
            percent: state.percent.min(100),
            ..state
        };
        if lock(&self.local).replace(state) == Some(state) {
            return;
        }
        if let Ok(packet) = OutboundPacket::unreliable(&state, Priority::Interactive) {
            self.peers.broadcast(&packet);
        }
    }

    /// The last state a connected peer reported.
    pub fn peer_state(&self, peer: DeviceId) -> Option<BatteryState> {
        lock(&self.remote).get(&peer).copied()
    }
}

#[async_trait]
impl Plugin for BatteryPlugin {
    fn id(&self) -> &'static str {
        "battery"
    }

    fn incoming(&self) -> &'static [&'static str] {
        &[BatteryState::TYPE]
    }

    fn outgoing(&self) -> &'static [&'static str] {
        &[BatteryState::TYPE]
    }

    async fn on_connected(&self, ctx: &PluginCtx) {
        self.peers.insert(ctx);
        let state = self.host.current().or(*lock(&self.local));
        if let Some(state) = state
            && ctx.peer_accepts(BatteryState::TYPE)
            && let Ok(packet) = OutboundPacket::unreliable(&state, Priority::Interactive)
        {
            *lock(&self.local) = Some(state);
            let _ = ctx.send(packet);
        }
    }

    async fn on_packet(&self, ctx: &PluginCtx, packet: Envelope) {
        match packet.body::<BatteryState>() {
            Ok(state) => {
                let state = BatteryState {
                    percent: state.percent.min(100),
                    ..state
                };
                let previous = lock(&self.remote).insert(ctx.peer(), state);
                self.host.peer_changed(&ctx.peer_info(), state, previous);
            }
            Err(e) => debug!(peer = %ctx.peer(), error = %e, "bad battery packet"),
        }
    }

    async fn on_disconnected(&self, peer: DeviceId) {
        self.peers.remove(peer);
        // Don't show a stale level for a device that is offline.
        lock(&self.remote).remove(&peer);
    }
}
