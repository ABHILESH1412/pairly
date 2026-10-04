//! Power: a PC asks a phone to lock its screen, or to power off or restart, and hears whether
//! that worked (a phone may need a permission first).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use pairly_core::{
    CoreError, DeviceId, Envelope, OutboundPacket, PacketBody, PeerInfo, Plugin, PluginCtx,
    Priority, Result,
};
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;
use tracing::debug;

use crate::peers::{Peers, lock};

const ANSWER_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PowerAction {
    Lock,
    PowerOff,
    Restart,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PowerRequest {
    pub req: u64,
    pub action: PowerAction,
}

impl PacketBody for PowerRequest {
    const TYPE: &'static str = "power.request";
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PowerResponse {
    pub req: u64,
    pub result: std::result::Result<(), String>,
}

impl PacketBody for PowerResponse {
    const TYPE: &'static str = "power.response";
}

pub trait PowerHost: Send + Sync + 'static {
    /// Carry out `action` because `from` asked; refused by default.
    fn act(&self, _from: &PeerInfo, _action: PowerAction) -> std::result::Result<(), String> {
        Err("this device can't do that".into())
    }
}

type Answer = std::result::Result<(), String>;

pub struct PowerPlugin {
    host: Arc<dyn PowerHost>,
    peers: Peers,
    next_req: AtomicU64,
    pending: std::sync::Mutex<HashMap<u64, oneshot::Sender<Answer>>>,
}

impl PowerPlugin {
    pub fn new(host: Arc<dyn PowerHost>) -> Arc<Self> {
        Arc::new(Self {
            host,
            peers: Peers::default(),
            next_req: AtomicU64::new(1),
            pending: std::sync::Mutex::default(),
        })
    }

    /// Ask `peer` to lock, power off or restart; resolves once it has (or couldn't).
    pub async fn request(&self, peer: DeviceId, action: PowerAction) -> Result<()> {
        let req = self.next_req.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        lock(&self.pending).insert(req, tx);
        let sent = OutboundPacket::reliable(&PowerRequest { req, action }, Priority::Control)
            .and_then(|p| self.peers.send(peer, p));
        if let Err(e) = sent {
            lock(&self.pending).remove(&req);
            return Err(e);
        }
        let answer = tokio::time::timeout(ANSWER_TIMEOUT, rx).await;
        lock(&self.pending).remove(&req);
        answer
            .map_err(|_| CoreError::Timeout)?
            .map_err(|_| CoreError::Closed)?
            .map_err(CoreError::Transport)
    }
}

#[async_trait]
impl Plugin for PowerPlugin {
    fn id(&self) -> &'static str {
        "power"
    }

    fn incoming(&self) -> &'static [&'static str] {
        &[PowerRequest::TYPE, PowerResponse::TYPE]
    }

    fn outgoing(&self) -> &'static [&'static str] {
        self.incoming()
    }

    async fn on_connected(&self, ctx: &PluginCtx) {
        self.peers.insert(ctx);
    }

    async fn on_packet(&self, ctx: &PluginCtx, packet: Envelope) {
        match packet.ty.as_str() {
            PowerRequest::TYPE => match packet.body::<PowerRequest>() {
                Ok(r) => {
                    let (host, ctx) = (self.host.clone(), ctx.clone());
                    tokio::task::spawn_blocking(move || {
                        let result = host.act(&ctx.peer_info(), r.action);
                        let answer = PowerResponse { req: r.req, result };
                        if let Ok(p) = OutboundPacket::reliable(&answer, Priority::Control) {
                            let _ = ctx.send(p);
                        }
                    });
                }
                Err(e) => debug!(peer = %ctx.peer(), error = %e, "bad power.request"),
            },
            PowerResponse::TYPE => match packet.body::<PowerResponse>() {
                Ok(r) => {
                    if let Some(tx) = lock(&self.pending).remove(&r.req) {
                        let _ = tx.send(r.result);
                    }
                }
                Err(e) => debug!(peer = %ctx.peer(), error = %e, "bad power.response"),
            },
            _ => {}
        }
    }

    async fn on_disconnected(&self, peer: DeviceId) {
        self.peers.remove(peer);
    }
}
