use snow::params::NoiseParams;
use snow::{Builder, HandshakeState, StatelessTransportState};

use crate::{CryptoError, IdentityKeypair, PublicKey};

pub const PSK_LEN: usize = 32;
pub const TAG_LEN: usize = 16;
const MAX_MESSAGE: usize = 65535;
const PROLOGUE_TAG: &[u8; 7] = b"pairly\0";
/// Bumped if handshake patterns or prologue semantics change.
const HANDSHAKE_VERSION: u8 = 1;

const NOISE_XX: &str = "Noise_XX_25519_ChaChaPoly_BLAKE2s";
const NOISE_XX_PSK3: &str = "Noise_XXpsk3_25519_ChaChaPoly_BLAKE2s";
const NOISE_IK: &str = "Noise_IK_25519_ChaChaPoly_BLAKE2s";

/// Which handshake a connection runs. Sent in clear before the handshake and bound into the
/// Noise prologue, so tampering with it makes the handshake fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum HandshakeKind {
    /// `XX`: first contact, authenticated afterwards by comparing the SAS code.
    Pair = 1,
    /// `XXpsk3`: first contact via QR code; the PSK from the QR authenticates both sides.
    PairPsk = 2,
    /// `IK`: reconnect to an already-paired device whose key is pinned.
    Reconnect = 3,
}

impl HandshakeKind {
    pub fn from_u8(v: u8) -> Result<Self, CryptoError> {
        match v {
            1 => Ok(Self::Pair),
            2 => Ok(Self::PairPsk),
            3 => Ok(Self::Reconnect),
            other => Err(CryptoError::UnknownHandshakeKind(other)),
        }
    }

    /// The two bytes the initiator sends before the first Noise message.
    pub fn hello(self) -> [u8; 2] {
        [HANDSHAKE_VERSION, self as u8]
    }

    /// Parse the initiator's hello.
    pub fn from_hello(hello: &[u8]) -> Result<Self, CryptoError> {
        match hello {
            [HANDSHAKE_VERSION, kind] => Self::from_u8(*kind),
            [_, kind] => Err(CryptoError::UnknownHandshakeKind(*kind)),
            _ => Err(CryptoError::UnknownHandshakeKind(0)),
        }
    }

    fn params(self) -> NoiseParams {
        let name = match self {
            Self::Pair => NOISE_XX,
            Self::PairPsk => NOISE_XX_PSK3,
            Self::Reconnect => NOISE_IK,
        };
        name.parse().expect("static noise params are valid")
    }

    fn prologue(self) -> [u8; 9] {
        let mut p = [0u8; 9];
        p[..7].copy_from_slice(PROLOGUE_TAG);
        p[7..].copy_from_slice(&self.hello());
        p
    }
}

/// Initiator-side parameters for each handshake kind.
#[derive(Debug, Clone, Copy)]
pub enum Initiate<'a> {
    Pair,
    PairPsk(&'a [u8; PSK_LEN]),
    Reconnect(&'a PublicKey),
}

impl Initiate<'_> {
    pub fn kind(&self) -> HandshakeKind {
        match self {
            Self::Pair => HandshakeKind::Pair,
            Self::PairPsk(_) => HandshakeKind::PairPsk,
            Self::Reconnect(_) => HandshakeKind::Reconnect,
        }
    }
}

/// An in-progress Noise handshake. Payloads are always empty: identity and capabilities are
/// exchanged as normal packets once the channel is encrypted.
pub struct Handshake {
    state: HandshakeState,
    kind: HandshakeKind,
}

impl Handshake {
    pub fn initiator(how: Initiate<'_>, local: &IdentityKeypair) -> Result<Self, CryptoError> {
        let kind = how.kind();
        let prologue = kind.prologue();
        let mut b = Builder::new(kind.params())
            .local_private_key(local.secret_bytes())?
            .prologue(&prologue)?;
        match how {
            Initiate::Pair => {}
            Initiate::PairPsk(psk) => b = b.psk(3, psk)?,
            Initiate::Reconnect(remote) => b = b.remote_public_key(remote.as_bytes())?,
        }
        Ok(Self {
            state: b.build_initiator()?,
            kind,
        })
    }

