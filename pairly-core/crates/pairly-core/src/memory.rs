//! In-process transport for tests: transports on the same [`MemoryNetwork`] discover each other
//! and connect through `tokio::io::duplex` pipes. [`MemoryNetwork::sever`] cuts every live
//! connection, like a Wi-Fi drop, and [`MemoryNetwork::set_reachable`] takes a device off the
//! network (or back) entirely, like leaving home.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio::task::AbortHandle;

use crate::transport::{
    Advertisement, BoxDuplex, PeerCandidate, Transport, TransportEvent, TransportKind,
};
use crate::{CoreError, Result};

const PIPE_CAPACITY: usize = 256 * 1024;

#[derive(Clone)]
struct Endpoint {
    advert: Advertisement,
    events: mpsc::Sender<TransportEvent>,
}

#[derive(Clone, Default)]
pub struct MemoryNetwork {
    endpoints: Arc<Mutex<HashMap<String, Endpoint>>>,
    /// The tasks relaying each live connection.
    links: Arc<Mutex<Vec<AbortHandle>>>,
    /// Addresses currently off the network.
    offline: Arc<Mutex<std::collections::HashSet<String>>>,
}

impl MemoryNetwork {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn transport(&self, address: impl Into<String>) -> Arc<MemoryTransport> {
        Arc::new(MemoryTransport {
            network: self.clone(),
            address: address.into(),
        })
    }

    /// Break every open connection. Both ends see their stream fail; nodes reconnect on their own.
    pub fn sever(&self) {
        let links = std::mem::take(&mut *self.links.lock().unwrap_or_else(PoisonError::into_inner));
        for link in links {
            link.abort();
        }
    }

    /// Take `address` off the network (its connections drop, everyone loses sight of it) or put
    /// it back (everyone discovers it again).
    pub async fn set_reachable(&self, address: &str, reachable: bool) {
        {
            let mut offline = self.offline.lock().unwrap_or_else(PoisonError::into_inner);
            let changed = if reachable {
                offline.remove(address)
            } else {
                offline.insert(address.to_owned())
            };
            if !changed {
                return;
            }
        }
        if !reachable {
            self.sever();
        }
        let endpoints: Vec<(String, Endpoint)> = self
            .lock()
            .iter()
            .map(|(a, e)| (a.clone(), e.clone()))
            .collect();
        let Some(me) = endpoints
            .iter()
            .find(|(a, _)| a == address)
            .map(|(_, e)| e.clone())
        else {
            return;
        };
        let mine = candidate(address, &me.advert);
        for (other, ep) in endpoints.iter().filter(|(a, _)| a != address) {
            let theirs = candidate(other, &ep.advert);
            if reachable {
                let _ = ep
                    .events
                    .send(TransportEvent::Discovered(mine.clone()))
                    .await;
                let _ = me.events.send(TransportEvent::Discovered(theirs)).await;
            } else {
                let _ = ep.events.send(TransportEvent::Lost(mine.clone())).await;
                let _ = me.events.send(TransportEvent::Lost(theirs)).await;
            }
        }
    }

    fn is_offline(&self, address: &str) -> bool {
        self.offline
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(address)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Endpoint>> {
        self.endpoints
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

pub struct MemoryTransport {
    network: MemoryNetwork,
    address: String,
}

fn candidate(address: &str, advert: &Advertisement) -> PeerCandidate {
    PeerCandidate {
        transport: TransportKind::Memory,
        address: address.to_owned(),
        device_id: Some(advert.device_id),
        name: Some(advert.name.clone()),
    }
}

#[async_trait]
impl Transport for MemoryTransport {
    fn kind(&self) -> TransportKind {
        TransportKind::Memory
    }

    async fn start(
        &self,
        advert: Advertisement,
        events: mpsc::Sender<TransportEvent>,
    ) -> Result<()> {
        let me = candidate(&self.address, &advert);
        let others: Vec<(String, Endpoint)> = {
            let mut map = self.network.lock();
            let others = map.iter().map(|(a, e)| (a.clone(), e.clone())).collect();
            map.insert(
                self.address.clone(),
                Endpoint {
                    advert,
                    events: events.clone(),
                },
            );
            others
        };
        for (address, ep) in others {
            let _ = events
                .send(TransportEvent::Discovered(candidate(&address, &ep.advert)))
                .await;
            let _ = ep.events.send(TransportEvent::Discovered(me.clone())).await;
        }
        Ok(())
    }

    fn listen_addresses(&self) -> Vec<String> {
        vec![self.address.clone()]
    }

    async fn connect(&self, target: &PeerCandidate) -> Result<BoxDuplex> {
        if self.network.is_offline(&self.address) || self.network.is_offline(&target.address) {
            return Err(CoreError::Transport(format!(
                "{} unreachable",
                target.address
            )));
        }
        let ep = self.network.lock().get(&target.address).cloned();
        let ep =
            ep.ok_or_else(|| CoreError::Transport(format!("{} unreachable", target.address)))?;
        // Two pipes joined by a relay task, so `sever` can cut the connection from outside.
        let (ours, mut near) = tokio::io::duplex(PIPE_CAPACITY);
        let (mut far, theirs) = tokio::io::duplex(PIPE_CAPACITY);
        let relay = tokio::spawn(async move {
            let _ = tokio::io::copy_bidirectional(&mut near, &mut far).await;
        });
        {
            let mut links = self
                .network
                .links
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            links.retain(|l| !l.is_finished());
            links.push(relay.abort_handle());
        }
        ep.events
            .send(TransportEvent::Incoming {
                stream: Box::new(theirs),
                transport: TransportKind::Memory,
            })
            .await
            .map_err(|_| CoreError::Transport(format!("{} is not listening", target.address)))?;
        Ok(Box::new(ours))
    }

    async fn stop(&self) {
        let (me, others): (Option<Endpoint>, Vec<Endpoint>) = {
            let mut map = self.network.lock();
            let me = map.remove(&self.address);
            (me, map.values().cloned().collect())
        };
        if let Some(me) = me {
            let lost = candidate(&self.address, &me.advert);
            for ep in others {
                let _ = ep.events.send(TransportEvent::Lost(lost.clone())).await;
            }
        }
    }
}
