//! LAN transport: QUIC (quinn) streams plus mDNS (`_pairly._udp.local.`) discovery.
//!
//! Each Pairly channel is one QUIC connection carrying one bidirectional stream. QUIC's TLS
//! uses a throwaway self-signed certificate and the client skips certificate checks:
//! authentication is the Noise handshake that runs inside the stream (see `plan.md` §6.4).
#![forbid(unsafe_code)]

mod quic;

use std::collections::HashMap;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use async_trait::async_trait;
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use pairly_core::transport::{
    Advertisement, BoxDuplex, PeerCandidate, Transport, TransportEvent, TransportKind,
};
use pairly_core::{CoreError, DeviceId, PROTOCOL_VERSION, Result};
use tokio::sync::mpsc;
use tokio::task::AbortHandle;
use tracing::{debug, info, warn};

pub use quic::QuicEndpoint;

pub const SERVICE_TYPE: &str = "_pairly._udp.local.";
pub const DEFAULT_PORT: u16 = 47100;

#[derive(Debug, Clone)]
pub struct LanConfig {
    /// UDP port to listen on. If taken, a random port is used (and advertised).
    pub port: u16,
    /// Advertise and browse with mDNS. Without it, only explicit connects work.
    pub mdns: bool,
}

impl Default for LanConfig {
    fn default() -> Self {
        Self {
            port: DEFAULT_PORT,
            mdns: true,
        }
    }
}

struct Mdns {
    daemon: ServiceDaemon,
    info: ServiceInfo,
    events: mpsc::Sender<TransportEvent>,
    browse: AbortHandle,
}

impl Mdns {
    fn start(
        advert: &Advertisement,
        port: u16,
        events: mpsc::Sender<TransportEvent>,
    ) -> Result<Self, mdns_sd::Error> {
        let daemon = ServiceDaemon::new()?;
        let id = advert.device_id.to_string();
        let version = PROTOCOL_VERSION.to_string();
        let properties = [
            ("id", id.as_str()),
            ("name", advert.name.as_str()),
            ("type", advert.device_type.as_str()),
            ("v", version.as_str()),
        ];
        let info = ServiceInfo::new(
            SERVICE_TYPE,
            &id,
            &format!("pairly-{id}.local."),
            "",
            port,
            &properties[..],
        )?
        .enable_addr_auto();
        daemon.register(info.clone())?;
        let browse = spawn_browse(daemon.browse(SERVICE_TYPE)?, events.clone());
        Ok(Self {
            daemon,
            info,
            events,
            browse,
        })
    }

    /// Announce ourselves again and restart browsing, so a network that appeared after start
    /// (joined Wi-Fi, USB tethering) is covered right away instead of at the next query backoff.
    fn refresh(&mut self) {
        self.browse.abort();
        let _ = self.daemon.stop_browse(SERVICE_TYPE);
        if let Err(e) = self.daemon.register(self.info.clone()) {
            warn!(error = %e, "mDNS re-announce failed");
        }
        match self.daemon.browse(SERVICE_TYPE) {
            Ok(rx) => self.browse = spawn_browse(rx, self.events.clone()),
            Err(e) => warn!(error = %e, "mDNS re-browse failed"),
        }
    }
}

struct Running {
    endpoint: QuicEndpoint,
    mdns: Option<Mdns>,
    accept: AbortHandle,
}

pub struct LanTransport {
    config: LanConfig,
    running: Mutex<Option<Running>>,
}

