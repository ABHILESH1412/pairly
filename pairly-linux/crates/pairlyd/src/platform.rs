//! The Linux side of the plugins: their host callbacks become [`PlatformEvent`]s for the main
//! event task (D-Bus signals, notifications), or act directly when that's all they need.

use std::sync::{Arc, Mutex, PoisonError};

use pairly_core::{DeviceId, PeerInfo, Platform};
use pairly_plugins::battery::{BatteryHost, BatteryState};
use pairly_plugins::clipboard::ClipboardHost;
use pairly_plugins::findmy::FindMyHost;
use tokio::sync::mpsc;

#[derive(Debug)]
pub enum PlatformEvent {
    Ping {
        from: DeviceId,
        name: String,
        message: Option<String>,
    },
    Battery {
        from: DeviceId,
        name: String,
        state: BatteryState,
        previous: Option<BatteryState>,
    },
    Ring {
        name: String,
        on: bool,
    },
}

pub struct LinuxPlatform {
    events: mpsc::UnboundedSender<PlatformEvent>,
    /// This PC's battery, kept current by [`crate::battery::watch`].
    pub battery: Arc<Mutex<Option<BatteryState>>>,
}

impl LinuxPlatform {
    pub fn new() -> (Arc<Self>, mpsc::UnboundedReceiver<PlatformEvent>) {
        let (events, rx) = mpsc::unbounded_channel();
        (
            Arc::new(Self {
                events,
                battery: Arc::default(),
            }),
            rx,
        )
    }
}

impl Platform for LinuxPlatform {
    fn ping_received(&self, from: &PeerInfo, message: Option<&str>) {
        let _ = self.events.send(PlatformEvent::Ping {
            from: from.id,
            name: from.name.clone(),
            message: message.map(str::to_owned),
        });
    }
}

impl ClipboardHost for LinuxPlatform {
    fn set_clipboard(&self, _from: &PeerInfo, text: &str) {
        crate::clipboard::set(text);
    }
}

impl BatteryHost for LinuxPlatform {
    fn current(&self) -> Option<BatteryState> {
        *self.battery.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn peer_changed(&self, from: &PeerInfo, state: BatteryState, previous: Option<BatteryState>) {
        let _ = self.events.send(PlatformEvent::Battery {
            from: from.id,
            name: from.name.clone(),
            state,
            previous,
        });
    }
}

impl FindMyHost for LinuxPlatform {
    fn ring(&self, from: &PeerInfo, on: bool) {
        let _ = self.events.send(PlatformEvent::Ring {
            name: from.name.clone(),
            on,
        });
    }
}