    /// `psk` is required for [`HandshakeKind::PairPsk`] and ignored otherwise.
    pub fn responder(
        kind: HandshakeKind,
        local: &IdentityKeypair,
        psk: Option<&[u8; PSK_LEN]>,
    ) -> Result<Self, CryptoError> {
        let prologue = kind.prologue();
        let mut b = Builder::new(kind.params())
            .local_private_key(local.secret_bytes())?
            .prologue(&prologue)?;
        if kind == HandshakeKind::PairPsk {
            let psk = psk.ok_or(CryptoError::InvalidKey("PairPsk responder needs a PSK"))?;
            b = b.psk(3, psk)?;
        }
        Ok(Self {
            state: b.build_responder()?,
            kind,
        })
    }

    pub fn kind(&self) -> HandshakeKind {
        self.kind
    }

    pub fn is_finished(&self) -> bool {
        self.state.is_handshake_finished()
    }

    pub fn is_my_turn(&self) -> bool {
        self.state.is_my_turn()
    }

    /// The peer's static key, once the handshake has revealed it. For an `IK` responder this
    /// is available right after the first message, so unknown peers can be rejected early.
    pub fn remote_static(&self) -> Option<PublicKey> {
        self.state
            .get_remote_static()
            .and_then(|k| PublicKey::from_slice(k).ok())
    }

    pub fn write_message(&mut self) -> Result<Vec<u8>, CryptoError> {
        let mut buf = vec![0u8; MAX_MESSAGE];
        let n = self.state.write_message(&[], &mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }

    pub fn read_message(&mut self, message: &[u8]) -> Result<(), CryptoError> {
        let mut payload = vec![0u8; MAX_MESSAGE];
        let n = self.state.read_message(message, &mut payload)?;
        if n != 0 {
            return Err(CryptoError::UnexpectedPayload);
        }
        Ok(())
    }

    pub fn finish(self) -> Result<(SessionCipher, HandshakeInfo), CryptoError> {
        let remote = self.remote_static().ok_or(CryptoError::MissingRemoteKey)?;
        let mut hash = [0u8; 32];
        hash.copy_from_slice(self.state.get_handshake_hash());
        let info = HandshakeInfo {
            kind: self.kind,
            remote,
            hash,
        };
        let cipher = SessionCipher {
            state: self.state.into_stateless_transport_mode()?,
        };
        Ok((cipher, info))
    }
}

/// What a completed handshake established.
#[derive(Debug, Clone)]
pub struct HandshakeInfo {
    pub kind: HandshakeKind,
    /// The peer's authenticated static key.
    pub remote: PublicKey,
    /// Noise handshake hash. Unique per session but **not secret**; use only for binding.
    pub hash: [u8; 32],
}

/// Transport-phase cipher. Nonces are explicit, so a channel's reader and writer can share one
/// instance (`&self`) while each keeps its own counter.
pub struct SessionCipher {
    state: StatelessTransportState,
}

impl SessionCipher {
    /// Largest plaintext that fits in one Noise message.
    pub const MAX_PLAINTEXT: usize = MAX_MESSAGE - TAG_LEN;

    pub fn encrypt(&self, nonce: u64, plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let mut out = vec![0u8; plaintext.len() + TAG_LEN];
        let n = self.state.write_message(nonce, plaintext, &mut out)?;
        out.truncate(n);
        Ok(out)
    }

