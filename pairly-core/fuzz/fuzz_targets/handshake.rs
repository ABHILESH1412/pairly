//! The first handshake bytes from a stranger: the hello, then a Noise message to the responder
//! (any of the three kinds), and a reply to our own first message as initiator.
#![no_main]

use std::sync::OnceLock;

use libfuzzer_sys::fuzz_target;
use pairly_crypto::{Handshake, HandshakeKind, IdentityKeypair, Initiate};

fn keys() -> &'static (IdentityKeypair, IdentityKeypair) {
    static KEYS: OnceLock<(IdentityKeypair, IdentityKeypair)> = OnceLock::new();
    KEYS.get_or_init(|| {
        (
            IdentityKeypair::from_secret([1; 32]),
            IdentityKeypair::from_secret([2; 32]),
        )
    })
}

fuzz_target!(|data: &[u8]| {
    let _ = HandshakeKind::from_hello(data);
    let Some((&selector, message)) = data.split_first() else {
        return;
    };
    let (ours, theirs) = keys();
    let psk = [7u8; 32];
    match selector % 4 {
        0 => {
            let mut r = Handshake::responder(HandshakeKind::Pair, ours, None).unwrap();
            if r.read_message(message).is_ok() {
                let _ = r.write_message();
            }
        }
        1 => {
            let mut r = Handshake::responder(HandshakeKind::PairPsk, ours, Some(&psk)).unwrap();
            if r.read_message(message).is_ok() {
                let _ = r.write_message();
            }
        }
        2 => {
            let mut r = Handshake::responder(HandshakeKind::Reconnect, ours, None).unwrap();
            if r.read_message(message).is_ok() {
                let _ = r.remote_static();
                let _ = r.write_message();
            }
        }
        _ => {
            let their_key = theirs.public();
            let mut i = Handshake::initiator(Initiate::Reconnect(&their_key), ours).unwrap();
            let _ = i.write_message().unwrap();
            if i.read_message(message).is_ok() && i.is_finished() {
                let _ = i.finish();
            }
        }
    }
});
