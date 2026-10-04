//! OS integration, implemented natively by each app (`pairlyd` on Linux, Kotlin via UniFFI on
//! Android). Plugins reach the OS only through this trait, which grows one method group per
//! plugin as features land. Every method has a no-op default so a platform can opt in.

use pairly_crypto::DeviceId;

/// Who a plugin event came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerInfo {
    pub id: DeviceId,
    pub name: String,
}

pub trait Platform: Send + Sync + 'static {
    /// The peer sent a ping (ping plugin).
    fn ping_received(&self, _from: &PeerInfo, _message: Option<&str>) {}
}

/// A platform without OS integration, for tests and headless tools.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullPlatform;

impl Platform for NullPlatform {}
