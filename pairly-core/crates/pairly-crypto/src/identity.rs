use std::fmt;
use std::str::FromStr;

use blake2::{Blake2s256, Digest};
use data_encoding::BASE32_NOPAD;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use x25519_dalek::StaticSecret;
use zeroize::Zeroizing;

use crate::CryptoError;

pub const KEY_LEN: usize = 32;
const DEVICE_ID_LEN: usize = 16;

/// A device's long-term X25519 public key (the Noise static key).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PublicKey([u8; KEY_LEN]);

impl PublicKey {
    pub const fn from_bytes(bytes: [u8; KEY_LEN]) -> Self {
        Self(bytes)
    }

    pub fn from_slice(bytes: &[u8]) -> Result<Self, CryptoError> {
        bytes
            .try_into()
            .map(Self)
            .map_err(|_| CryptoError::InvalidKey("public key must be 32 bytes"))
    }

    pub const fn as_bytes(&self) -> &[u8; KEY_LEN] {
        &self.0
    }

    pub fn device_id(&self) -> DeviceId {
        let digest = Blake2s256::new_with_prefix(b"pairly-device-id")
            .chain_update(self.0)
            .finalize();
        let mut id = [0u8; DEVICE_ID_LEN];
        id.copy_from_slice(&digest[..DEVICE_ID_LEN]);
        DeviceId(id)
    }
}

impl fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PublicKey({})", self.device_id())
    }
}

/// Stable device identifier: a hash of the static public key, shown as 26 base32 characters.
/// Because it is derived from the authenticated Noise key, a peer cannot claim someone else's id.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DeviceId([u8; DEVICE_ID_LEN]);

impl DeviceId {
    pub const fn as_bytes(&self) -> &[u8; DEVICE_ID_LEN] {
        &self.0
    }
}

impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&BASE32_NOPAD.encode(&self.0).to_ascii_lowercase())
    }
}

impl fmt::Debug for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DeviceId({self})")
    }
}

impl FromStr for DeviceId {
    type Err = CryptoError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bytes = BASE32_NOPAD
            .decode(s.to_ascii_uppercase().as_bytes())
            .map_err(|_| CryptoError::InvalidDeviceId)?;
        bytes
            .try_into()
            .map(Self)
            .map_err(|_| CryptoError::InvalidDeviceId)
    }
}

impl Serialize for DeviceId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for DeviceId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// The device's long-term static key pair. The secret is zeroized on drop.
#[derive(Clone)]
pub struct IdentityKeypair {
    secret: Zeroizing<[u8; KEY_LEN]>,
    public: PublicKey,
}

impl IdentityKeypair {
    pub fn generate() -> Self {
        Self::from_secret(rand::random())
    }

    pub fn from_secret(secret: [u8; KEY_LEN]) -> Self {
        let secret = Zeroizing::new(secret);
        let public = x25519_dalek::PublicKey::from(&StaticSecret::from(*secret));
        Self {
            secret,
            public: PublicKey(public.to_bytes()),
        }
    }

    pub fn public(&self) -> PublicKey {
        self.public
    }

    pub fn device_id(&self) -> DeviceId {
        self.public.device_id()
    }

    pub fn secret_bytes(&self) -> &[u8; KEY_LEN] {
        &self.secret
    }
}

impl fmt::Debug for IdentityKeypair {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IdentityKeypair")
            .field("public", &self.public)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_id_is_stable_and_parses() {
        let kp = IdentityKeypair::from_secret([1; 32]);
        let id = kp.device_id();
        assert_eq!(id, IdentityKeypair::from_secret([1; 32]).device_id());
        let text = id.to_string();
        assert_eq!(text.len(), 26);
        assert_eq!(text.parse::<DeviceId>().unwrap(), id);
        assert_eq!(text.to_uppercase().parse::<DeviceId>().unwrap(), id);
        assert!("not-an-id".parse::<DeviceId>().is_err());
    }

    #[test]
    fn generated_keys_differ_and_debug_hides_secret() {
        let a = IdentityKeypair::generate();
        let b = IdentityKeypair::generate();
        assert_ne!(a.public(), b.public());
        assert!(!format!("{a:?}").contains(&format!("{:?}", a.secret_bytes())));
    }
}
