//! The byte-stream transport abstraction. Transports know nothing about crypto or packets.

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
}

pub enum TransportEvent {
    Discovered(PeerCandidate),
    Lost(PeerCandidate),
    /// An inbound connection; the node runs the handshake on it.
    Incoming {
        stream: BoxDuplex,
        transport: TransportKind,
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
