use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProtoError {
    #[error("packet of {0} bytes exceeds the {max} byte limit", max = crate::MAX_PACKET_SIZE)]
    TooLarge(usize),
    #[error("unsupported protocol version {0}")]
    UnsupportedVersion(u8),
    #[error("invalid packet type {0:?}")]
    InvalidType(String),
    #[error("expected packet type {expected:?}, got {actual:?}")]
    WrongType {
        expected: &'static str,
        actual: String,
    },
    #[error("invalid {field}: {reason}")]
    Invalid {
        field: &'static str,
        reason: &'static str,
    },
    #[error("CBOR encode error: {0}")]
    Encode(String),
    #[error("CBOR decode error: {0}")]
    Decode(String),
}
