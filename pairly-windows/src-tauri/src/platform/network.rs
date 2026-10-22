//! When this PC's network changes (another Wi-Fi, a cable in or out), so the node checks its
//! links at once instead of after their keepalive runs out.

use std::sync::OnceLock;

use tokio::sync::watch;

/// A counter that goes up on every network change Windows reports.
pub fn changes() -> watch::Receiver<u64> {
    static CHANGES: OnceLock<watch::Sender<u64>> = OnceLock::new();
    CHANGES
        .get_or_init(|| {
            let (tx, _) = watch::channel(0);
            #[cfg(windows)]
            register(tx.clone());
            tx
        })
        .subscribe()
}

#[cfg(windows)]
fn register(tx: watch::Sender<u64>) {
    use windows::Networking::Connectivity::{NetworkInformation, NetworkStatusChangedEventHandler};
    let handler = NetworkStatusChangedEventHandler::new(move |_| {
        tx.send_modify(|n| *n += 1);
        Ok(())
    });
    // Registered for the app's lifetime.
    if let Err(e) = NetworkInformation::NetworkStatusChanged(&handler) {
        tracing::debug!(error = %e, "can't watch the network");
    }
}
