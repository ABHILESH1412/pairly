//! Tell the node when this PC's network changes (another Wi-Fi, cable in or out), so links over
//! the old network are checked at once instead of after their keepalive runs out. Watches
//! NetworkManager's primary connection; without NetworkManager, nothing happens.

use futures_util::StreamExt;
use pairly_core::PairlyNode;
use tracing::{debug, info};

pub async fn watch(node: PairlyNode) {
    if let Err(e) = run(&node).await {
        debug!(error = %e, "not watching the network (no NetworkManager?)");
    }
}

async fn run(node: &PairlyNode) -> zbus::Result<()> {
    let conn = zbus::Connection::system().await?;
    let nm = zbus::Proxy::new(
        &conn,
        "org.freedesktop.NetworkManager",
        "/org/freedesktop/NetworkManager",
        "org.freedesktop.NetworkManager",
    )
    .await?;
    let mut changes = nm
        .receive_property_changed::<zbus::zvariant::OwnedObjectPath>("PrimaryConnection")
        .await;
    // The first value is the current one, not a change.
    let mut current = nm
        .get_property::<zbus::zvariant::OwnedObjectPath>("PrimaryConnection")
        .await
        .ok();
    while let Some(change) = changes.next().await {
        let Ok(primary) = change.get().await else {
            continue;
        };
        if current.as_ref() == Some(&primary) {
            continue;
        }
        current = Some(primary);
        info!("the network changed: checking links");
        node.network_changed().await;
    }
    Ok(())
}
