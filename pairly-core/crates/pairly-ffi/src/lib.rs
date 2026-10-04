//! UniFFI bindings exposing the Pairly node to Kotlin. Built as `libpairly_ffi.so` for Android
//! by `pairly-android/scripts/build-rust.sh`; Kotlin lives in package `dev.pairly.core.ffi`.
//!
//! The surface is deliberately coarse: one [`Node`] object, plain records/enums, and a few
//! foreign-implemented traits ([`EventListener`], [`SecretStore`] and one per feature area).
//! All async work runs on a private Tokio runtime, so Kotlin can call from any coroutine
//! dispatcher.

mod bluetooth;

use std::fs::File;
use std::os::fd::{FromRawFd, OwnedFd};
use std::path::PathBuf;
use std::sync::{Arc, Once, OnceLock};

use pairly_core::{
    CoreError, DeviceId, DeviceType, NodeConfig, NodeEvent, PairlyNode, PeerInfo, Platform,
    Registry, TransportKind,
};
use pairly_crypto::{CryptoError, IdentityKeypair, KEY_LEN, KeyStore};
use pairly_plugins::battery::{BatteryHost, BatteryPlugin, BatteryState};
use pairly_plugins::clipboard::{ClipboardHost, ClipboardPlugin};
use pairly_plugins::findmy::{FindMyHost, FindMyPlugin};
use pairly_plugins::notification::{
    Icon, Notification, NotificationAction, NotificationHost, NotificationPlugin,
};
use pairly_plugins::ping::PingPlugin;
use pairly_plugins::share::{ShareHost, SharePlugin, Transfer, TransferState};
use pairly_transport_lan::{LanConfig, LanTransport};
use pairly_transport_relay::{RelayConfig, RelayTransport};
use tokio::runtime::Runtime;
use tokio::sync::broadcast;
use tokio::task::AbortHandle;

use bluetooth::ForeignBluetooth;
pub use bluetooth::{BluetoothHandler, BluetoothSocket, bluetooth_service_uuid};

uniffi::setup_scaffolding!();

pub(crate) fn runtime() -> &'static Runtime {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("pairly")
            .enable_all()
            .build()
            .expect("failed to start the Tokio runtime")
    })
}

fn init_logging() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        use tracing_subscriber::EnvFilter;
        use tracing_subscriber::prelude::*;
        let filter = EnvFilter::new("info,pairly_core=debug");
        #[cfg(target_os = "android")]
        let _ = tracing_subscriber::registry()
            .with(filter)
            .with(paranoid_android::layer("pairly").with_ansi(false))
            .try_init();
        #[cfg(not(target_os = "android"))]
        let _ = tracing_subscriber::registry()
            .with(filter)
            .with(tracing_subscriber::fmt::layer())
            .try_init();
    });
}

// ----- errors ------------------------------------------------------------------------------

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum PairlyError {
    #[error("{reason}")]
    Failed { reason: String },
    #[error("not a device id: {id}")]
    InvalidDevice { id: String },
}

impl From<CoreError> for PairlyError {
    fn from(e: CoreError) -> Self {
        Self::Failed {
            reason: e.to_string(),
        }
    }
}

impl From<uniffi::UnexpectedUniFFICallbackError> for PairlyError {
    fn from(e: uniffi::UnexpectedUniFFICallbackError) -> Self {
        Self::Failed { reason: e.reason }
    }
}

fn failed(e: impl std::fmt::Display) -> PairlyError {
    PairlyError::Failed {
        reason: e.to_string(),
    }
}

fn parse_id(id: &str) -> Result<DeviceId, PairlyError> {
    id.parse()
        .map_err(|_| PairlyError::InvalidDevice { id: id.to_owned() })
}

// ----- records and enums -------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum DeviceKind {
    Desktop,
    Laptop,
    Phone,
    Tablet,
}

