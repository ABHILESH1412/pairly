//! Pairly wire protocol: the inner packet envelope, packet bodies, CBOR codec and the
//! length-prefixed frame format used on every transport.
//!
//! Layering (see `plan.md` §5): a transport carries **frames** (`u16` length + bytes). After the
//! Noise handshake every frame is one encrypted Noise message; the decrypted payloads are
//! reassembled into a CBOR-encoded [`Envelope`].
#![forbid(unsafe_code)]

mod envelope;
mod error;
pub mod frame;
pub mod packets;

pub use envelope::{Envelope, MAX_PACKET_SIZE, MAX_TYPE_LEN, PacketBody};
pub use error::ProtoError;

/// Protocol version carried in every envelope.
pub const PROTOCOL_VERSION: u8 = 1;
