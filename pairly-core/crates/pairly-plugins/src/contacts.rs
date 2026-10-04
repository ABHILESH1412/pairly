//! Contacts: a PC asks a phone for its contacts (names and numbers), to look them up, text them
//! or call them through the phone.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use pairly_core::{
    CoreError, DeviceId, Envelope, OutboundPacket, PacketBody, Plugin, PluginCtx, Priority, Result,
};
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;
use tracing::debug;

use crate::peers::{Peers, lock};

const ANSWER_TIMEOUT: Duration = Duration::from_secs(20);
pub const MAX_CONTACTS: usize = 5000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Contact {
    pub name: String,
    pub numbers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactsRequest {
    pub req: u64,
}

impl PacketBody for ContactsRequest {
    const TYPE: &'static str = "contacts.request";
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactsResponse {
    pub req: u64,
    pub contacts: std::result::Result<Vec<Contact>, String>,
}

impl PacketBody for ContactsResponse {
    const TYPE: &'static str = "contacts.response";
}

pub trait ContactsHost: Send + Sync + 'static {
    /// This device's contacts (the phone); none by default.
    fn contacts(&self) -> std::result::Result<Vec<Contact>, String> {
        Err("this device has no contacts".into())
    }
}

type Answer = std::result::Result<Vec<Contact>, String>;

pub struct ContactsPlugin {
    host: Arc<dyn ContactsHost>,
    peers: Peers,
    next_req: AtomicU64,
    pending: std::sync::Mutex<HashMap<u64, oneshot::Sender<Answer>>>,
}

impl ContactsPlugin {
    pub fn new(host: Arc<dyn ContactsHost>) -> Arc<Self> {
        Arc::new(Self {
            host,
            peers: Peers::default(),
            next_req: AtomicU64::new(1),
            pending: std::sync::Mutex::default(),
        })
    }

    /// A phone's contacts, sorted by name.
    pub async fn fetch(&self, peer: DeviceId) -> Result<Vec<Contact>> {
        let req = self.next_req.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        lock(&self.pending).insert(req, tx);
        let sent = OutboundPacket::reliable(&ContactsRequest { req }, Priority::Interactive)
            .and_then(|p| self.peers.send(peer, p));
        if let Err(e) = sent {
            lock(&self.pending).remove(&req);
            return Err(e);
        }
        let answer = tokio::time::timeout(ANSWER_TIMEOUT, rx).await;
        lock(&self.pending).remove(&req);
        let mut list = answer
            .map_err(|_| CoreError::Timeout)?
            .map_err(|_| CoreError::Closed)?
            .map_err(CoreError::Transport)?;
        list.sort_by_key(|c| c.name.to_lowercase());
        Ok(list)
    }
}

#[async_trait]
impl Plugin for ContactsPlugin {
    fn id(&self) -> &'static str {
        "contacts"
    }

    fn incoming(&self) -> &'static [&'static str] {
        &[ContactsRequest::TYPE, ContactsResponse::TYPE]
    }

    fn outgoing(&self) -> &'static [&'static str] {
        self.incoming()
    }

    async fn on_connected(&self, ctx: &PluginCtx) {
        self.peers.insert(ctx);
    }

    async fn on_packet(&self, ctx: &PluginCtx, packet: Envelope) {
        match packet.ty.as_str() {
            ContactsRequest::TYPE => match packet.body::<ContactsRequest>() {
                Ok(r) => {
                    // A contacts query can take a moment: answer off the packet loop.
                    let (host, ctx) = (self.host.clone(), ctx.clone());
                    tokio::task::spawn_blocking(move || {
                        let contacts = host.contacts().map(|mut c| {
                            c.truncate(MAX_CONTACTS);
                            c
                        });
                        let answer = ContactsResponse {
                            req: r.req,
                            contacts,
                        };
                        if let Ok(p) = OutboundPacket::reliable(&answer, Priority::Bulk) {
                            let _ = ctx.send(p);
                        }
                    });
                }
                Err(e) => debug!(peer = %ctx.peer(), error = %e, "bad contacts.request"),
            },
            ContactsResponse::TYPE => match packet.body::<ContactsResponse>() {
                Ok(r) => {
                    if let Some(tx) = lock(&self.pending).remove(&r.req) {
                        let _ = tx.send(r.contacts);
                    }
                }
                Err(e) => debug!(peer = %ctx.peer(), error = %e, "bad contacts.response"),
            },
            _ => {}
        }
    }

    async fn on_disconnected(&self, peer: DeviceId) {
        self.peers.remove(peer);
    }
}
