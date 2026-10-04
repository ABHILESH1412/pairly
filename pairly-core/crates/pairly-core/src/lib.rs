//! Pairly node: encrypted channels, sessions with ack/dedup, device registry, pairing,
//! transport plumbing and the plugin host. Shared verbatim by the Linux daemon and Android.
//!
//! Layers, bottom to top (see `plan.md` §3):
//! [`transport`] (bytes) → [`channel`] (Noise) → [`session`] (acks, dedup, priorities)
//! → [`node`] (pairing, connection management) → [`plugin`]s.
#![forbid(unsafe_code)]

pub mod channel;
mod error;
pub mod memory;
pub mod node;
mod pairing;
pub mod platform;
pub mod plugin;
pub mod qr;
pub mod registry;
pub mod session;
pub mod transport;

pub use error::{CoreError, Result};
pub use node::{DeviceInfo, NodeBuilder, NodeConfig, NodeEvent, PairlyNode};
pub use pairly_crypto::sas::SasCode;
pub use pairly_crypto::{DeviceId, KeyStore, PublicKey};
pub use pairly_proto::packets::{DeviceType, Identity};
pub use pairly_proto::{Envelope, PROTOCOL_VERSION, PacketBody};
pub use platform::{NullPlatform, PeerInfo, Platform};
pub use plugin::{Plugin, PluginCtx};
pub use qr::QrInvite;
pub use registry::Registry;
pub use session::{OutboundPacket, Priority, SessionConfig};
pub use transport::{
    KnownAddresses, PairedPeer, PeerCandidate, Transport, TransportEvent, TransportKind,
};
