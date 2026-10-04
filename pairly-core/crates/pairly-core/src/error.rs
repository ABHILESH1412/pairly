use pairly_crypto::{CryptoError, DeviceId};
use pairly_proto::ProtoError;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("protocol: {0}")]
    Proto(#[from] ProtoError),
    #[error("crypto: {0}")]
    Crypto(#[from] CryptoError),
    #[error("registry: {0}")]
    Registry(#[from] rusqlite::Error),
    #[error("transport: {0}")]
    Transport(String),
    #[error("connection closed")]
    Closed,
    #[error("timed out")]
    Timeout,
    #[error("protocol violation: {0}")]
    Violation(&'static str),
    #[error("device {0} is not paired")]
    NotPaired(DeviceId),
    #[error("device {0} is not connected")]
    NotConnected(DeviceId),
    #[error("unknown device {0}")]
    UnknownDevice(DeviceId),
    #[error("pairing was rejected")]
    PairingRejected,
    #[error("pairing is disabled")]
    PairingDisabled,
    #[error("peer does not accept {0:?} packets")]
    Unsupported(String),
    #[error("too many packets are waiting to be sent to {0}")]
    Backlog(DeviceId),
    #[error("no such {0}")]
    NotFound(&'static str),
    #[error("node is shut down")]
    Shutdown,
}

pub type Result<T, E = CoreError> = std::result::Result<T, E>;
