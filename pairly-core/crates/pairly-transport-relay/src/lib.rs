//! Internet transport through a blind `pairly-relay` (see [`proto`]).
//!
//! For every paired device the node reports (with the relays it should be met on), this keeps a
//! listener in the device's room. The relay says when the device is there too, which becomes a
//! [`PeerCandidate`]; dialing it pairs a stream with the device's listener. Each side's Noise
//! session then runs over that pipe exactly as it would over LAN.
#![forbid(unsafe_code)]

pub mod proto;
pub mod tls;

use std::collections::HashMap;
use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use pairly_core::transport::{Advertisement, BoxDuplex, PairedPeer};
use pairly_core::{
    CoreError, DeviceId, PeerCandidate, Result, Transport, TransportEvent, TransportKind,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf};
use tokio::sync::{mpsc, watch};
use tokio::task::{AbortHandle, JoinSet};
use tracing::{debug, info, warn};

pub use proto::{BadRelayAddr, RelayAddr};
use proto::{Join, Role, Room};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a dial waits for the relay to pair it.
const DIAL_TIMEOUT: Duration = Duration::from_secs(10);
const RETRY_MIN: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(60);

fn err(e: impl std::fmt::Display) -> CoreError {
    CoreError::Transport(e.to_string())
}

#[derive(Debug, Clone, Default)]
pub struct RelayConfig {
    /// Only use the relay for devices without a better link (phones: saves battery, since an
    /// idle relay connection still pings every 15 s). Otherwise stay in every room.
    pub only_when_needed: bool,
}

/// Rooms to listen in on one relay, and which device each belongs to.
type Rooms = HashMap<Room, DeviceId>;

struct RelayHandle {
    rooms: watch::Sender<Rooms>,
    conn: watch::Receiver<Option<quinn::Connection>>,
    addr: RelayAddr,
}

struct Running {
    endpoint: quinn::Endpoint,
    events: mpsc::Sender<TransportEvent>,
    relays: HashMap<String, RelayHandle>,
}

pub struct RelayTransport {
    config: RelayConfig,
    running: Mutex<Option<Running>>,
}