impl LanTransport {
    pub fn new(config: LanConfig) -> Arc<Self> {
        Arc::new(Self {
            config,
            running: Mutex::new(None),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Option<Running>> {
        self.running.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The bound UDP address, once started.
    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.lock()
            .as_ref()
            .and_then(|r| r.endpoint.local_addr().ok())
    }
}

#[async_trait]
impl Transport for LanTransport {
    fn kind(&self) -> TransportKind {
        TransportKind::Lan
    }

    async fn start(
        &self,
        advert: Advertisement,
        events: mpsc::Sender<TransportEvent>,
    ) -> Result<()> {
        let endpoint = QuicEndpoint::bind(self.config.port)?;
        let port = endpoint.local_addr()?.port();
        info!(port, "LAN transport listening");

        let accept = tokio::spawn(accept_loop(endpoint.clone(), events.clone())).abort_handle();
        let mdns = if self.config.mdns {
            Mdns::start(&advert, port, events)
                .inspect_err(|e| warn!(error = %e, "mDNS unavailable; LAN discovery disabled"))
                .ok()
        } else {
            None
        };
        *self.lock() = Some(Running {
            endpoint,
            mdns,
            accept,
        });
        Ok(())
    }

    fn listen_addresses(&self) -> Vec<String> {
        let Some(port) = self.local_addr().map(|a| a.port()) else {
            return Vec::new();
        };
        let mut found: Vec<(u8, String)> = if_addrs::get_if_addrs()
            .unwrap_or_default()
            .into_iter()
            .filter(|i| !i.is_loopback())
            .filter_map(|i| {
                let IpAddr::V4(ip) = i.ip() else { return None };
                let rank = interface_rank(&i.name)?;
                (!ip.is_link_local())
                    .then(|| (rank, SocketAddr::new(IpAddr::V4(ip), port).to_string()))
            })
            .collect();
        found.sort();
        found.into_iter().map(|(_, a)| a).collect()
    }

    async fn connect(&self, candidate: &PeerCandidate) -> Result<BoxDuplex> {
        let addr: SocketAddr = candidate.address.parse().map_err(|_| {
            CoreError::Transport(format!("bad LAN address {:?}", candidate.address))
        })?;
        let endpoint = self.lock().as_ref().map(|r| r.endpoint.clone());
        let endpoint =
            endpoint.ok_or_else(|| CoreError::Transport("LAN transport not started".into()))?;
        Ok(Box::new(endpoint.connect(addr).await?))
    }

    async fn network_changed(&self) {
        if let Some(mdns) = self.lock().as_mut().and_then(|r| r.mdns.as_mut()) {
            debug!("network changed; refreshing mDNS");
            mdns.refresh();
        }
    }

    async fn stop(&self) {
        let Some(running) = self.lock().take() else {
            return;
        };
        running.accept.abort();
        if let Some(mdns) = running.mdns {
            mdns.browse.abort();
            // Shutting down unregisters the service, which sends a goodbye so peers drop us
            // promptly.
            let _ = mdns.daemon.shutdown();
        }
        running.endpoint.close();
    }
}

async fn accept_loop(endpoint: QuicEndpoint, events: mpsc::Sender<TransportEvent>) {
    loop {
        let Some(incoming) = endpoint.accept().await else {
            return;
        };
        let events = events.clone();
        tokio::spawn(async move {
            match quic::accept_stream(incoming).await {
                Ok(stream) => {
                    let _ = events
                        .send(TransportEvent::Incoming {
                            stream: Box::new(stream),
                            transport: TransportKind::Lan,
                        })
                        .await;
                }
                Err(e) => debug!(error = %e, "inbound QUIC connection failed"),
            }
        });
    }
}

fn spawn_browse(
    browse: mdns_sd::Receiver<ServiceEvent>,
    events: mpsc::Sender<TransportEvent>,
) -> AbortHandle {
    tokio::spawn(async move {
        // fullname -> candidates we reported for it, so removals can be mapped back.
        let mut known: HashMap<String, Vec<PeerCandidate>> = HashMap::new();
        while let Ok(event) = browse.recv_async().await {
            match event {
                ServiceEvent::ServiceResolved(svc) => {
                    let Some(device_id) = svc
                        .get_property_val_str("id")
                        .and_then(|s| s.parse::<DeviceId>().ok())
                    else {
                        continue;
                    };
                    let name = svc.get_property_val_str("name").map(str::to_owned);
                    let mut addrs: Vec<IpAddr> =
                        svc.get_addresses().iter().map(|a| a.to_ip_addr()).collect();
                    addrs.retain(usable);
                    addrs.sort_by_key(IpAddr::is_ipv6); // prefer IPv4
                    let list = known.entry(svc.fullname.clone()).or_default();
                    for ip in addrs {
                        let c = PeerCandidate {
                            transport: TransportKind::Lan,
                            address: SocketAddr::new(ip, svc.port).to_string(),
                            device_id: Some(device_id),
                            name: name.clone(),
                        };
                        if !list.contains(&c) {
                            list.push(c.clone());
                        }
                        // Re-sent on every resolve: "seen again" is how the node notices a
                        // device back on this network (and moves off the relay).
                        if events.send(TransportEvent::Discovered(c)).await.is_err() {
                            return;
                        }
                    }
                }
                ServiceEvent::ServiceRemoved(_, fullname) => {
                    for c in known.remove(&fullname).unwrap_or_default() {
                        let _ = events.send(TransportEvent::Lost(c)).await;
                    }
                }
                _ => {}
            }
        }
    })
    .abort_handle()
}

/// Order interfaces for pairing QR codes: real LAN links first, VPN tunnels last, and container
/// or VM bridges not at all (peers can never reach them).
fn interface_rank(name: &str) -> Option<u8> {
    const SKIP: [&str; 5] = ["docker", "br-", "veth", "virbr", "lxc"];
    const VPN: [&str; 6] = ["tun", "tap", "wg", "tailscale", "zt", "CloudflareWARP"];
    if SKIP.iter().any(|p| name.starts_with(p)) {
        None
    } else if VPN.iter().any(|p| name.starts_with(p)) {
        Some(1)
    } else {
        Some(0)
    }
}

/// Skip addresses we can't dial without extra scope information.
fn usable(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => !v4.is_unspecified() && !v4.is_link_local(),
        IpAddr::V6(v6) => !v6.is_unspecified() && !is_unicast_link_local(v6),
    }
}

fn is_unicast_link_local(ip: &Ipv6Addr) -> bool {
    (ip.segments()[0] & 0xffc0) == 0xfe80
}
