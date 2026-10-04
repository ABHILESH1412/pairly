//! This PC's battery from UPower's display device (absent on desktops).

use std::sync::{Arc, Mutex, PoisonError};

use futures_util::StreamExt;
use pairly_plugins::battery::{BatteryPlugin, BatteryState};
use tracing::{info, warn};
use zbus::Connection;

const UPOWER: &str = "org.freedesktop.UPower";
const DISPLAY_DEVICE: &str = "/org/freedesktop/UPower/devices/DisplayDevice";
const DEVICE_IFACE: &str = "org.freedesktop.UPower.Device";

/// UPower `State`: 1 charging, 4 fully charged, 5 pending charge (plugged in).
fn on_power(state: u32) -> bool {
    matches!(state, 1 | 4 | 5)
}

async fn read(proxy: &zbus::Proxy<'_>) -> zbus::Result<Option<BatteryState>> {
    if !proxy.get_property::<bool>("IsPresent").await? {
        return Ok(None);
    }
    let percent: f64 = proxy.get_property("Percentage").await?;
    let state: u32 = proxy.get_property("State").await?;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // clamped to 0..=100
    let percent = percent.clamp(0.0, 100.0).round() as u8;
    Ok(Some(BatteryState {
        percent,
        charging: on_power(state),
    }))
}

pub async fn watch(plugin: Arc<BatteryPlugin>, cache: Arc<Mutex<Option<BatteryState>>>) {
    let result = async {
        let conn = Connection::system().await?;
        let proxy = zbus::Proxy::new(&conn, UPOWER, DISPLAY_DEVICE, DEVICE_IFACE).await?;
        let props = zbus::fdo::PropertiesProxy::builder(&conn)
            .destination(UPOWER)?
            .path(DISPLAY_DEVICE)?
            .build()
            .await?;
        let mut changes = props.receive_properties_changed().await?;
        let update = || async {
            let state = read(&proxy).await?;
            *cache.lock().unwrap_or_else(PoisonError::into_inner) = state;
            if let Some(state) = state {
                plugin.local_changed(state);
            }
            Ok::<_, zbus::Error>(state)
        };
        match update().await? {
            Some(s) => info!(percent = s.percent, charging = s.charging, "battery"),
            None => info!("no battery"),
        }
        while changes.next().await.is_some() {
            update().await?;
        }
        Ok::<_, zbus::Error>(())
    };
    if let Err(e) = result.await {
        warn!(error = %e, "can't read the battery from UPower");
    }
}
