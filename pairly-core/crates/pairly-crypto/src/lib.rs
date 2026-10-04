//! Pairly cryptography: long-term device identity, Noise handshakes, the SAS pairing
//! primitives and key storage.
//!
//! This crate is sans-IO: [`Handshake`] produces and consumes handshake messages, and the
//! caller (`pairly-core`) moves them over a transport.
#![forbid(unsafe_code)]

mod error;
mod handshake;
mod identity;
mod keystore;
pub mod sas;

pub use error::CryptoError;
pub use handshake::{
    Handshake, HandshakeInfo, HandshakeKind, Initiate, PSK_LEN, SessionCipher, TAG_LEN,
};
pub use identity::{DeviceId, IdentityKeypair, KEY_LEN, PublicKey};
pub use keystore::{FileKeyStore, KeyStore, MemoryKeyStore, load_or_generate};