impl From<DeviceKind> for DeviceType {
    fn from(k: DeviceKind) -> Self {
        match k {
            DeviceKind::Desktop => Self::Desktop,
            DeviceKind::Laptop => Self::Laptop,
            DeviceKind::Phone => Self::Phone,
            DeviceKind::Tablet => Self::Tablet,
        }
    }
}

impl From<DeviceType> for DeviceKind {
    fn from(t: DeviceType) -> Self {
        match t {
            DeviceType::Desktop => Self::Desktop,
            DeviceType::Laptop => Self::Laptop,
            DeviceType::Phone => Self::Phone,
            DeviceType::Tablet => Self::Tablet,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum Link {
    Lan,
    Bluetooth,
    Relay,
}

impl From<TransportKind> for Link {
    fn from(k: TransportKind) -> Self {
        match k {
            // The in-memory transport only exists in tests.
            TransportKind::Lan | TransportKind::Memory => Self::Lan,
            TransportKind::Bluetooth => Self::Bluetooth,
            TransportKind::Relay => Self::Relay,
        }
    }
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct NodeOptions {
    pub name: String,
    pub kind: DeviceKind,
    /// App-private directory for the registry database.
    pub data_dir: String,
    /// UDP port for the LAN transport (a random one is used if it is taken).
    pub lan_port: u16,
    /// A relay of our own (`pairly-relay://…`). Usually `None`: the phone uses the relay its
    /// paired PC announces.
    pub relay: Option<String>,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct Device {
    pub id: String,
    pub name: String,
    /// Known once paired.
    pub kind: Option<DeviceKind>,
    pub paired: bool,
    /// The active link, if connected.
    pub link: Option<Link>,
    pub rtt_ms: Option<u32>,
    /// Known while connected, for devices with a battery.
    pub battery: Option<BatteryData>,
}

#[derive(Debug, Clone, uniffi::Enum)]
pub enum Event {
    DeviceDiscovered {
        id: String,
        name: Option<String>,
    },
    DeviceLost {
        id: String,
    },
    /// Show `code` and ask the user to confirm it matches the other screen.
    PairingRequested {
        id: String,
        name: String,
        code: String,
        incoming: bool,
    },
    Paired {
        id: String,
        name: String,
    },
    PairingFailed {
        id: String,
        reason: String,
    },
    Connected {
        id: String,
        link: Link,
    },
    Disconnected {
        id: String,
        reason: String,
    },
    Unpaired {
        id: String,
    },
    PingReceived {
        id: String,
        name: String,
        message: Option<String>,
    },
}

impl From<NodeEvent> for Event {
    fn from(e: NodeEvent) -> Self {
        match e {
            NodeEvent::DeviceDiscovered { id, name, .. } => Self::DeviceDiscovered {
                id: id.to_string(),
                name,
            },
            NodeEvent::DeviceLost { id } => Self::DeviceLost { id: id.to_string() },
            NodeEvent::PairingRequested {
                id,
                name,
                code,
                incoming,
            } => Self::PairingRequested {
                id: id.to_string(),
                name,
                code: code.to_string(),
                incoming,
            },
            NodeEvent::Paired { id, name } => Self::Paired {
                id: id.to_string(),
                name,
            },
            NodeEvent::PairingFailed { id, reason } => Self::PairingFailed {
                id: id.to_string(),
                reason,
            },
            NodeEvent::Connected { id, transport } => Self::Connected {
                id: id.to_string(),
                link: transport.into(),
            },
            NodeEvent::Disconnected { id, reason } => Self::Disconnected {
                id: id.to_string(),
                reason,
            },
            NodeEvent::Unpaired { id } => Self::Unpaired { id: id.to_string() },
        }
    }
}

// ----- foreign-implemented traits -----------------------------------------------------------

/// Receives every node event. Called from Rust worker threads: hand off quickly.
#[uniffi::export(with_foreign)]
pub trait EventListener: Send + Sync {
    fn on_event(&self, event: Event);
}

/// Persists the 32-byte identity secret. On Android this is encrypted with a Keystore key.
#[uniffi::export(with_foreign)]
pub trait SecretStore: Send + Sync {
    fn load(&self) -> Result<Option<Vec<u8>>, PairlyError>;
    fn store(&self, secret: Vec<u8>) -> Result<(), PairlyError>;
}

struct ForeignKeyStore(Arc<dyn SecretStore>);

impl KeyStore for ForeignKeyStore {
    fn load(&self) -> Result<Option<IdentityKeypair>, CryptoError> {
        let Some(bytes) = self
            .0
            .load()
            .map_err(|e| CryptoError::KeyStore(e.to_string()))?
        else {
            return Ok(None);
        };
        let secret: [u8; KEY_LEN] = bytes
            .as_slice()
            .try_into()
            .map_err(|_| CryptoError::KeyStore("stored identity has the wrong length".into()))?;
        Ok(Some(IdentityKeypair::from_secret(secret)))
    }

    fn store(&self, keypair: &IdentityKeypair) -> Result<(), CryptoError> {
        self.0
            .store(keypair.secret_bytes().to_vec())
            .map_err(|e| CryptoError::KeyStore(e.to_string()))
    }
}

/// Plugin callbacks become [`Event`]s; Kotlin decides how to surface them.
struct FfiPlatform {
    listener: Arc<dyn EventListener>,
}

impl Platform for FfiPlatform {
    fn ping_received(&self, from: &PeerInfo, message: Option<&str>) {
        self.listener.on_event(Event::PingReceived {
            id: from.id.to_string(),
            name: from.name.clone(),
            message: message.map(str::to_owned),
        });
    }
}

// ----- notifications -----------------------------------------------------------------------

#[derive(Debug, Clone, uniffi::Record)]
pub struct NotificationActionData {
    pub key: String,
    pub label: String,
}

/// Raw RGBA pixels (`width * height * 4` bytes, at most 128x128).
#[derive(Debug, Clone, uniffi::Record)]
pub struct IconData {
    pub width: u16,
    pub height: u16,
    pub rgba: Vec<u8>,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct NotificationData {
    /// Stable per notification on its source device; updates reuse it.
    pub id: String,
    pub app: String,
    pub title: String,
    pub text: String,
    pub time_ms: u64,
    pub actions: Vec<NotificationActionData>,
    pub can_reply: bool,
    pub icon: Option<IconData>,
    pub silent: bool,
}

impl From<NotificationData> for Notification {
    fn from(n: NotificationData) -> Self {
        Self {
            id: n.id,
            app: n.app,
            title: n.title,
            text: n.text,
            time: n.time_ms,
            actions: n
                .actions
                .into_iter()
                .map(|a| NotificationAction {
                    key: a.key,
                    label: a.label,
                })
                .collect(),
            can_reply: n.can_reply,
            icon: n.icon.map(|i| Icon {
                width: i.width,
                height: i.height,
                rgba: i.rgba,
            }),
            silent: n.silent,
        }
    }
}

impl From<&Notification> for NotificationData {
    fn from(n: &Notification) -> Self {
        Self {
            id: n.id.clone(),
            app: n.app.clone(),
            title: n.title.clone(),
            text: n.text.clone(),
            time_ms: n.time,
            actions: n
                .actions
                .iter()
                .map(|a| NotificationActionData {
                    key: a.key.clone(),
                    label: a.label.clone(),
                })
                .collect(),
            can_reply: n.can_reply,
            icon: n.icon.as_ref().map(|i| IconData {
                width: i.width,
                height: i.height,
                rgba: i.rgba.clone(),
            }),
            silent: n.silent,
        }
    }
}

/// Implemented in Kotlin: shows mirrors from other devices and acts on the phone's own
/// notifications when a paired device asks. Called on Rust threads: hand off quickly.
#[uniffi::export(with_foreign)]
pub trait NotificationHandler: Send + Sync {
    fn show(&self, from_id: String, from_name: String, notification: NotificationData);
    fn remove(&self, from_id: String, id: String);
    /// `from_id` has exactly these notifications showing: drop other mirrors from it.
    fn sync(&self, from_id: String, active: Vec<String>);
    fn dismiss_local(&self, id: String);
    fn action_local(&self, id: String, action: String);
    fn reply_local(&self, id: String, text: String);
}

struct ForeignNotificationHost(Arc<dyn NotificationHandler>);

impl NotificationHost for ForeignNotificationHost {
    fn show(&self, from: &PeerInfo, notification: &Notification) {
        self.0
            .show(from.id.to_string(), from.name.clone(), notification.into());
    }
    fn remove(&self, from: &PeerInfo, id: &str) {
        self.0.remove(from.id.to_string(), id.to_owned());
    }
    fn sync(&self, from: &PeerInfo, active: &[String]) {
        self.0.sync(from.id.to_string(), active.to_vec());
    }
    fn dismiss_local(&self, id: &str) {
        self.0.dismiss_local(id.to_owned());
    }
    fn action_local(&self, id: &str, action: &str) {
        self.0.action_local(id.to_owned(), action.to_owned());
    }
    fn reply_local(&self, id: &str, text: &str) {
        self.0.reply_local(id.to_owned(), text.to_owned());
    }
}

// ----- clipboard, battery, find my phone ---------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct BatteryData {
    pub percent: u8,
    pub charging: bool,
}

impl From<BatteryState> for BatteryData {
    fn from(b: BatteryState) -> Self {
        Self {
            percent: b.percent,
            charging: b.charging,
        }
    }
}

impl From<BatteryData> for BatteryState {
    fn from(b: BatteryData) -> Self {
        Self {
            percent: b.percent,
            charging: b.charging,
        }
    }
}

/// Implemented in Kotlin: device-level features. Called on Rust threads: hand off quickly.
#[uniffi::export(with_foreign)]
pub trait DeviceHandler: Send + Sync {
    /// Put text from a paired device on the clipboard.
    fn set_clipboard(&self, from_id: String, from_name: String, text: String);
    /// Start or stop ringing (find my phone).
    fn ring(&self, from_id: String, from_name: String, on: bool);
    /// The phone's battery right now.
    fn battery(&self) -> Option<BatteryData>;
    /// A paired device reported its battery.
    fn peer_battery(
        &self,
        from_id: String,
        from_name: String,
        state: BatteryData,
        previous: Option<BatteryData>,
    );
}

struct ForeignDeviceHost(Arc<dyn DeviceHandler>);

impl ClipboardHost for ForeignDeviceHost {
    fn set_clipboard(&self, from: &PeerInfo, text: &str) {
        self.0
            .set_clipboard(from.id.to_string(), from.name.clone(), text.to_owned());
    }
}

impl BatteryHost for ForeignDeviceHost {
    fn current(&self) -> Option<BatteryState> {
        self.0.battery().map(Into::into)
    }
    fn peer_changed(&self, from: &PeerInfo, state: BatteryState, previous: Option<BatteryState>) {
        self.0.peer_battery(
            from.id.to_string(),
            from.name.clone(),
            state.into(),
            previous.map(Into::into),
        );
    }
}

impl FindMyHost for ForeignDeviceHost {
    fn ring(&self, from: &PeerInfo, on: bool) {
        self.0.ring(from.id.to_string(), from.name.clone(), on);
    }
}

// ----- sharing -----------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum TransferStatus {
    /// Incoming: waiting for the user to accept. Outgoing: waiting for the other device.
    Waiting,
    Running,
    Done,
    Failed {
        reason: String,
    },
    Cancelled,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct TransferData {
    pub id: u64,
    pub device_id: String,
    pub device_name: String,
    pub incoming: bool,
    /// A plain file name, safe to create.
    pub name: String,
    pub size: u64,
    pub mime: Option<String>,
    /// Bytes the receiver has confirmed.
    pub bytes: u64,
    pub status: TransferStatus,
}

impl From<&Transfer> for TransferData {
    fn from(t: &Transfer) -> Self {
        Self {
            id: t.id,
            device_id: t.peer.to_string(),
            device_name: t.peer_name.clone(),
            incoming: t.incoming,
            name: t.name.clone(),
            size: t.size,
            mime: t.mime.clone(),
            bytes: t.bytes,
            status: match &t.state {
                TransferState::Waiting => TransferStatus::Waiting,
                TransferState::Running => TransferStatus::Running,
                TransferState::Done => TransferStatus::Done,
                TransferState::Failed(reason) => TransferStatus::Failed {
                    reason: reason.clone(),
                },
                TransferState::Cancelled => TransferStatus::Cancelled,
            },
        }
    }
}

/// Implemented in Kotlin: files, links and text from paired devices. Called on Rust threads
/// (including per-transfer I/O threads): hand off quickly.
#[uniffi::export(with_foreign)]
pub trait ShareHandler: Send + Sync {
    /// A device offers a file: answer with [`Node::accept_transfer`] or
    /// [`Node::cancel_transfer`].
    fn file_offered(&self, transfer: TransferData);
    /// Progress (at most every 250 ms) or a state change. An incoming transfer that is `Done`
    /// is complete, verified and closed; after `Failed` or `Cancelled` delete the partial file.
    fn transfer_changed(&self, transfer: TransferData);
    /// `url` is only true for `http(s)` links.
    fn text_received(&self, from_id: String, from_name: String, text: String, url: bool);
}

struct ForeignShareHost(Arc<dyn ShareHandler>);

impl ShareHost for ForeignShareHost {
    fn file_offered(&self, transfer: &Transfer) {
        self.0.file_offered(transfer.into());
    }
    fn transfer_changed(&self, transfer: &Transfer) {
        self.0.transfer_changed(transfer.into());
    }
    fn text_received(&self, from: &PeerInfo, text: &str, url: bool) {
        self.0
            .text_received(from.id.to_string(), from.name.clone(), text.to_owned(), url);
    }
}

/// Take ownership of a file descriptor that Kotlin detached from a `ParcelFileDescriptor`.
fn file_from_fd(fd: i32) -> Result<File, PairlyError> {
    if fd < 0 {
        return Err(failed("invalid file descriptor"));
    }
    // SAFETY: Kotlin hands over a valid descriptor it no longer owns (`detachFd()`), so this is
    // its only owner and closing it on drop is correct.
    Ok(File::from(unsafe { OwnedFd::from_raw_fd(fd) }))
}

// ----- the node ----------------------------------------------------------------------------

#[derive(uniffi::Object)]
pub struct Node {
    inner: PairlyNode,
    notifications: Arc<NotificationPlugin>,
    clipboard: Arc<ClipboardPlugin>,
    battery: Arc<BatteryPlugin>,
    findmy: Arc<FindMyPlugin>,
    share: Arc<SharePlugin>,
    bluetooth: Arc<ForeignBluetooth>,
    forward: AbortHandle,
}

/// Start the node. Call [`Node::shutdown`] when the service stops.
#[uniffi::export]
pub async fn start_node(
    options: NodeOptions,
    secrets: Arc<dyn SecretStore>,
    listener: Arc<dyn EventListener>,
    notifications: Arc<dyn NotificationHandler>,
    device: Arc<dyn DeviceHandler>,
    share: Arc<dyn ShareHandler>,
    bluetooth: Arc<dyn BluetoothHandler>,
) -> Result<Arc<Node>, PairlyError> {
    init_logging();
    let task = runtime().spawn(async move {
        let data_dir = PathBuf::from(&options.data_dir);
        let notifications =
            NotificationPlugin::new(Arc::new(ForeignNotificationHost(notifications)));
        let device_host = Arc::new(ForeignDeviceHost(device));
        let clipboard = ClipboardPlugin::new(device_host.clone());
        let battery = BatteryPlugin::new(device_host.clone());
        let findmy = FindMyPlugin::new(device_host);
        let share = SharePlugin::new(Arc::new(ForeignShareHost(share)));
        let bluetooth = ForeignBluetooth::new(bluetooth);
        let mut config = NodeConfig::new(options.name, options.kind.into());
        config.relay = options.relay.filter(|r| !r.trim().is_empty());
        let node = PairlyNode::builder(config)
            .keystore(Arc::new(ForeignKeyStore(secrets)))
            .registry(Registry::open(&data_dir.join("registry.db"))?)
            .platform(Arc::new(FfiPlatform {
                listener: listener.clone(),
            }))
            .transport(LanTransport::new(LanConfig {
                port: options.lan_port,
                mdns: true,
            }))
            // Only while a device has no LAN link: an idle relay connection costs battery.
            .transport(RelayTransport::new(RelayConfig {
                only_when_needed: true,
            }))
            .transport(bluetooth.clone())
            .plugin(Arc::new(PingPlugin))
            .plugin(notifications.clone())
            .plugin(clipboard.clone())
            .plugin(battery.clone())
            .plugin(findmy.clone())
            .plugin(share.clone())
            .start()
            .await?;
        let mut events = node.subscribe();
        let forward = tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(event) => listener.on_event(event.into()),
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        });
        Ok::<_, CoreError>(Node {
            inner: node,
            notifications,
            clipboard,
            battery,
            findmy,
            share,
            bluetooth,
            forward: forward.abort_handle(),
        })
    });
    Ok(Arc::new(task.await.map_err(failed)??))
}

#[uniffi::export]
impl Node {
    pub fn device_id(&self) -> String {
        self.inner.device_id().to_string()
    }

    pub fn name(&self) -> String {
        self.inner.name().to_owned()
    }

    /// Paired devices first, then discovered ones.
    pub fn devices(&self) -> Result<Vec<Device>, PairlyError> {
        Ok(self
            .inner
            .devices()?
            .into_iter()
            .map(|d| Device {
                id: d.id.to_string(),
                name: d.name,
                kind: d.device_type.map(Into::into),
                paired: d.paired,
                link: d.link.map(Into::into),
                rtt_ms: d
                    .rtt
                    .map(|r| u32::try_from(r.as_millis()).unwrap_or(u32::MAX).max(1)),
                battery: self.battery.peer_state(d.id).map(Into::into),
            })
            .collect())
    }

    pub async fn request_pair(&self, id: String) -> Result<(), PairlyError> {
        let (node, id) = (self.inner.clone(), parse_id(&id)?);
        runtime()
            .spawn(async move { node.request_pair(id).await })
            .await
            .map_err(failed)??;
        Ok(())
    }

    /// Pair with the device whose QR code was scanned. Resolves once paired.
    pub async fn pair_from_qr(&self, uri: String) -> Result<(), PairlyError> {
        let node = self.inner.clone();
        runtime()
            .spawn(async move { node.pair_from_qr(&uri).await })
            .await
            .map_err(failed)??;
        Ok(())
    }

    pub fn confirm_pair(&self, id: String, accept: bool) -> Result<(), PairlyError> {
        Ok(self.inner.confirm_pair(parse_id(&id)?, accept)?)
    }

    pub fn unpair(&self, id: String) -> Result<(), PairlyError> {
        Ok(self.inner.unpair(parse_id(&id)?)?)
    }

    pub fn ping(&self, id: String, message: Option<String>) -> Result<(), PairlyError> {
        let packet = pairly_plugins::ping::packet(message)?;
        self.inner.send(parse_id(&id)?, packet)?;
        Ok(())
    }

    /// Send text to one device's clipboard.
    pub fn send_clipboard(&self, device: String, text: String) -> Result<(), PairlyError> {
        Ok(self.clipboard.send_to(parse_id(&device)?, &text)?)
    }

    /// Send text to every connected device's clipboard (skipped if it was just received).
    pub fn clipboard_changed(&self, text: String) {
        self.clipboard.local_changed(&text);
    }

    /// Ask a device to start or stop ringing.
    pub fn ring(&self, device: String, on: bool) -> Result<(), PairlyError> {
        Ok(self.findmy.ring(parse_id(&device)?, on)?)
    }

    /// The phone's battery changed.
    pub fn battery_changed(&self, state: BatteryData) {
        self.battery.local_changed(state.into());
    }

    /// One of the phone's notifications appeared or changed.
    pub fn notification_posted(&self, notification: NotificationData) {
        self.notifications.posted(notification.into());
    }

    /// One of the phone's notifications went away.
    pub fn notification_removed(&self, id: String) {
        self.notifications.removed(&id);
    }

    /// The user dismissed a mirror of `device`'s notification.
    pub fn mirror_dismissed(&self, device: String, id: String) -> Result<(), PairlyError> {
        Ok(self
            .notifications
            .request_dismiss(parse_id(&device)?, &id)?)
    }

    pub fn mirror_action(
        &self,
        device: String,
        id: String,
        action: String,
    ) -> Result<(), PairlyError> {
        Ok(self
            .notifications
            .request_action(parse_id(&device)?, &id, &action)?)
    }

    pub fn mirror_reply(
        &self,
        device: String,
        id: String,
        text: String,
    ) -> Result<(), PairlyError> {
        Ok(self
            .notifications
            .request_reply(parse_id(&device)?, &id, &text)?)
    }

    /// Send a link (`url`) or text to a device.
    pub fn send_text(&self, device: String, text: String, url: bool) -> Result<(), PairlyError> {
        Ok(self.share.send_text(parse_id(&device)?, &text, url)?)
    }

    /// Offer a file to a device. Takes ownership of `fd` (a seekable, readable descriptor).
    /// Returns the transfer id.
    pub fn send_file(
        &self,
        device: String,
        fd: i32,
        name: String,
        mime: Option<String>,
    ) -> Result<u64, PairlyError> {
        let file = file_from_fd(fd)?;
        Ok(self
            .share
            .send_file(parse_id(&device)?, file, &name, mime.as_deref())?)
    }

    /// Accept an offered file into `fd` (a writable, seekable descriptor; takes ownership).
    pub fn accept_transfer(&self, id: u64, fd: i32) -> Result<(), PairlyError> {
        let file = file_from_fd(fd)?;
        Ok(self.share.accept(id, file)?)
    }

    /// Cancel a transfer or decline an offer.
    pub fn cancel_transfer(&self, id: u64) -> Result<(), PairlyError> {
        Ok(self.share.cancel(id)?)
    }

    /// Transfers that haven't finished.
    pub fn transfers(&self) -> Vec<TransferData> {
        self.share.transfers().iter().map(Into::into).collect()
    }

    /// A paired device connected to the app's Bluetooth server socket.
    pub fn bluetooth_incoming(&self, socket: Arc<dyn BluetoothSocket>, address: String) {
        self.bluetooth.incoming(socket, address);
    }

    /// Call when the OS reports a network change or the app returns to the foreground.
    pub async fn network_changed(&self) {
        let node = self.inner.clone();
        let _ = runtime()
            .spawn(async move { node.network_changed().await })
            .await;
    }

    pub async fn shutdown(&self) {
        self.forward.abort();
        let node = self.inner.clone();
        let _ = runtime().spawn(async move { node.shutdown().await }).await;
    }
}
