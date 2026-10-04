//! The byte-stream transport abstraction. Transports know nothing about crypto or packets.

use std::collections::HashMap;
use std::fmt;

use async_trait::async_trait;
use pairly_crypto::DeviceId;
use pairly_proto::packets::DeviceType;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;

use crate::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransportKind {
    /// In-process, for tests.
    Memory,
    Lan,
    Bluetooth,
    Relay,
}

impl TransportKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Memory => "mem",
            Self::Lan => "lan",
            Self::Bluetooth => "bt",
            Self::Relay => "relay",
        }
    }

    /// How much we prefer this kind of link (higher is better). A session moves to a
    /// higher-ranked link when one appears, and never to a lower one while it is up.
    pub fn rank(self) -> u8 {
        match self {
            Self::Memory | Self::Lan => 100,
            Self::Bluetooth => 60,
            Self::Relay => 30,
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "mem" => Some(Self::Memory),
            "lan" => Some(Self::Lan),
            "bt" => Some(Self::Bluetooth),
            "relay" => Some(Self::Relay),
            _ => None,
        }
    }
}

/// Pairly's RFCOMM service class UUID (the same on every platform).
pub const BLUETOOTH_SERVICE_UUID: &str = "9666e1eb-cdfa-4e10-aab2-648e9e26ac5d";

/// A reliable, ordered byte stream.
pub trait Duplex: AsyncRead + AsyncWrite + Send + Unpin + 'static {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin + 'static> Duplex for T {}

pub type BoxDuplex = Box<dyn Duplex>;

/// What a transport advertises about this device (mDNS TXT, BT service record, ...).
#[derive(Debug, Clone)]
pub struct Advertisement {
    pub device_id: DeviceId,
    pub name: String,
    pub device_type: DeviceType,
}

/// A way to reach a (possibly) known device. `device_id` is only a hint until the Noise
/// handshake authenticates the peer.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PeerCandidate {
    pub transport: TransportKind,
    pub address: String,
    pub device_id: Option<DeviceId>,
    pub name: Option<String>,
}

/// A paired device, as transports that need to know about pairings (the relay) see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairedPeer {
    pub id: DeviceId,
    /// Secret meeting point derived from the pairing (see `pairly_crypto::sas::rendezvous`).
    pub rendezvous: [u8; pairly_crypto::sas::RENDEZVOUS_LEN],
    /// Relays to meet it on: ours and the one it announced.
    pub relays: Vec<String>,
    /// The link the session currently uses, if connected.
    pub link: Option<TransportKind>,
    /// Its Bluetooth address, if known.
    pub bluetooth: Option<String>,
}

pub enum TransportEvent {
    Discovered(PeerCandidate),
    Lost(PeerCandidate),
    /// An inbound connection; the node runs the handshake on it.
    Incoming {
        stream: BoxDuplex,
        transport: TransportKind,
        /// Where it came from, if the transport knows something worth remembering (the
        /// Bluetooth address of a phone that dialed us).
        remote: Option<String>,
    },
}

impl fmt::Debug for TransportEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Discovered(c) => f.debug_tuple("Discovered").field(c).finish(),
            Self::Lost(c) => f.debug_tuple("Lost").field(c).finish(),
            Self::Incoming { transport, .. } => f
                .debug_struct("Incoming")
                .field("transport", transport)
                .finish_non_exhaustive(),
        }
    }
}

#[async_trait]
pub trait Transport: Send + Sync + 'static {
    fn kind(&self) -> TransportKind;
    /// Start advertising and discovering. Events go to `events` until [`Transport::stop`].
    async fn start(
        &self,
        advert: Advertisement,
        events: mpsc::Sender<TransportEvent>,
    ) -> Result<()>;
    /// Addresses peers can dial us on, best first (put in pairing QR codes).
    fn listen_addresses(&self) -> Vec<String> {
        Vec::new()
    }
    /// Open a stream to a candidate.
    async fn connect(&self, candidate: &PeerCandidate) -> Result<BoxDuplex>;
    /// The OS reported a network change (interface up/down, new address). Re-run discovery so
    /// peers on the new network are found promptly.
    async fn network_changed(&self) {}
    /// The set of paired devices, or how they are connected, changed.
    fn peers_changed(&self, _peers: &[PairedPeer]) {}
    async fn stop(&self);
}

/// For transports without discovery (Bluetooth): turns "these paired devices have these
/// addresses" into `Discovered`/`Lost` events for what changed since last time.
#[derive(Debug)]
pub struct KnownAddresses {
    kind: TransportKind,
    offered: HashMap<String, DeviceId>,
}

impl KnownAddresses {
    pub fn new(kind: TransportKind) -> Self {
        Self {
            kind,
            offered: HashMap::new(),
        }
    }

    pub fn update(
        &mut self,
        now: impl IntoIterator<Item = (String, DeviceId)>,
    ) -> Vec<TransportEvent> {
        let now: HashMap<String, DeviceId> = now.into_iter().collect();
        let candidate = |address: &str, id: DeviceId| PeerCandidate {
            transport: self.kind,
            address: address.to_owned(),
            device_id: Some(id),
            name: None,
        };
        let mut events: Vec<TransportEvent> = self
            .offered
            .iter()
            .filter(|(a, id)| now.get(*a) != Some(id))
            .map(|(a, id)| TransportEvent::Lost(candidate(a, *id)))
            .collect();
        events.extend(
            now.iter()
                .filter(|(a, id)| self.offered.get(*a) != Some(id))
                .map(|(a, id)| TransportEvent::Discovered(candidate(a, *id))),
        );
        self.offered = now;
        events
    }

    /// Everything offered so far, as `Lost` events (the transport is going away).
    pub fn clear(&mut self) -> Vec<TransportEvent> {
        self.update([])
    }
}

#[cfg(test)]
mod tests {
    use pairly_crypto::IdentityKeypair;

    use super::*;

    fn summary(events: &[TransportEvent]) -> Vec<(bool, String)> {
        let mut out: Vec<(bool, String)> = events
            .iter()
            .map(|e| match e {
                TransportEvent::Discovered(c) => (true, c.address.clone()),
                TransportEvent::Lost(c) => (false, c.address.clone()),
                TransportEvent::Incoming { .. } => unreachable!(),
            })
            .collect();
        out.sort();
        out
    }

    #[test]
    fn known_addresses_report_only_changes() {
        let (a, b) = (
            IdentityKeypair::generate().device_id(),
            IdentityKeypair::generate().device_id(),
        );
        let mut known = KnownAddresses::new(TransportKind::Bluetooth);
        let ev = known.update([("AA".to_owned(), a)]);
        assert_eq!(summary(&ev), vec![(true, "AA".to_owned())]);
        assert!(known.update([("AA".to_owned(), a)]).is_empty());
        let ev = known.update([("AA".to_owned(), a), ("BB".to_owned(), b)]);
        assert_eq!(summary(&ev), vec![(true, "BB".to_owned())]);
        let ev = known.clear();
        assert_eq!(
            summary(&ev),
            vec![(false, "AA".to_owned()), (false, "BB".to_owned())]
        );
    }
}
