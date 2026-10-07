//! The `io.github.abhilesh1412.Pairly.Daemon1` D-Bus interface: shared types and the client proxy used by
//! `pairly-gtk` and the `pairly` CLI. `pairlyd` implements the server side.
//!
//! D-Bus has no optional type, so "absent" is an empty string or `0`.
#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use zbus::proxy;
use zbus::zvariant::Type;

/// Default well-known bus name owned by `pairlyd`. Override it to run a second instance.
pub const BUS_NAME: &str = "io.github.abhilesh1412.Pairly.Daemon";
/// Object path of the daemon interface.
pub const OBJECT_PATH: &str = "/io/github/abhilesh1412/Pairly/Daemon";
pub const INTERFACE: &str = "io.github.abhilesh1412.Pairly.Daemon1";

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
    /// Paused: paired, but nothing passes either way until resumed.
    pub paused: bool,
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

/// A media player on a paired device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct Player {
    pub id: String,
    pub name: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub playing: bool,
    /// Milliseconds, or 0 if unknown.
    pub length_ms: u64,
    /// Milliseconds when the state was reported, or 0.
    pub position_ms: u64,
    pub can_play: bool,
    pub can_pause: bool,
    pub can_next: bool,
    pub can_previous: bool,
    pub can_seek: bool,
    /// 0–100, or -1 if the player has no volume.
    pub volume: i32,
    /// A local `file://` (or `https://`) URL of the artwork, or empty.
    pub art_url: String,
}

/// A text conversation on a phone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct Conversation {
    pub thread_id: i64,
    pub addresses: Vec<String>,
    /// Contact names, parallel to `addresses` (empty if unknown).
    pub names: Vec<String>,
    pub snippet: String,
    pub date_ms: i64,
    pub read: bool,
}

/// A picture, video or other file in a picture message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct MessageAttachment {
    pub part_id: i64,
    pub mime: String,
    pub name: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct TextMessage {
    pub id: i64,
    pub thread_id: i64,
    pub address: String,
    pub body: String,
    pub date_ms: i64,
    pub outgoing: bool,
    /// Everyone in a group message (empty for one-to-one).
    pub participants: Vec<String>,
    pub attachments: Vec<MessageAttachment>,
}

/// A file or folder on a phone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct FileEntry {
    pub name: String,
    pub dir: bool,
    pub size: u64,
    pub modified_ms: i64,
}

/// A contact on a phone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct Contact {
    pub name: String,
    pub numbers: Vec<String>,
}

/// An app on this PC that has posted notifications, and whether they go to paired devices.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct NotificationApp {
    /// The name the app gives its notifications, e.g. "Screenshot".
    pub name: String,
    pub send: bool,
    /// Unix seconds (to the hour) of its latest notification.
    pub last_seen: u64,
}

/// A command paired devices may run on this PC.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct Command {
    pub id: String,
    pub name: String,
    pub command: String,
}

