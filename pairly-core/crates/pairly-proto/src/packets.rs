//! Control and pairing packet bodies. Feature packets live with their plugins.

use serde::{Deserialize, Serialize};

use crate::{PacketBody, ProtoError};

/// Acknowledges packets that were sent with `ack: true`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ack {
    pub ids: Vec<u64>,
}
impl PacketBody for Ack {
    const TYPE: &'static str = "ack";
}

/// Liveness probe; the peer answers with [`KeepaliveAck`] echoing `t` (used for RTT).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Keepalive {
    pub t: u64,
}
impl PacketBody for Keepalive {
    const TYPE: &'static str = "keepalive";
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeepaliveAck {
    pub t: u64,
}
impl PacketBody for KeepaliveAck {
    const TYPE: &'static str = "keepalive.ack";
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeviceType {
    Desktop,
    Laptop,
    Phone,
    Tablet,
}

impl DeviceType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Desktop => "desktop",
            Self::Laptop => "laptop",
            Self::Phone => "phone",
            Self::Tablet => "tablet",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "desktop" => Some(Self::Desktop),
            "laptop" => Some(Self::Laptop),
            "phone" => Some(Self::Phone),
            "tablet" => Some(Self::Tablet),
            _ => None,
        }
    }
}

/// First packet in each direction on every new channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub name: String,
    pub device_type: DeviceType,
    pub app_version: String,
    /// Random per process run. A change tells the peer we restarted, so it resets its dedup state.
    pub session_nonce: u64,
    /// Packet types we can handle.
    pub incoming: Vec<String>,
    /// Packet types we may send.
    pub outgoing: Vec<String>,
    /// The relay this device uses (`pairly-relay://…`), so a paired peer can meet it there.
    /// Sent only inside the encrypted channel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay: Option<String>,
}
impl PacketBody for Identity {
    const TYPE: &'static str = "identity";
}

impl Identity {
    pub const MAX_NAME_CHARS: usize = 64;
    pub const MAX_TYPES: usize = 256;
    pub const MAX_RELAY_LEN: usize = 512;

    pub fn validate(&self) -> Result<(), ProtoError> {
        let invalid = |field, reason| Err(ProtoError::Invalid { field, reason });
        if self.name.trim().is_empty() || self.name.chars().count() > Self::MAX_NAME_CHARS {
            return invalid("name", "must be 1-64 characters");
        }
        if self.name.chars().any(char::is_control) {
            return invalid("name", "must not contain control characters");
        }
        if self.app_version.len() > 32 {
            return invalid("app_version", "too long");
        }
        if self.incoming.len() > Self::MAX_TYPES || self.outgoing.len() > Self::MAX_TYPES {
            return invalid("capabilities", "too many packet types");
        }
        if self
            .incoming
            .iter()
            .chain(&self.outgoing)
            .any(|t| t.len() > crate::MAX_TYPE_LEN)
        {
            return invalid("capabilities", "packet type too long");
        }
        if self
            .relay
            .as_ref()
            .is_some_and(|r| r.len() > Self::MAX_RELAY_LEN || r.chars().any(char::is_control))
        {
            return invalid("relay", "too long or malformed");
        }
        Ok(())
    }
}

/// SAS pairing step 1 (responder → initiator): commitment to the responder's nonce.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairCommit {
    #[serde(with = "serde_bytes")]
    pub commitment: [u8; 32],
}
impl PacketBody for PairCommit {
    const TYPE: &'static str = "pair.commit";
}

/// SAS pairing step 2 (initiator → responder): the initiator's nonce.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairNonce {
    #[serde(with = "serde_bytes")]
    pub nonce: [u8; 32],
}
impl PacketBody for PairNonce {
    const TYPE: &'static str = "pair.nonce";
}

/// SAS pairing step 3 (responder → initiator): opens the commitment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairReveal {
    #[serde(with = "serde_bytes")]
    pub nonce: [u8; 32],
}
impl PacketBody for PairReveal {
    const TYPE: &'static str = "pair.reveal";
}

/// The user's decision after comparing codes. Both sides must accept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairConfirm {
    pub accept: bool,
}
impl PacketBody for PairConfirm {
    const TYPE: &'static str = "pair.confirm";
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Envelope;

    fn identity() -> Identity {
        Identity {
            name: "Laptop".into(),
            device_type: DeviceType::Laptop,
            app_version: "0.1.0".into(),
            session_nonce: 42,
            incoming: vec!["ping".into()],
            outgoing: vec!["ping".into()],
            relay: Some("pairly-relay://relay.example:47200/abc".into()),
        }
    }

    #[test]
    fn identity_roundtrip_and_validation() {
        let id = identity();
        id.validate().unwrap();
        let env = Envelope::new(0, false, &id).unwrap();
        let back: Identity = Envelope::decode(&env.encode().unwrap())
            .unwrap()
            .body()
            .unwrap();
        assert_eq!(back, id);

        let mut bad = identity();
        bad.name = "x".repeat(65);
        assert!(bad.validate().is_err());
        bad.name = "evil\nname".into();
        assert!(bad.validate().is_err());
        bad.name = "   ".into();
        assert!(bad.validate().is_err());
    }

    #[test]
    fn byte_arrays_encode_as_cbor_bytes() {
        let env = Envelope::new(0, false, &PairNonce { nonce: [7; 32] }).unwrap();
        let encoded = env.encode().unwrap();
        // 0x58 0x20 = CBOR byte string of length 32 (an array would be 0x98 0x20).
        assert!(encoded.windows(2).any(|w| w == [0x58, 0x20]));
        let back: PairNonce = Envelope::decode(&encoded).unwrap().body().unwrap();
        assert_eq!(back.nonce, [7; 32]);
    }
}
