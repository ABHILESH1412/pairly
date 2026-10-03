//! Pairly node: encrypted channels, sessions with ack/dedup, device registry,
//! transport manager and plugin host. Shared verbatim by the Linux daemon and Android.
#![forbid(unsafe_code)]

pub use pairly_proto::PROTOCOL_VERSION;