#[proxy(
    interface = "io.github.abhilesh1412.Pairly.Daemon1",
    default_service = "io.github.abhilesh1412.Pairly.Daemon",
    default_path = "/io/github/abhilesh1412/Pairly/Daemon"
)]
pub trait Daemon {
    /// This device's `(id, name)`.
    fn get_identity(&self) -> zbus::Result<(String, String)>;
    /// Rename this PC (1-64 characters). The daemon restarts to announce the new name.
    fn set_name(&self, name: &str) -> zbus::Result<()>;
    /// Stop the daemon (every device disconnects).
    fn quit(&self) -> zbus::Result<()>;
    /// Pause a paired device (no connection either way, refused at the handshake) or resume it.
    fn set_paused(&self, id: &str, paused: bool) -> zbus::Result<()>;
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
    /// The media players on a device.
    fn list_players(&self, id: &str) -> zbus::Result<Vec<Player>>;
    /// A phone's text conversations, newest first.
    fn list_conversations(&self, id: &str) -> zbus::Result<Vec<Conversation>>;
    /// Messages in a conversation, oldest first; `before_ms` 0 for the newest page.
    fn list_messages(
        &self,
        id: &str,
        thread_id: i64,
        before_ms: i64,
    ) -> zbus::Result<Vec<TextMessage>>;
    /// Send a text through the phone (an MMS to several addresses).
    fn send_sms(&self, id: &str, addresses: &[&str], text: &str) -> zbus::Result<()>;
    /// Send a picture message: text plus local files.
    fn send_mms(
        &self,
        id: &str,
        addresses: &[&str],
        text: &str,
        files: &[&str],
    ) -> zbus::Result<()>;
    /// Fetch a picture message's attachment into a local cache file; returns its path.
    fn sms_attachment(&self, id: &str, part_id: i64, name: &str) -> zbus::Result<String>;
    /// A folder on a phone (`path` relative to its storage, `""` for the top).
    fn files_list(&self, id: &str, path: &str) -> zbus::Result<Vec<FileEntry>>;
    /// Copy a phone's file into the download folder; returns the local path. Progress comes
    /// as `FilesProgress`.
    fn files_download(&self, id: &str, path: &str, size: u64) -> zbus::Result<String>;
    /// Copy a local file into a phone folder.
    fn files_upload(&self, id: &str, local_path: &str, remote_dir: &str) -> zbus::Result<()>;
    fn files_delete(&self, id: &str, path: &str) -> zbus::Result<()>;
    fn files_mkdir(&self, id: &str, path: &str) -> zbus::Result<()>;
    fn files_rename(&self, id: &str, from: &str, to: &str) -> zbus::Result<()>;
    /// Open a window with the phone's screen, to watch and control it (same Wi-Fi only).
    fn show_phone_screen(&self, id: &str) -> zbus::Result<()>;
    /// Lock the phone's screen, or power it off or restart it: `lock`, `poweroff`, `restart`.
    fn phone_power(&self, id: &str, action: &str) -> zbus::Result<()>;
    /// A phone's contacts, sorted by name (also saved as a vCard file).
    fn list_contacts(&self, id: &str) -> zbus::Result<Vec<Contact>>;
    /// Have the phone call `number`.
    fn dial(&self, id: &str, number: &str) -> zbus::Result<()>;
    /// Act on the phone's current call: `answer`, `speaker`, `reject` or `hangup`.
    fn call_action(&self, id: &str, action: &str) -> zbus::Result<()>;
    /// Apps on this PC that have posted notifications.
    fn list_notification_apps(&self) -> zbus::Result<Vec<NotificationApp>>;
    /// Send (or stop sending) an app's notifications to paired devices.
    fn set_notification_app_send(&self, app: &str, send: bool) -> zbus::Result<()>;
    /// Commands paired devices may run on this PC.
    fn list_commands(&self) -> zbus::Result<Vec<Command>>;
    /// Returns the new command's id.
    fn add_command(&self, name: &str, command: &str) -> zbus::Result<String>;
    fn remove_command(&self, id: &str) -> zbus::Result<()>;
    /// Control a device's player. `action`: `play`, `pause`, `play_pause`, `stop`, `next`,
    /// `previous`, `seek` (`value` = ms, relative), `set_position` (ms), `set_volume` (0–100).
    fn media_control(&self, id: &str, player: &str, action: &str, value: i64) -> zbus::Result<()>;

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
    /// A download or upload moved on.
    #[zbus(signal)]
    fn files_progress(&self, id: &str, path: &str, done: u64, total: u64) -> zbus::Result<()>;
    /// A text arrived on (or was sent from) a phone.
    #[zbus(signal)]
    fn sms_received(&self, id: &str, thread_id: i64) -> zbus::Result<()>;
    /// A device's media players changed.
    #[zbus(signal)]
    fn players_changed(&self, id: &str) -> zbus::Result<()>;
    /// A transfer started, progressed (about 4 times a second) or finished.
    #[zbus(signal)]
    fn transfer_changed(&self, transfer: Transfer) -> zbus::Result<()>;
}
