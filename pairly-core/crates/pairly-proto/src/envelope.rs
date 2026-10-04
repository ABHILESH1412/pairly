use ciborium::Value;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::{PROTOCOL_VERSION, ProtoError};

/// Upper bound for one encoded envelope. Bigger payloads (files) are chunked by the share plugin.
pub const MAX_PACKET_SIZE: usize = 1024 * 1024 + 4096;
/// Upper bound for the `ty` string.
pub const MAX_TYPE_LEN: usize = 64;
/// Nesting limit for CBOR decoding of untrusted input.
const RECURSION_LIMIT: usize = 32;

/// A typed packet body. `TYPE` is the wire `ty` string, e.g. `"notification.posted"`.
pub trait PacketBody: Serialize + DeserializeOwned {
    const TYPE: &'static str;
}

/// The inner (decrypted) packet.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    /// Protocol version.
    pub v: u8,
    /// Per-session monotonic id used for acks and dedup. `0` for pre-session packets
    /// (identity exchange, pairing).
    pub id: u64,
    /// Whether the sender wants this packet acknowledged (and will resend it until it is).
    pub ack: bool,
    /// Packet type, e.g. `"clipboard.set"`.
    pub ty: String,
    /// Type-specific payload.
    pub body: Value,
}

impl Envelope {
    pub fn new<T: PacketBody>(id: u64, ack: bool, body: &T) -> Result<Self, ProtoError> {
        Ok(Self {
            v: PROTOCOL_VERSION,
            id,
            ack,
            ty: T::TYPE.to_owned(),
            body: Value::serialized(body).map_err(|e| ProtoError::Encode(e.to_string()))?,
        })
    }

    /// Whether this envelope carries a `T`.
    pub fn is<T: PacketBody>(&self) -> bool {
        self.ty == T::TYPE
    }

    /// Decode the body as `T`, checking the type tag first.
    pub fn body<T: PacketBody>(&self) -> Result<T, ProtoError> {
        if !self.is::<T>() {
            return Err(ProtoError::WrongType {
                expected: T::TYPE,
                actual: self.ty.clone(),
            });
        }
        self.body
            .deserialized()
            .map_err(|e| ProtoError::Decode(e.to_string()))
    }

    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        let mut out = Vec::new();
        ciborium::into_writer(self, &mut out).map_err(|e| ProtoError::Encode(e.to_string()))?;
        if out.len() > MAX_PACKET_SIZE {
            return Err(ProtoError::TooLarge(out.len()));
        }
        Ok(out)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        if bytes.len() > MAX_PACKET_SIZE {
            return Err(ProtoError::TooLarge(bytes.len()));
        }
        let env: Self = ciborium::de::from_reader_with_recursion_limit(bytes, RECURSION_LIMIT)
            .map_err(|e| ProtoError::Decode(e.to_string()))?;
        if env.v != PROTOCOL_VERSION {
            return Err(ProtoError::UnsupportedVersion(env.v));
        }
        if !is_valid_type(&env.ty) {
            return Err(ProtoError::InvalidType(env.ty));
        }
        Ok(env)
    }
}

/// `ty` must be short, lowercase ASCII with `.`/`_` separators.
fn is_valid_type(ty: &str) -> bool {
    !ty.is_empty()
        && ty.len() <= MAX_TYPE_LEN
        && ty
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packets::{Ack, Keepalive};

    #[test]
    fn roundtrip() {
        let env = Envelope::new(7, true, &Ack { ids: vec![1, 2, 3] }).unwrap();
        let decoded = Envelope::decode(&env.encode().unwrap()).unwrap();
        assert_eq!(decoded, env);
        assert_eq!(decoded.body::<Ack>().unwrap().ids, vec![1, 2, 3]);
    }

    #[test]
    fn body_type_is_checked() {
        let env = Envelope::new(1, false, &Keepalive { t: 5 }).unwrap();
        assert!(matches!(
            env.body::<Ack>(),
            Err(ProtoError::WrongType { .. })
        ));
    }

    #[test]
    fn rejects_other_versions() {
        let mut env = Envelope::new(1, false, &Keepalive { t: 5 }).unwrap();
        env.v = 99;
        let bytes = env.encode().unwrap();
        assert!(matches!(
            Envelope::decode(&bytes),
            Err(ProtoError::UnsupportedVersion(99))
        ));
    }

    #[test]
    fn rejects_bad_types() {
        let mut env = Envelope::new(1, false, &Keepalive { t: 5 }).unwrap();
        env.ty = "Bad Type!".into();
        let bytes = env.encode().unwrap();
        assert!(matches!(
            Envelope::decode(&bytes),
            Err(ProtoError::InvalidType(_))
        ));
    }

    #[test]
    fn rejects_garbage_and_oversize() {
        assert!(Envelope::decode(&[0xff, 0x00, 0x13]).is_err());
        assert!(matches!(
            Envelope::decode(&vec![0u8; MAX_PACKET_SIZE + 1]),
            Err(ProtoError::TooLarge(_))
        ));
    }

    #[test]
    fn rejects_deep_nesting() {
        // 100 nested CBOR arrays of length 1.
        let mut bytes = vec![0x81; 100];
        bytes.push(0x00);
        assert!(Envelope::decode(&bytes).is_err());
    }
}
