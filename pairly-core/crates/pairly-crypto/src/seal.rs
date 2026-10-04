//! Encrypting individual secrets at rest (the registry's pairing secrets and relay addresses).
//!
//! The key is derived from the device's identity secret, which each platform already keeps in
//! its secure store (the Android Keystore, the Secret Service on Linux), so these fields are
//! exactly as protected as the identity, with no second key to store.
//!
//! Format: `[version = 1][12-byte random nonce][ChaCha20-Poly1305 ciphertext + tag]`. The caller
//! passes associated data naming the field and its row, so a sealed value can't be moved to
//! another field or device.

use blake2::{Blake2s256, Digest};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use zeroize::Zeroizing;

use crate::{CryptoError, IdentityKeypair};

const VERSION: u8 = 1;
const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;

pub struct FieldKey(Zeroizing<[u8; 32]>);

impl FieldKey {
    /// The key for `purpose` (e.g. `"registry"`), derived from the identity secret.
    pub fn derive(identity: &IdentityKeypair, purpose: &str) -> Self {
        let mut h = Blake2s256::new_with_prefix(b"pairly-field-key");
        h.update(purpose.as_bytes());
        h.update([0]);
        h.update(identity.secret_bytes());
        Self(Zeroizing::new(h.finalize().into()))
    }

    fn cipher(&self) -> ChaCha20Poly1305 {
        ChaCha20Poly1305::new(Key::from_slice(&self.0[..]))
    }

    pub fn seal(&self, aad: &[u8], plaintext: &[u8]) -> Vec<u8> {
        let nonce: [u8; NONCE_LEN] = rand::random();
        let sealed = self
            .cipher()
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: plaintext,
                    aad,
                },
            )
            .expect("ChaCha20-Poly1305 encryption of an in-memory buffer can't fail");
        let mut out = Vec::with_capacity(1 + NONCE_LEN + sealed.len());
        out.push(VERSION);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&sealed);
        out
    }

    pub fn open(&self, aad: &[u8], sealed: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let bad = || CryptoError::KeyStore("a stored secret is corrupt or from another key".into());
        if sealed.len() < 1 + NONCE_LEN + TAG_LEN || sealed[0] != VERSION {
            return Err(bad());
        }
        let (nonce, ciphertext) = sealed[1..].split_at(NONCE_LEN);
        self.cipher()
            .decrypt(
                Nonce::from_slice(nonce),
                Payload {
                    msg: ciphertext,
                    aad,
                },
            )
            .map_err(|_| bad())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_roundtrip_and_binding() {
        let id = IdentityKeypair::generate();
        let key = FieldKey::derive(&id, "registry");
        let sealed = key.seal(b"pair_secret:abc", &[7; 32]);
        assert_eq!(key.open(b"pair_secret:abc", &sealed).unwrap(), [7; 32]);
        // Another field, row, purpose or identity can't open it.
        assert!(key.open(b"pair_secret:xyz", &sealed).is_err());
        assert!(
            FieldKey::derive(&id, "other")
                .open(b"pair_secret:abc", &sealed)
                .is_err()
        );
        let stranger = FieldKey::derive(&IdentityKeypair::generate(), "registry");
        assert!(stranger.open(b"pair_secret:abc", &sealed).is_err());
        // Random nonces: sealing twice differs.
        assert_ne!(sealed, key.seal(b"pair_secret:abc", &[7; 32]));
        let mut tampered = sealed.clone();
        tampered[20] ^= 1;
        assert!(key.open(b"pair_secret:abc", &tampered).is_err());
    }
}
