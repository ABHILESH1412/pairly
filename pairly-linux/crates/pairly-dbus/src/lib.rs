//! The `dev.pairly.Daemon1` D-Bus interface: shared types and the client proxy used by
//! `pairly-gtk` and the `pairly` CLI. `pairlyd` implements the server side.
//!
//! D-Bus has no optional type, so "absent" is an empty string or `0`.
#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use zbus::proxy;
use zbus::zvariant::Type;

/// Default well-known bus name owned by `pairlyd`. Override it to run a second instance.
pub const BUS_NAME: &str = "dev.pairly.Daemon";
/// Object path of the daemon interface.
pub const OBJECT_PATH: &str = "/dev/pairly/Daemon";
pub const INTERFACE: &str = "dev.pairly.Daemon1";

/// A paired or discovered device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct Device {
    pub id: String,
    pub name: String,
    /// `desktop` | `laptop` | `phone` | `tablet`, or empty if not paired yet.
    pub device_type: String,
    pub paired: bool,
    /// Active transport (`lan`, `bluetooth`, `relay`), or empty when disconnected.
    pub link: String,
    /// Round-trip time in milliseconds, or 0 if unknown.
    pub rtt_ms: u32,
    /// Battery percentage, or -1 if unknown (offline, or no battery).
    pub battery: i32,
    pub charging: bool,
}

impl Device {
    pub fn is_connected(&self) -> bool {
        !self.link.is_empty()
    }
}

/// A file transfer in either direction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct Transfer {
    pub id: u64,
    pub device: String,
    pub device_name: String,
    pub incoming: bool,
    pub name: String,
    pub size: u64,
    /// Bytes the receiver has confirmed.
    pub bytes: u64,
    /// `waiting` (for an answer) | `running` | `done` | `failed` | `cancelled`.
    pub state: String,
    /// Why it failed, or empty.
    pub error: String,
    /// Where a received file was saved (once `done`), or empty.
    pub path: String,
}

impl Transfer {
    pub fn is_finished(&self) -> bool {
        matches!(self.state.as_str(), "done" | "failed" | "cancelled")
    }

    /// Progress from 0.0 to 1.0.
    pub fn fraction(&self) -> f64 {
        if self.size == 0 {
            return if self.state == "done" { 1.0 } else { 0.0 };
        }
        #[allow(clippy::cast_precision_loss)] // a progress bar
        let f = self.bytes as f64 / self.size as f64;
        f.clamp(0.0, 1.0)
    }
}

#[proxy(
    interface = "dev.pairly.Daemon1",
    default_service = "dev.pairly.Daemon",
    default_path = "/dev/pairly/Daemon"
)]
pub trait Daemon {
    /// This device's `(id, name)`.
    fn get_identity(&self) -> zbus::Result<(String, String)>;
    fn list_devices(&self) -> zbus::Result<Vec<Device>>;
    /// Start pairing; answer the resulting `PairingRequested` with `ConfirmPair`.
    fn request_pair(&self, id: &str) -> zbus::Result<()>;
    fn confirm_pair(&self, id: &str, accept: bool) -> zbus::Result<()>;
    /// Create a one-time pairing QR code: returns `(uri, valid_for_secs)`. Scanning it pairs
    /// without a code comparison; completion arrives as `PairingFinished`.
    fn start_qr_pairing(&self) -> zbus::Result<(String, u32)>;
    fn cancel_qr_pairing(&self) -> zbus::Result<()>;
    fn unpair(&self, id: &str) -> zbus::Result<()>;
    /// Reply through a notification mirrored from `device` (fallback when the notification
    /// server has no inline replies).
    fn reply_to_notification(
        &self,
        device: &str,
        notification: &str,
        text: &str,
    ) -> zbus::Result<()>;
    /// `message` may be empty.
    fn ping(&self, id: &str, message: &str) -> zbus::Result<()>;
    /// Make the device ring loudly (find my phone), or stop it.
    fn ring(&self, id: &str, on: bool) -> zbus::Result<()>;
    /// Send this PC's current clipboard text to the device.
    fn send_clipboard(&self, id: &str) -> zbus::Result<()>;
    /// Offer files (absolute paths) to the device. Returns one transfer id per file.
    fn send_files(&self, id: &str, paths: &[&str]) -> zbus::Result<Vec<u64>>;
    /// Send a link (opened on arrival) or text (copied on arrival).
    fn send_text(&self, id: &str, text: &str, url: bool) -> zbus::Result<()>;
    /// Accept a file offered with `state == "waiting"`.
    fn accept_transfer(&self, transfer: u64) -> zbus::Result<()>;
    /// Cancel a transfer, or decline an offer.
    fn cancel_transfer(&self, transfer: u64) -> zbus::Result<()>;
    /// Transfers that haven't finished.
    fn list_transfers(&self) -> zbus::Result<Vec<Transfer>>;

    /// A device appeared, disappeared, connected, disconnected, paired or unpaired.
    #[zbus(signal)]
    fn device_changed(&self, id: &str) -> zbus::Result<()>;
    /// Show `code` (e.g. `"123 456"`) and ask the user to confirm it matches the other screen.
    #[zbus(signal)]
    fn pairing_requested(
        &self,
        id: &str,
        name: &str,
        code: &str,
        incoming: bool,
    ) -> zbus::Result<()>;
    #[zbus(signal)]
    fn pairing_finished(&self, id: &str, success: bool, message: &str) -> zbus::Result<()>;
    #[zbus(signal)]
    fn ping_received(&self, id: &str, name: &str, message: &str) -> zbus::Result<()>;
    /// A transfer started, progressed (about 4 times a second) or finished.
    #[zbus(signal)]
    fn transfer_changed(&self, transfer: Transfer) -> zbus::Result<()>;
}
