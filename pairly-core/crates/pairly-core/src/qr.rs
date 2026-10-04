//! Pairing QR codes. The PC shows `pairly://pair?v=1&pk=…&s=…&a=lan:192.168.0.10:47100`; the
//! phone scans it and runs Noise `XXpsk3` with a PSK derived from the one-time secret `s`.
//! The PSK proves the phone saw this QR, and checking the responder's key against `pk` proves
//! it reached the device that showed it, so no code comparison is needed (`plan.md` §6.2).

use data_encoding::BASE32_NOPAD;
use pairly_crypto::sas::{QR_SECRET_LEN, qr_psk};
use pairly_crypto::{DeviceId, PSK_LEN, PublicKey};

use crate::transport::TransportKind;
use crate::{CoreError, Result};

const PREFIX: &str = "pairly://pair?";
const VERSION: &str = "1";
const MAX_ADDRESSES: usize = 8;
const MAX_ADDRESS_LEN: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QrInvite {
    pub public_key: PublicKey,
    pub secret: [u8; QR_SECRET_LEN],
    /// Where to reach the inviting device, best first.
    pub addresses: Vec<(TransportKind, String)>,
}

fn b32(bytes: &[u8]) -> String {
    BASE32_NOPAD.encode(bytes).to_ascii_lowercase()
}

fn unb32(s: &str) -> Option<Vec<u8>> {
    BASE32_NOPAD.decode(s.to_ascii_uppercase().as_bytes()).ok()
}

impl QrInvite {
    pub fn device_id(&self) -> DeviceId {
        self.public_key.device_id()
    }

    pub fn psk(&self) -> [u8; PSK_LEN] {
        qr_psk(&self.secret)
    }

    pub fn to_uri(&self) -> String {
        let mut uri = format!(
            "{PREFIX}v={VERSION}&pk={}&s={}",
            b32(self.public_key.as_bytes()),
            b32(&self.secret)
        );
        for (kind, addr) in self.addresses.iter().take(MAX_ADDRESSES) {
            uri.push_str(&format!("&a={}:{addr}", kind.as_str()));
        }
        uri
    }

    pub fn parse(uri: &str) -> Result<Self> {
        let bad = |what: &'static str| CoreError::Violation(what);
        let query = uri
            .trim()
            .strip_prefix(PREFIX)
            .ok_or(bad("not a Pairly pairing code"))?;
        let (mut version, mut key, mut secret, mut addresses) = (None, None, None, Vec::new());
        for pair in query.split('&') {
            let (k, v) = pair.split_once('=').ok_or(bad("malformed pairing code"))?;
            match k {
                "v" => version = Some(v),
                "pk" => key = unb32(v),
                "s" => secret = unb32(v),
                "a" => {
                    let (kind, addr) = v.split_once(':').ok_or(bad("malformed address"))?;
                    if addresses.len() < MAX_ADDRESSES
                        && !addr.is_empty()
                        && addr.len() <= MAX_ADDRESS_LEN
                        && let Some(kind) = TransportKind::parse(kind)
                    {
                        addresses.push((kind, addr.to_owned()));
                    }
                }
                _ => {} // ignore unknown keys from newer versions
            }
        }
        if version != Some(VERSION) {
            return Err(bad("unsupported pairing code version"));
        }
        let public_key = PublicKey::from_slice(&key.ok_or(bad("pairing code has no key"))?)?;
        let secret = secret
            .and_then(|s| <[u8; QR_SECRET_LEN]>::try_from(s).ok())
            .ok_or(bad("pairing code has no valid secret"))?;
        Ok(Self {
            public_key,
            secret,
            addresses,
        })
    }
}

#[cfg(test)]
mod tests {
    use pairly_crypto::IdentityKeypair;

    use super::*;

    fn invite() -> QrInvite {
        QrInvite {
            public_key: IdentityKeypair::generate().public(),
            secret: [9; QR_SECRET_LEN],
            addresses: vec![
                (TransportKind::Lan, "192.168.0.10:47100".into()),
                (TransportKind::Lan, "[2001:db8::1]:47100".into()),
            ],
        }
    }

    #[test]
    fn roundtrip() {
        let inv = invite();
        let uri = inv.to_uri();
        assert!(uri.starts_with("pairly://pair?v=1&pk="));
        assert!(uri.len() < 300, "keeps the QR small: {} chars", uri.len());
        assert_eq!(QrInvite::parse(&uri).unwrap(), inv);
        assert_ne!(inv.psk(), [0; 32]);
    }

    #[test]
    fn rejects_garbage() {
        let uri = invite().to_uri();
        for bad in [
            "https://example.com",
            "pairly://pair?v=2&pk=aa&s=bb",
            &uri.replace("&s=", "&x="),
            &uri.replace("pk=", "pk=zz"),
            "pairly://pair?nonsense",
        ] {
            assert!(QrInvite::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn ignores_unknown_transports_and_keys() {
        let uri = format!("{}&a=carrier-pigeon:coop&future=1", invite().to_uri());
        assert_eq!(QrInvite::parse(&uri).unwrap().addresses.len(), 2);
    }
}
