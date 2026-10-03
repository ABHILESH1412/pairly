//! Pairly wire protocol: the inner packet envelope, packet bodies, CBOR codec and the
//! length-prefixed frame codec used on every transport.
#![forbid(unsafe_code)]

/// Protocol version carried in every envelope and in the identity packet.
pub const PROTOCOL_VERSION: u8 = 1;
