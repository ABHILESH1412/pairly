use thiserror::Error;

#[derive(Debug, Error)]
pub enum CryptoError {
    #[error("noise: {0}")]
    Noise(#[from] snow::Error),
    #[error("handshake payloads must be empty")]
    UnexpectedPayload,
    #[error("handshake finished without a remote static key")]
    MissingRemoteKey,
    #[error("invalid key: {0}")]
    InvalidKey(&'static str),
    #[error("invalid device id")]
    InvalidDeviceId,
    #[error("unknown handshake kind {0}")]
    UnknownHandshakeKind(u8),
    #[error("key storage: {0}")]
    KeyStore(String),
}