    pub fn decrypt(&self, nonce: u64, ciphertext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let mut out = vec![0u8; ciphertext.len()];
        let n = self.state.read_message(nonce, ciphertext, &mut out)?;
        out.truncate(n);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run a handshake to completion in memory.
    fn run(
        mut i: Handshake,
        mut r: Handshake,
    ) -> Result<[(SessionCipher, HandshakeInfo); 2], CryptoError> {
        loop {
            let (from, to) = if i.is_my_turn() {
                (&mut i, &mut r)
            } else {
                (&mut r, &mut i)
            };
            let msg = from.write_message()?;
            to.read_message(&msg)?;
            if i.is_finished() && r.is_finished() {
                return Ok([i.finish()?, r.finish()?]);
            }
        }
    }

    fn keys() -> (IdentityKeypair, IdentityKeypair) {
        (IdentityKeypair::generate(), IdentityKeypair::generate())
    }

    #[test]
    fn xx_establishes_matching_session() {
        let (a, b) = keys();
        let [(ca, ia), (cb, ib)] = run(
            Handshake::initiator(Initiate::Pair, &a).unwrap(),
            Handshake::responder(HandshakeKind::Pair, &b, None).unwrap(),
        )
        .unwrap();
        assert_eq!(ia.remote, b.public());
        assert_eq!(ib.remote, a.public());
        assert_eq!(ia.hash, ib.hash);
        let ct = ca.encrypt(0, b"hi").unwrap();
        assert_eq!(cb.decrypt(0, &ct).unwrap(), b"hi");
        let ct = cb.encrypt(0, b"back").unwrap();
        assert_eq!(ca.decrypt(0, &ct).unwrap(), b"back");
    }

    #[test]
    fn ik_with_pinned_key() {
        let (a, b) = keys();
        let mut r = Handshake::responder(HandshakeKind::Reconnect, &b, None).unwrap();
        let mut i = Handshake::initiator(Initiate::Reconnect(&b.public()), &a).unwrap();
        // The responder learns the initiator's key after the first message.
        r.read_message(&i.write_message().unwrap()).unwrap();
        assert_eq!(r.remote_static(), Some(a.public()));
        i.read_message(&r.write_message().unwrap()).unwrap();
        assert!(i.is_finished() && r.is_finished());
    }

    #[test]
    fn ik_to_impostor_fails() {
        let (a, b) = keys();
        let impostor = IdentityKeypair::generate();
        let res = run(
            Handshake::initiator(Initiate::Reconnect(&b.public()), &a).unwrap(),
            Handshake::responder(HandshakeKind::Reconnect, &impostor, None).unwrap(),
        );
        assert!(res.is_err());
    }

    #[test]
    fn psk_must_match() {
        let (a, b) = keys();
        let ok = run(
            Handshake::initiator(Initiate::PairPsk(&[5; 32]), &a).unwrap(),
            Handshake::responder(HandshakeKind::PairPsk, &b, Some(&[5; 32])).unwrap(),
        );
        assert!(ok.is_ok());
        let bad = run(
            Handshake::initiator(Initiate::PairPsk(&[5; 32]), &a).unwrap(),
            Handshake::responder(HandshakeKind::PairPsk, &b, Some(&[6; 32])).unwrap(),
        );
        assert!(bad.is_err());
    }

    #[test]
    fn kind_mismatch_fails() {
        // Simulates an attacker rewriting the hello: the prologues differ, so the handshake fails.
        let (a, b) = keys();
        let res = run(
            Handshake::initiator(Initiate::Pair, &a).unwrap(),
            Handshake::responder(HandshakeKind::PairPsk, &b, Some(&[0; 32])).unwrap(),
        );
        assert!(res.is_err());
    }

    #[test]
    fn tampered_or_replayed_ciphertext_fails() {
        let (a, b) = keys();
        let [(ca, _), (cb, _)] = run(
            Handshake::initiator(Initiate::Pair, &a).unwrap(),
            Handshake::responder(HandshakeKind::Pair, &b, None).unwrap(),
        )
        .unwrap();
        let mut ct = ca.encrypt(0, b"secret").unwrap();
        assert!(cb.decrypt(1, &ct).is_err(), "wrong nonce must fail");
        ct[0] ^= 1;
        assert!(cb.decrypt(0, &ct).is_err(), "bit flip must fail");
    }

    #[test]
    fn hello_roundtrip() {
        for k in [
            HandshakeKind::Pair,
            HandshakeKind::PairPsk,
            HandshakeKind::Reconnect,
        ] {
            assert_eq!(HandshakeKind::from_hello(&k.hello()).unwrap(), k);
        }
        assert!(HandshakeKind::from_hello(&[9, 1]).is_err());
        assert!(HandshakeKind::from_hello(&[1, 9]).is_err());
        assert!(HandshakeKind::from_hello(&[1]).is_err());
    }
}
