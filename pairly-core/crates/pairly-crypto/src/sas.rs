//! Short Authentication String (SAS) pairing, after Bluetooth's numeric comparison.
//!
//! A bare 6-digit code derived from the Noise handshake hash could be brute-forced by an
//! active MITM (it controls ephemerals on both legs and can grind ~10^6 tries offline). The
//! commit/reveal exchange below removes that freedom:
//!
//! 1. responder → initiator: `commitment(h, n_r)`
//! 2. initiator → responder: `n_i`
//! 3. responder → initiator: `n_r` (initiator checks the commitment)
//!
//! The responder is bound to `n_r` before it sees `n_i`, and the initiator reveals `n_i`
//! before it sees `n_r`, so a MITM gets one 1-in-10^6 guess per pairing attempt.
//!
//! Nonces travel inside the encrypted channel, so [`pair_secret`] is unknown to passive
//! observers; the handshake hash alone is not secret.

use std::fmt;

use blake2::{Blake2s256, Digest};

pub const NONCE_LEN: usize = 32;

pub fn random_nonce() -> [u8; NONCE_LEN] {
    rand::random()
}

fn hash(label: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut h = Blake2s256::new_with_prefix(label);
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

pub fn commitment(handshake_hash: &[u8; 32], responder_nonce: &[u8; NONCE_LEN]) -> [u8; 32] {
    hash(b"pairly-sas-commit", &[handshake_hash, responder_nonce])
}

pub fn verify_commitment(
    commitment_value: &[u8; 32],
    handshake_hash: &[u8; 32],
    responder_nonce: &[u8; NONCE_LEN],
) -> bool {
    // Not secret material; a plain comparison is fine.
    commitment(handshake_hash, responder_nonce) == *commitment_value
}

/// The 6-digit code both users compare.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct SasCode(u32);

impl SasCode {
    pub fn value(self) -> u32 {
        self.0
    }
}

impl fmt::Display for SasCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:03} {:03}", self.0 / 1000, self.0 % 1000)
    }
}

impl fmt::Debug for SasCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SasCode({self})")
    }
}

pub fn sas_code(
    handshake_hash: &[u8; 32],
    initiator_nonce: &[u8; NONCE_LEN],
    responder_nonce: &[u8; NONCE_LEN],
) -> SasCode {
    let d = hash(
        b"pairly-sas-code",
        &[handshake_hash, initiator_nonce, responder_nonce],
    );
    let n = u64::from_be_bytes(d[..8].try_into().expect("8 bytes"));
    SasCode(u32::try_from(n % 1_000_000).expect("below 10^6"))
}

/// Long-term secret shared by a paired couple (used later to derive the relay room).
pub fn pair_secret(
    handshake_hash: &[u8; 32],
    initiator_nonce: &[u8; NONCE_LEN],
    responder_nonce: &[u8; NONCE_LEN],
) -> [u8; 32] {
    hash(
        b"pairly-pair-secret",
        &[handshake_hash, initiator_nonce, responder_nonce],
    )
}

/// Length of the one-time secret carried in a pairing QR code.
pub const QR_SECRET_LEN: usize = 16;

/// The Noise PSK for QR pairing (`XXpsk3`), derived from the QR's one-time secret.
pub fn qr_psk(secret: &[u8; QR_SECRET_LEN]) -> [u8; 32] {
    hash(b"pairly-qr-psk", &[secret])
}

/// Length of a relay rendezvous id.
pub const RENDEZVOUS_LEN: usize = 16;

/// Where two paired devices meet on a relay: derived from their pairing secret, so only the two
/// of them can compute it and the relay learns nothing about who they are.
pub fn rendezvous(pair_secret: &[u8; 32]) -> [u8; RENDEZVOUS_LEN] {
    let full = hash(b"pairly-relay-room", &[pair_secret]);
    let mut out = [0; RENDEZVOUS_LEN];
    out.copy_from_slice(&full[..RENDEZVOUS_LEN]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commitment_binds_nonce_and_hash() {
        let h = [1; 32];
        let n = random_nonce();
        let c = commitment(&h, &n);
        assert!(verify_commitment(&c, &h, &n));
        assert!(!verify_commitment(&c, &h, &random_nonce()));
        assert!(!verify_commitment(&c, &[2; 32], &n));
    }

    #[test]
    fn sas_depends_on_every_input_and_formats() {
        let (h, ni, nr) = ([1; 32], [2; 32], [3; 32]);
        let code = sas_code(&h, &ni, &nr);
        assert!(code.value() < 1_000_000);
        assert_eq!(code.to_string().len(), 7);
        assert_ne!(code, sas_code(&h, &nr, &ni));
        assert_ne!(code, sas_code(&[9; 32], &ni, &nr));
        assert_ne!(pair_secret(&h, &ni, &nr), pair_secret(&h, &nr, &ni));
        assert_eq!(SasCode(42).to_string(), "000 042");
    }
}
