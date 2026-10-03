//! Blind relay: pairs two QUIC connections that join the same opaque room and pipes
//! ciphertext between them. It never sees device identities or keys.
#![forbid(unsafe_code)]

fn main() {
    println!("pairly-relay {}", env!("CARGO_PKG_VERSION"));
}
