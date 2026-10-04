//! Blind relay: pairs two QUIC streams that join the same opaque room and pipes bytes between
//! them. It never sees device identities, keys or plaintext (see
//! `pairly_transport_relay::proto` for the protocol).
#![forbid(unsafe_code)]

mod hub;

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use pairly_transport_relay::proto::{self, Join, Role};
use pairly_transport_relay::tls;
use tokio::task::JoinHandle;
use tracing::{debug, info};

use crate::hub::Hub;

/// How long a new stream may take to say what it wants.
const JOIN_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone)]
pub struct Config {
    pub listen: SocketAddr,
    /// Accepted access tokens. Empty: anyone who has the address may use the relay.
    pub tokens: Vec<String>,
    pub max_conns_per_ip: usize,
    /// Rooms one connection (one device) may listen in.
    pub max_rooms_per_conn: usize,
    /// Per pipe and direction, in bytes per second. `None`: unlimited.
    pub rate_limit: Option<u64>,
}

impl Config {
    pub fn new(listen: SocketAddr) -> Self {
        Self {
            listen,
            tokens: Vec::new(),
            max_conns_per_ip: 32,
            max_rooms_per_conn: 64,
            rate_limit: None,
        }
    }
}

#[derive(Debug, Default)]
pub struct Stats {
    pub connections: AtomicU64,
    pub connections_total: AtomicU64,
    pub pipes: AtomicU64,
    pub pipes_total: AtomicU64,
    pub bytes: AtomicU64,
    pub denied: AtomicU64,
}

impl Stats {
    pub fn summary(&self) -> String {
        let get = |a: &AtomicU64| a.load(Ordering::Relaxed);
        format!(
            "connections={} (total {}) pipes={} (total {}) relayed={} MB denied={}",
            get(&self.connections),
            get(&self.connections_total),
            get(&self.pipes),
            get(&self.pipes_total),
            get(&self.bytes) / 1_000_000,
            get(&self.denied),
        )
    }
}

pub struct Relay {
    endpoint: quinn::Endpoint,
    pin: [u8; 32],
    stats: Arc<Stats>,
    task: JoinHandle<()>,
}

impl Relay {
    /// Bind and start serving with the given certificate (`tls::generate_cert` makes one).
    pub fn start(config: Config, cert_der: Vec<u8>, key_der: Vec<u8>) -> anyhow::Result<Self> {
        let pin = tls::pin_of(&cert_der);
        let server =
            tls::server_config(cert_der, key_der, 512).map_err(anyhow::Error::from_boxed)?;
        let endpoint = quinn::Endpoint::server(server, config.listen)?;
        let stats = Arc::new(Stats::default());
        let hub = Arc::new(Hub::new(config.rate_limit, stats.clone()));
        let task = tokio::spawn(accept_loop(
            endpoint.clone(),
            Arc::new(config),
            hub,
            stats.clone(),
        ));
        Ok(Self {
            endpoint,
            pin,
            stats,
            task,
        })
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.endpoint.local_addr()
    }

    pub fn pin(&self) -> [u8; 32] {
        self.pin
    }

    pub fn stats(&self) -> &Stats {
        &self.stats
    }

    pub async fn shutdown(self) {
        self.task.abort();
        self.endpoint.close(0u32.into(), b"relay shutting down");
        // Give the close a moment to reach clients, but don't wait out the idle timeout for
        // ones that vanished without saying goodbye.
        let _ = tokio::time::timeout(Duration::from_secs(2), self.endpoint.wait_idle()).await;
    }
}

async fn accept_loop(
    endpoint: quinn::Endpoint,
    config: Arc<Config>,
    hub: Arc<Hub>,
    stats: Arc<Stats>,
) {
    let per_ip: Arc<Mutex<HashMap<IpAddr, usize>>> = Arc::default();
    let mut next_id: u64 = 0;
    while let Some(incoming) = endpoint.accept().await {
        let ip = incoming.remote_address().ip().to_canonical();
        {
            let mut counts = per_ip.lock().unwrap_or_else(PoisonError::into_inner);
            let count = counts.entry(ip).or_default();
            if *count >= config.max_conns_per_ip {
                stats.denied.fetch_add(1, Ordering::Relaxed);
                debug!(%ip, "too many connections from this address");
                incoming.refuse();
                continue;
            }
            *count += 1;
        }
        next_id += 1;
        let (id, config, hub, stats, per_ip) = (
            next_id,
            config.clone(),
            hub.clone(),
            stats.clone(),
            per_ip.clone(),
        );
        tokio::spawn(async move {
            if let Ok(conn) = incoming.await {
                stats.connections.fetch_add(1, Ordering::Relaxed);
                stats.connections_total.fetch_add(1, Ordering::Relaxed);
                serve_conn(id, &conn, &config, &hub, &stats).await;
                hub.connection_closed(id);
                stats.connections.fetch_sub(1, Ordering::Relaxed);
            }
            let mut counts = per_ip.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(count) = counts.get_mut(&ip) {
                *count -= 1;
                if *count == 0 {
                    counts.remove(&ip);
                }
            }
        });
    }
}

async fn serve_conn(
    id: u64,
    conn: &quinn::Connection,
    config: &Arc<Config>,
    hub: &Arc<Hub>,
    stats: &Arc<Stats>,
) {
    while let Ok((send, recv)) = conn.accept_bi().await {
        let (config, hub, stats) = (config.clone(), hub.clone(), stats.clone());
        tokio::spawn(async move {
            serve_stream(id, send, recv, &config, &hub, &stats).await;
        });
    }
}

async fn serve_stream(
    conn: u64,
    mut send: quinn::SendStream,
    mut recv: quinn::RecvStream,
    config: &Config,
    hub: &Arc<Hub>,
    stats: &Stats,
) {
    let join = match tokio::time::timeout(JOIN_TIMEOUT, Join::read(&mut recv)).await {
        Ok(Ok(join)) => join,
        _ => return,
    };
    let authorized = config.tokens.is_empty()
        || join
            .token
            .as_ref()
            .is_some_and(|t| config.tokens.iter().any(|ok| ok == t));
    if !authorized {
        stats.denied.fetch_add(1, Ordering::Relaxed);
        let _ = send.write_all(&[proto::DENIED]).await;
        let _ = send.finish();
        return;
    }
    match join.role {
        Role::Listen => {
            if !hub.listen(conn, join.room, send, recv, config.max_rooms_per_conn) {
                stats.denied.fetch_add(1, Ordering::Relaxed);
            }
        }
        Role::Dial => hub.dial(conn, join.room, send, recv),
    }
}

/// Log a stats line every `interval` until cancelled.
pub async fn report_stats(stats: Arc<Stats>, interval: Duration) {
    let mut tick = tokio::time::interval(interval);
    tick.tick().await;
    loop {
        tick.tick().await;
        info!("{}", stats.summary());
    }
}

impl Relay {
    pub fn stats_handle(&self) -> Arc<Stats> {
        self.stats.clone()
    }
}