impl RelayTransport {
    pub fn new(config: RelayConfig) -> Arc<Self> {
        Arc::new(Self {
            config,
            running: Mutex::new(None),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Option<Running>> {
        self.running.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn bind() -> io::Result<UdpSocket> {
    UdpSocket::bind((Ipv6Addr::UNSPECIFIED, 0))
        .or_else(|_| UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)))
}

/// Candidate addresses are `<room hex> <relay address>`; only this transport reads them.
fn candidate_address(room: &Room, relay: &str) -> String {
    let hex: String = room.iter().map(|b| format!("{b:02x}")).collect();
    format!("{hex} {relay}")
}

fn parse_candidate(address: &str) -> Option<(Room, &str)> {
    let (hex, relay) = address.split_once(' ')?;
    if hex.len() != proto::ROOM_LEN * 2 {
        return None;
    }
    let mut room = [0u8; proto::ROOM_LEN];
    for (i, b) in room.iter_mut().enumerate() {
        *b = u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some((room, relay))
}

#[async_trait]
impl Transport for RelayTransport {
    fn kind(&self) -> TransportKind {
        TransportKind::Relay
    }

    async fn start(
        &self,
        _advert: Advertisement,
        events: mpsc::Sender<TransportEvent>,
    ) -> Result<()> {
        let socket = bind()?;
        let endpoint = quinn::Endpoint::new(
            quinn::EndpointConfig::default(),
            None,
            socket,
            Arc::new(quinn::TokioRuntime),
        )?;
        *self.lock() = Some(Running {
            endpoint,
            events,
            relays: HashMap::new(),
        });
        Ok(())
    }

    fn peers_changed(&self, peers: &[PairedPeer]) {
        // relay address -> rooms we want to be in there
        let mut wanted: HashMap<String, (RelayAddr, Rooms)> = HashMap::new();
        for peer in peers {
            let direct = peer
                .link
                .is_some_and(|l| l.rank() > TransportKind::Relay.rank());
            if self.config.only_when_needed && direct {
                continue;
            }
            for relay in &peer.relays {
                match relay.parse::<RelayAddr>() {
                    Ok(addr) => {
                        wanted
                            .entry(addr.to_string())
                            .or_insert_with(|| (addr, Rooms::new()))
                            .1
                            .insert(peer.rendezvous, peer.id);
                    }
                    Err(e) => debug!(error = %e, "ignoring relay"),
                }
            }
        }
        let mut guard = self.lock();
        let Some(running) = guard.as_mut() else {
            return;
        };
        // Dropping a handle's room sender tells its task to leave and stop.
        running.relays.retain(|key, _| wanted.contains_key(key));
        for (key, (addr, rooms)) in wanted {
            if let Some(handle) = running.relays.get(&key) {
                handle.rooms.send_if_modified(|current| {
                    let changed = *current != rooms;
                    *current = rooms;
                    changed
                });
                continue;
            }
            let client = match tls::client_config(addr.pin) {
                Ok(c) => c,
                Err(e) => {
                    warn!(error = %e, "can't set up TLS for the relay");
                    continue;
                }
            };
            let (rooms_tx, rooms_rx) = watch::channel(rooms);
            let (conn_tx, conn_rx) = watch::channel(None);
            tokio::spawn(run_relay(
                addr.clone(),
                key.clone(),
                client,
                running.endpoint.clone(),
                running.events.clone(),
                rooms_rx,
                conn_tx,
            ));
            running.relays.insert(
                key,
                RelayHandle {
                    rooms: rooms_tx,
                    conn: conn_rx,
                    addr,
                },
            );
        }
    }

    async fn connect(&self, candidate: &PeerCandidate) -> Result<BoxDuplex> {
        let (room, relay) = parse_candidate(&candidate.address)
            .ok_or_else(|| err(format!("bad relay candidate {:?}", candidate.address)))?;
        let (conn, token) = {
            let guard = self.lock();
            let handle = guard
                .as_ref()
                .and_then(|r| r.relays.get(relay))
                .ok_or_else(|| err("not using that relay"))?;
            let conn = handle.conn.borrow().clone();
            (
                conn.ok_or_else(|| err("relay not connected"))?,
                handle.addr.token.clone(),
            )
        };
        let dial = async {
            let (mut send, mut recv) = conn.open_bi().await.map_err(err)?;
            let join = Join {
                role: Role::Dial,
                room,
                token,
            };
            send.write_all(&join.encode()).await.map_err(err)?;
            match recv.read_u8().await.map_err(err)? {
                proto::MATCHED => Ok(RelayStream {
                    send: Some(send),
                    recv,
                }),
                proto::NO_PEER => Err(err("the device isn't on the relay")),
                proto::DENIED => Err(err("the relay refused (check the access token)")),
                other => Err(err(format!("unexpected relay reply {other}"))),
            }
        };
        let stream = tokio::time::timeout(DIAL_TIMEOUT, dial)
            .await
            .map_err(|_| CoreError::Timeout)??;
        Ok(Box::new(stream))
    }

    async fn stop(&self) {
        if let Some(running) = self.lock().take() {
            // Dropping the handles ends the relay tasks.
            running.endpoint.close(0u32.into(), b"bye");
        }
    }
}

async fn connect_relay(
    endpoint: &quinn::Endpoint,
    addr: &RelayAddr,
    client: quinn::ClientConfig,
) -> Result<quinn::Connection> {
    let connect = async {
        let target = tokio::net::lookup_host(addr.authority())
            .await?
            .next()
            .ok_or_else(|| err(format!("{} has no address", addr.host)))?;
        // An IPv6-only socket can't reach IPv4 relays and vice versa; dual-stack sockets take
        // IPv4 as mapped addresses.
        let target = match (target, endpoint.local_addr()) {
            (SocketAddr::V4(v4), Ok(SocketAddr::V6(_))) => {
                SocketAddr::new(v4.ip().to_ipv6_mapped().into(), v4.port())
            }
            _ => target,
        };
        endpoint
            .connect_with(client, target, proto::SERVER_NAME)
            .map_err(err)?
            .await
            .map_err(err)
    };
    tokio::time::timeout(CONNECT_TIMEOUT, connect)
        .await
        .map_err(|_| CoreError::Timeout)?
}

/// Keep a connection to one relay and a listener in each wanted room, until the room sender is
/// dropped.
async fn run_relay(
    addr: RelayAddr,
    key: String,
    client: quinn::ClientConfig,
    endpoint: quinn::Endpoint,
    events: mpsc::Sender<TransportEvent>,
    mut rooms: watch::Receiver<Rooms>,
    conn_tx: watch::Sender<Option<quinn::Connection>>,
) {
    let mut retry = RETRY_MIN;
    loop {
        let conn = tokio::select! {
            c = connect_relay(&endpoint, &addr, client.clone()) => c,
            r = rooms.changed() => {
                if r.is_err() { return; }
                continue;
            }
        };
        let conn = match conn {
            Ok(c) => c,
            Err(e) => {
                debug!(relay = ?addr, error = %e, "can't reach the relay");
                tokio::select! {
                    () = tokio::time::sleep(retry) => {}
                    r = rooms.changed() => if r.is_err() { return; },
                }
                retry = (retry * 2).min(RETRY_MAX);
                continue;
            }
        };
        info!(relay = ?addr, "connected to relay");
        retry = RETRY_MIN;
        conn_tx.send_replace(Some(conn.clone()));

        let present: Arc<Mutex<HashMap<Room, PeerCandidate>>> = Arc::default();
        let mut listeners: HashMap<Room, AbortHandle> = HashMap::new();
        let mut tasks = JoinSet::new();
        let stop = loop {
            let wanted = rooms.borrow_and_update().clone();
            listeners.retain(|room, task| {
                let keep = wanted.contains_key(room);
                if !keep {
                    task.abort();
                }
                keep
            });
            for (room, id) in &wanted {
                if listeners.contains_key(room) {
                    continue;
                }
                let listener = Listener {
                    conn: conn.clone(),
                    room: *room,
                    id: *id,
                    token: addr.token.clone(),
                    candidate: PeerCandidate {
                        transport: TransportKind::Relay,
                        address: candidate_address(room, &key),
                        device_id: Some(*id),
                        name: None,
                    },
                    events: events.clone(),
                    present: present.clone(),
                };
                listeners.insert(*room, tasks.spawn(listener.run()));
            }
            // Rooms we left: the device is no longer reachable here.
            let gone: Vec<PeerCandidate> = {
                let mut p = present.lock().unwrap_or_else(PoisonError::into_inner);
                let rooms_gone: Vec<Room> = p
                    .keys()
                    .filter(|r| !wanted.contains_key(*r))
                    .copied()
                    .collect();
                rooms_gone.iter().filter_map(|r| p.remove(r)).collect()
            };
            for c in gone {
                let _ = events.send(TransportEvent::Lost(c)).await;
            }
            tokio::select! {
                _ = conn.closed() => break false,
                r = rooms.changed() => if r.is_err() { break true; },
            }
        };
        tasks.abort_all();
        conn_tx.send_replace(None);
        let lost: Vec<PeerCandidate> = present
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .drain()
            .map(|(_, c)| c)
            .collect();
        for c in lost {
            let _ = events.send(TransportEvent::Lost(c)).await;
        }
        if stop {
            conn.close(0u32.into(), b"done");
            return;
        }
        debug!(relay = ?addr, reason = ?conn.close_reason(), "relay connection lost");
        tokio::select! {
            () = tokio::time::sleep(retry) => {}
            r = rooms.changed() => if r.is_err() { return; },
        }
        retry = (retry * 2).min(RETRY_MAX);
    }
}

/// Waits in one room: reports the device's presence and hands over its calls.
struct Listener {
    conn: quinn::Connection,
    room: Room,
    id: DeviceId,
    token: Option<String>,
    candidate: PeerCandidate,
    events: mpsc::Sender<TransportEvent>,
    present: Arc<Mutex<HashMap<Room, PeerCandidate>>>,
}

impl Listener {
    async fn run(self) {
        loop {
            match self.listen_once().await {
                Ok(()) => {}
                Err(e) => {
                    if self.conn.close_reason().is_some() {
                        return;
                    }
                    debug!(id = %self.id, error = %e, "relay listener failed");
                    tokio::time::sleep(RETRY_MIN * 2).await;
                }
            }
        }
    }

    fn set_present(&self, present: bool) -> bool {
        let mut p = self.present.lock().unwrap_or_else(PoisonError::into_inner);
        if present {
            p.insert(self.room, self.candidate.clone());
            true
        } else {
            p.remove(&self.room).is_some()
        }
    }

    /// One listening stream, until the device calls (`Ok`) or something fails.
    async fn listen_once(&self) -> Result<()> {
        let (mut send, mut recv) = self.conn.open_bi().await.map_err(err)?;
        let join = Join {
            role: Role::Listen,
            room: self.room,
            token: self.token.clone(),
        };
        send.write_all(&join.encode()).await.map_err(err)?;
        loop {
            match recv.read_u8().await.map_err(err)? {
                proto::PRESENT => {
                    self.set_present(true);
                    // Sent again on every report: "seen again" wakes a waiting reconnect.
                    let _ = self
                        .events
                        .send(TransportEvent::Discovered(self.candidate.clone()))
                        .await;
                }
                proto::ABSENT => {
                    if self.set_present(false) {
                        let _ = self
                            .events
                            .send(TransportEvent::Lost(self.candidate.clone()))
                            .await;
                    }
                }
                proto::MATCHED => {
                    let stream = RelayStream {
                        send: Some(send),
                        recv,
                    };
                    let _ = self
                        .events
                        .send(TransportEvent::Incoming {
                            stream: Box::new(stream),
                            transport: TransportKind::Relay,
                        })
                        .await;
                    return Ok(());
                }
                proto::DENIED => {
                    warn!("the relay refused to listen (check the access token in its address)");
                    // Don't hammer a relay that said no.
                    tokio::time::sleep(RETRY_MAX).await;
                    return Err(err("denied"));
                }
                other => return Err(err(format!("unexpected relay message {other}"))),
            }
        }
    }
}

/// A piped stream through the relay.
pub struct RelayStream {
    send: Option<quinn::SendStream>,
    recv: quinn::RecvStream,
}

impl AsyncRead for RelayStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().recv).poll_read(cx, buf)
    }
}

impl AsyncWrite for RelayStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut().send.as_mut() {
            Some(send) => AsyncWrite::poll_write(Pin::new(send), cx, buf),
            None => Poll::Ready(Err(io::ErrorKind::BrokenPipe.into())),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut().send.as_mut() {
            Some(send) => AsyncWrite::poll_flush(Pin::new(send), cx),
            None => Poll::Ready(Ok(())),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut().send.as_mut() {
            Some(send) => AsyncWrite::poll_shutdown(Pin::new(send), cx),
            None => Poll::Ready(Ok(())),
        }
    }
}

impl Drop for RelayStream {
    fn drop(&mut self) {
        // Finish (rather than reset) so bytes already written still reach the other side.
        if let Some(mut send) = self.send.take() {
            let _ = send.finish();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidate_addresses_round_trip() {
        let room = [0xab; proto::ROOM_LEN];
        let a = candidate_address(&room, "pairly-relay://h:1/x");
        assert_eq!(parse_candidate(&a), Some((room, "pairly-relay://h:1/x")));
        assert_eq!(parse_candidate("zz pairly-relay://h:1/x"), None);
    }
}
