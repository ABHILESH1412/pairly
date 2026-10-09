//! The engine: the Pairly node with its features, and what the window shows.
//!
//! The node runs inside the app (Windows has no session bus to talk to a separate service);
//! the tray keeps the app running with the window closed. Every change is sent to the window as
//! one `state` event with the whole picture, which the page renders.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use anyhow::{Context, Result};
use pairly_core::registry::Registry;
use pairly_core::{DeviceId, DeviceType, NodeConfig, NodeEvent, PairlyNode, PeerInfo, Platform};
use pairly_crypto::FileKeyStore;
use pairly_plugins::battery::{BatteryHost, BatteryPlugin, BatteryState};
use pairly_plugins::clipboard::{ClipboardHost, ClipboardPlugin};
use pairly_plugins::findmy::{FindMyHost, FindMyPlugin};
use pairly_plugins::input::{InputHost, InputPlugin, KeyInput, PointerButton, PointerMotion};
use pairly_plugins::notification::{Notification, NotificationHost, NotificationPlugin};
use pairly_plugins::ping::PingPlugin;
use pairly_plugins::power::{PowerAction, PowerHost, PowerPlugin};
use pairly_plugins::share::{ShareHost, SharePlugin, Transfer, TransferState};
use pairly_transport_lan::{LanConfig, LanTransport};
use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tauri_plugin_notification::NotificationExt;

use crate::platform::{self, clipboard::Clipboard, input::RemoteInput};
use crate::settings::{Settings, computer_name};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The features the window and the PC use while the node runs.
#[derive(Clone)]
struct Features {
    clipboard: Arc<ClipboardPlugin>,
    battery: Arc<BatteryPlugin>,
    findmy: Arc<FindMyPlugin>,
    notifications: Arc<NotificationPlugin>,
    share: Arc<SharePlugin>,
    power: Arc<PowerPlugin>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Pairing {
    pub id: String,
    pub name: String,
    pub code: String,
    pub incoming: bool,
}

/// The updater's state, set by [`crate::update`].
#[derive(Debug, Clone, Default, Serialize)]
pub struct UpdateView {
    /// idle, checking, up-to-date, available, downloading, installed, failed.
    pub state: String,
    pub detail: String,
}

#[derive(Clone, Serialize)]
struct DeviceView {
    id: String,
    name: String,
    /// phone, tablet, laptop, desktop, or "" before pairing.
    kind: String,
    paired: bool,
    paused: bool,
    connected: bool,
    link: String,
    rtt_ms: u64,
    battery: i32,
    charging: bool,
}

#[derive(Clone, Serialize)]
struct TransferView {
    id: u64,
    device: String,
    name: String,
    incoming: bool,
    size: u64,
    bytes: u64,
    state: String,
}

#[derive(Clone, Serialize)]
struct Snapshot {
    version: &'static str,
    me: Option<(String, String)>,
    devices: Vec<DeviceView>,
    transfers: Vec<TransferView>,
    pairing: Option<Pairing>,
    qr: Option<String>,
    settings: Settings,
    update: UpdateView,
    ringing: Vec<String>,
}

pub struct Backend {
    app: AppHandle,
    data_dir: PathBuf,
    downloads: PathBuf,
    settings: Mutex<Settings>,
    node: tokio::sync::Mutex<Option<PairlyNode>>,
    /// A clone of the running node, for quick synchronous reads.
    current: Mutex<Option<(PairlyNode, Features)>>,
    pairing: Mutex<Option<Pairing>>,
    qr: Mutex<Option<String>>,
    pub update: Mutex<UpdateView>,
    /// Phones we asked to ring (the page shows "Stop").
    ringing: Mutex<BTreeSet<String>>,
    /// Incoming files being written: transfer id → (partial file, final file).
    receiving: Mutex<HashMap<u64, (PathBuf, PathBuf)>>,
    clipboard: Arc<Clipboard>,
    input: RemoteInput,
    /// This PC is ringing (the phone is looking for it).
    rung: Mutex<bool>,
}

impl Backend {
    pub fn new(app: AppHandle, data_dir: PathBuf, downloads: PathBuf) -> Arc<Self> {
        let settings = Settings::load(&data_dir);
        let this = Arc::new(Self {
            app,
            data_dir,
            downloads,
            settings: Mutex::new(settings),
            node: tokio::sync::Mutex::new(None),
            current: Mutex::new(None),
            pairing: Mutex::new(None),
            qr: Mutex::new(None),
            update: Mutex::new(UpdateView::default()),
            ringing: Mutex::new(BTreeSet::new()),
            receiving: Mutex::new(HashMap::new()),
            clipboard: Clipboard::new(),
            input: RemoteInput::new(),
            rung: Mutex::new(false),
        });
        // Copies on this PC go to connected devices (when that's on).
        let weak = Arc::downgrade(&this);
        this.clipboard.clone().watch(move |text| {
            if let Some(this) = weak.upgrade()
                && this.settings().clipboard_auto
                && let Some((_, f)) = this.running()
            {
                f.clipboard.local_changed(&text);
            }
        });
        // Our battery, every minute.
        let weak = Arc::downgrade(&this);
        tauri::async_runtime::spawn(async move {
            let mut last = None;
            loop {
                let Some(this) = weak.upgrade() else { return };
                let now = platform::battery();
                if now != last
                    && let (Some(state), Some((_, f))) = (now, this.running())
                {
                    f.battery.local_changed(state);
                }
                last = now;
                drop(this);
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        });
        this
    }

    pub fn settings(&self) -> Settings {
        lock(&self.settings).clone()
    }

    pub fn change_settings(&self, f: impl FnOnce(&mut Settings)) {
        let mut s = lock(&self.settings);
        f(&mut s);
        s.save(&self.data_dir);
    }

    fn running(&self) -> Option<(PairlyNode, Features)> {
        lock(&self.current).clone()
    }

    fn node(&self) -> Result<PairlyNode> {
        self.running()
            .map(|(n, _)| n)
            .context("Pairly is off")
    }

    fn features(&self) -> Result<Features> {
        self.running()
            .map(|(_, f)| f)
            .context("Pairly is off")
    }

    // ----- starting and stopping -----------------------------------------------------------

    /// Start the node (if Pairly is on and it isn't running).
    pub async fn start(self: &Arc<Self>) -> Result<()> {
        let mut slot = self.node.lock().await;
        if slot.is_some() || !self.settings().enabled {
            return Ok(());
        }
        std::fs::create_dir_all(&self.data_dir)?;
        let settings = self.settings();
        let name = settings.name.clone().unwrap_or_else(computer_name);
        let kind = if platform::battery().is_some() {
            DeviceType::Laptop
        } else {
            DeviceType::Desktop
        };
        let mut config = NodeConfig::new(name, kind);
        config.app_version = VERSION.to_owned();
        let host = Arc::new(Host(Arc::downgrade(self)));
        let features = Features {
            clipboard: ClipboardPlugin::new(host.clone()),
            battery: BatteryPlugin::new(host.clone()),
            findmy: FindMyPlugin::new(host.clone()),
            notifications: NotificationPlugin::new(host.clone()),
            share: SharePlugin::new(host.clone()),
            power: PowerPlugin::new(host.clone()),
        };
        let node = PairlyNode::builder(config)
            .keystore(Arc::new(FileKeyStore::new(self.data_dir.join("identity.key"))))
            .registry(Registry::open(&self.data_dir.join("registry.db"))?)
            .platform(host.clone())
            .transport(LanTransport::new(LanConfig {
                port: pairly_transport_lan::DEFAULT_PORT,
                mdns: true,
            }))
            .plugin(Arc::new(PingPlugin))
            .plugin(features.clipboard.clone())
            .plugin(features.battery.clone())
            .plugin(features.findmy.clone())
            .plugin(features.notifications.clone())
            .plugin(features.share.clone())
            .plugin(features.power.clone())
            .plugin(InputPlugin::new(host.clone()))
            .start()
            .await
            .context("starting Pairly")?;
        if let Some(state) = platform::battery() {
            features.battery.local_changed(state);
        }
        let mut events = node.subscribe();
        *lock(&self.current) = Some((node.clone(), features));
        *slot = Some(node);
        drop(slot);
        let weak = Arc::downgrade(self);
        tauri::async_runtime::spawn(async move {
            loop {
                let event = match events.recv().await {
                    Ok(e) => e,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => return,
                };
                let Some(this) = weak.upgrade() else { return };
                this.on_event(event);
            }
        });
        // Refresh link times and the like now and then.
        let weak = Arc::downgrade(self);
        tauri::async_runtime::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                let Some(this) = weak.upgrade() else { return };
                if this.running().is_none() {
                    return;
                }
                this.emit();
            }
        });
        self.emit();
        Ok(())
    }

    /// Stop the node: every device disconnects.
    pub async fn stop(&self) {
        let node = self.node.lock().await.take();
        *lock(&self.current) = None;
        *lock(&self.pairing) = None;
        *lock(&self.qr) = None;
        if let Some(node) = node {
            node.shutdown().await;
        }
        self.emit();
    }

    pub async fn restart(self: &Arc<Self>) -> Result<()> {
        self.stop().await;
        self.start().await
    }

    pub async fn set_enabled(self: &Arc<Self>, on: bool) -> Result<()> {
        self.change_settings(|s| s.enabled = on);
        if on { self.start().await } else {
            self.stop().await;
            Ok(())
        }
    }

    pub async fn rename(self: &Arc<Self>, name: &str) -> Result<()> {
        let name = name.trim();
        anyhow::ensure!(
            !name.is_empty() && name.chars().count() <= 64 && !name.chars().any(char::is_control),
            "use 1 to 64 characters"
        );
        self.change_settings(|s| s.name = Some(name.to_owned()));
        self.restart().await
    }

    fn on_event(&self, event: NodeEvent) {
        match &event {
            NodeEvent::PairingRequested {
                id,
                name,
                code,
                incoming,
            } => {
                *lock(&self.pairing) = Some(Pairing {
                    id: id.to_string(),
                    name: name.clone(),
                    code: code.to_string(),
                    incoming: *incoming,
                });
                self.show_window();
            }
            NodeEvent::Paired { name, .. } => {
                *lock(&self.pairing) = None;
                *lock(&self.qr) = None;
                self.notify("Paired", &format!("{name} is paired with this PC"));
            }
            NodeEvent::PairingFailed { reason, .. } => {
                *lock(&self.pairing) = None;
                self.notify("Pairing failed", reason);
            }
            _ => {}
        }
        self.emit();
    }

    // ----- what the window shows -----------------------------------------------------------

    pub fn emit(&self) {
        let settings = self.settings();
        let running = self.running();
        let me = running
            .as_ref()
            .map(|(n, _)| (n.device_id().to_string(), n.name().to_owned()));
        let devices = running
            .as_ref()
            .and_then(|(n, f)| Some((n.devices().ok()?, f)))
            .map(|(list, f)| {
                list.into_iter()
                    .map(|d| {
                        let battery = f.battery.peer_state(d.id);
                        DeviceView {
                            id: d.id.to_string(),
                            name: d.name,
                            kind: d
                                .device_type
                                .map(|t| t.as_str().to_owned())
                                .unwrap_or_default(),
                            paired: d.paired,
                            paused: d.paused,
                            connected: d.link.is_some(),
                            link: d.link.map(|l| format!("{l:?}")).unwrap_or_default(),
                            rtt_ms: d.rtt.map_or(0, |r| r.as_millis() as u64),
                            battery: battery.map_or(-1, |b| i32::from(b.percent)),
                            charging: battery.is_some_and(|b| b.charging),
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        let transfers = running
            .as_ref()
            .map(|(_, f)| {
                f.share
                    .transfers()
                    .into_iter()
                    .map(|t| TransferView {
                        id: t.id,
                        device: t.peer.to_string(),
                        name: t.name,
                        incoming: t.incoming,
                        size: t.size,
                        bytes: t.bytes,
                        state: match t.state {
                            TransferState::Waiting => "waiting".into(),
                            TransferState::Running => "running".into(),
                            TransferState::Done => "done".into(),
                            TransferState::Failed(e) => format!("failed: {e}"),
                            TransferState::Cancelled => "cancelled".into(),
                        },
                    })
                    .collect()
            })
            .unwrap_or_default();
        let snapshot = Snapshot {
            version: VERSION,
            me,
            devices,
            transfers,
            pairing: lock(&self.pairing).clone(),
            qr: lock(&self.qr).clone(),
            settings,
            update: lock(&self.update).clone(),
            ringing: lock(&self.ringing).iter().cloned().collect(),
        };
        let _ = self.app.emit("state", snapshot);
    }

    pub fn notify(&self, title: &str, body: &str) {
        let _ = self
            .app
            .notification()
            .builder()
            .title(title)
            .body(body)
            .show();
    }

    pub fn show_window(&self) {
        crate::show_main_window(&self.app);
    }

    // ----- actions from the window ---------------------------------------------------------

    pub async fn pair(&self, id: &str) -> Result<()> {
        let node = self.node()?;
        Ok(node.request_pair(parse(id)?).await?)
    }

    pub fn confirm_pair(&self, id: &str, accept: bool) -> Result<()> {
        *lock(&self.pairing) = None;
        let r = self.node()?.confirm_pair(parse(id)?, accept);
        self.emit();
        Ok(r?)
    }

    /// Show a pairing QR code (an SVG for the page).
    pub fn start_qr(&self) -> Result<()> {
        let invite = self.node()?.start_qr_pairing()?;
        let svg = qrcode::QrCode::new(invite.to_uri().as_bytes())?
            .render::<qrcode::render::svg::Color>()
            .min_dimensions(240, 240)
            .quiet_zone(true)
            .build();
        *lock(&self.qr) = Some(svg);
        self.emit();
        Ok(())
    }

    pub fn cancel_qr(&self) {
        if let Ok(node) = self.node() {
            node.cancel_qr_pairing();
        }
        *lock(&self.qr) = None;
        self.emit();
    }

    pub fn unpair(&self, id: &str) -> Result<()> {
        let r = self.node()?.unpair(parse(id)?);
        self.emit();
        Ok(r?)
    }

    pub async fn set_paused(&self, id: &str, paused: bool) -> Result<()> {
        let node = self.node()?;
        node.set_paused(parse(id)?, paused).await?;
        self.emit();
        Ok(())
    }

    pub fn ping(&self, id: &str) -> Result<()> {
        let packet = pairly_plugins::ping::packet(None)?;
        self.node()?.send(parse(id)?, packet)?;
        Ok(())
    }

    pub fn ring(&self, id: &str, on: bool) -> Result<()> {
        self.features()?.findmy.ring(parse(id)?, on)?;
        if on {
            lock(&self.ringing).insert(id.to_owned());
        } else {
            lock(&self.ringing).remove(id);
        }
        self.emit();
        Ok(())
    }

    pub fn send_clipboard(&self, id: &str) -> Result<()> {
        let text = Clipboard::get().context("the clipboard has no text")?;
        self.features()?.clipboard.send_to(parse(id)?, &text)?;
        Ok(())
    }

    pub fn send_text(&self, id: &str, text: &str) -> Result<()> {
        let url = pairly_plugins::share::is_web_url(text);
        self.features()?.share.send_text(parse(id)?, text.trim(), url)?;
        Ok(())
    }

    pub fn send_files(&self, id: &str, paths: &[String]) -> Result<usize> {
        let (peer, share) = (parse(id)?, self.features()?.share);
        let mut sent = 0;
        for path in paths {
            let path = Path::new(path);
            if !path.is_file() {
                continue;
            }
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "file".into());
            let file = std::fs::File::open(path).with_context(|| format!("opening {name}"))?;
            share.send_file(peer, file, &name, None)?;
            sent += 1;
        }
        self.emit();
        Ok(sent)
    }

    pub fn cancel_transfer(&self, id: u64) -> Result<()> {
        Ok(self.features()?.share.cancel(id)?)
    }

    pub async fn phone_power(&self, id: &str, action: &str) -> Result<()> {
        let action = match action {
            "lock" => PowerAction::Lock,
            "restart" => PowerAction::Restart,
            _ => PowerAction::PowerOff,
        };
        let power = self.features()?.power;
        Ok(power.request(parse(id)?, action).await?)
    }

    pub fn open_downloads(&self) {
        let _ = std::fs::create_dir_all(&self.downloads);
        platform::open(&self.downloads.to_string_lossy());
    }

    // ----- incoming files ------------------------------------------------------------------

    /// A free name in the downloads folder: "photo.jpg", else "photo (2).jpg", and so on.
    fn free_name(&self, name: &str) -> PathBuf {
        let path = Path::new(name);
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "file".into());
        let ext = path
            .extension()
            .map(|e| format!(".{}", e.to_string_lossy()))
            .unwrap_or_default();
        let taken = |p: &Path| {
            let part = p.with_file_name(format!(
                "{}.part",
                p.file_name().unwrap_or_default().to_string_lossy()
            ));
            p.exists() || part.exists()
        };
        let mut candidate = self.downloads.join(name);
        let mut n = 2;
        while taken(&candidate) {
            candidate = self.downloads.join(format!("{stem} ({n}){ext}"));
            n += 1;
        }
        candidate
    }

    fn accept_file(&self, t: &Transfer) -> Result<()> {
        std::fs::create_dir_all(&self.downloads)?;
        let target = self.free_name(&t.name);
        let part = target.with_file_name(format!(
            "{}.part",
            target.file_name().unwrap_or_default().to_string_lossy()
        ));
        let file = std::fs::File::create(&part)?;
        lock(&self.receiving).insert(t.id, (part, target));
        self.features()?.share.accept(t.id, file)?;
        Ok(())
    }
}

fn parse(id: &str) -> Result<DeviceId> {
    id.parse().map_err(|_| anyhow::anyhow!("not a device id"))
}

/// The node's view of this PC: every feature's hooks, answered by the backend.
struct Host(Weak<Backend>);

impl Host {
    fn with(&self, f: impl FnOnce(&Arc<Backend>)) {
        if let Some(b) = self.0.upgrade() {
            f(&b);
        }
    }
}

impl Platform for Host {
    fn ping_received(&self, from: &PeerInfo, message: Option<&str>) {
        self.with(|b| b.notify(&format!("Ping from {}", from.name), message.unwrap_or("")));
    }
}

impl ClipboardHost for Host {
    fn set_clipboard(&self, _from: &PeerInfo, text: &str) {
        self.with(|b| b.clipboard.set(text));
    }
}

impl BatteryHost for Host {
    fn current(&self) -> Option<BatteryState> {
        platform::battery()
    }

    fn peer_changed(&self, from: &PeerInfo, state: BatteryState, previous: Option<BatteryState>) {
        self.with(|b| {
            let was_ok = previous.is_none_or(|p| p.percent > 15 || p.charging);
            if was_ok && state.percent <= 15 && !state.charging {
                b.notify(
                    &format!("{} battery low", from.name),
                    &format!("{}% left", state.percent),
                );
            }
            b.emit();
        });
    }
}

impl FindMyHost for Host {
    fn ring(&self, from: &PeerInfo, on: bool) {
        self.with(|b| {
            *lock(&b.rung) = on;
            if !on {
                return;
            }
            b.notify(
                "Your PC is ringing",
                &format!("{} is looking for this PC", from.name),
            );
            b.show_window();
            // A beep every second or so, for half a minute or until stopped.
            let weak = Arc::downgrade(b);
            std::thread::spawn(move || {
                for _ in 0..25 {
                    match weak.upgrade() {
                        Some(b) if *lock(&b.rung) => platform::beep(),
                        _ => return,
                    }
                    std::thread::sleep(Duration::from_millis(1200));
                }
            });
        });
    }
}

impl NotificationHost for Host {
    fn show(&self, _from: &PeerInfo, n: &Notification) {
        if n.silent {
            return;
        }
        self.with(|b| {
            let title = if n.title.is_empty() {
                n.app.clone()
            } else {
                format!("{} · {}", n.title, n.app)
            };
            b.notify(&title, &n.text);
        });
    }

    fn remove(&self, _from: &PeerInfo, _id: &str) {}

    fn sync(&self, _from: &PeerInfo, _active: &[String]) {}

    fn dismiss_local(&self, _id: &str) {}

    fn action_local(&self, _id: &str, _action: &str) {}

    fn reply_local(&self, _id: &str, _text: &str) {}
}

impl ShareHost for Host {
    fn file_offered(&self, transfer: &Transfer) {
        self.with(|b| {
            // Files from paired devices are taken straight away, as on Linux.
            if let Err(e) = b.accept_file(transfer) {
                tracing::warn!(error = %e, "can't receive {}", transfer.name);
                if let Ok(f) = b.features() {
                    let _ = f.share.cancel(transfer.id);
                }
            }
            b.emit();
        });
    }

    fn transfer_changed(&self, t: &Transfer) {
        self.with(|b| {
            if t.state.is_finished() {
                if let Some((part, target)) = lock(&b.receiving).remove(&t.id) {
                    if t.state == TransferState::Done {
                        let _ = std::fs::rename(&part, &target);
                        b.notify(
                            &format!("Received {}", t.name),
                            &format!("From {} · in Downloads\\Pairly", t.peer_name),
                        );
                    } else {
                        let _ = std::fs::remove_file(&part);
                    }
                } else if !t.incoming && t.state == TransferState::Done {
                    b.notify(&format!("Sent {}", t.name), &format!("To {}", t.peer_name));
                }
            }
            b.emit();
        });
    }

    fn text_received(&self, from: &PeerInfo, text: &str, url: bool) {
        self.with(|b| {
            if url {
                platform::open(text);
                b.notify(&format!("Link from {}", from.name), text);
            } else {
                b.clipboard.set(text);
                b.notify(&format!("Text from {}", from.name), "Copied to the clipboard");
            }
        });
    }
}

impl PowerHost for Host {
    fn act(&self, from: &PeerInfo, action: PowerAction) -> std::result::Result<(), String> {
        tracing::info!(device = %from.name, ?action, "power request");
        platform::power(action)
    }
}

impl InputHost for Host {
    fn pointer(&self, _from: &PeerInfo, motion: PointerMotion) {
        self.with(|b| b.input.pointer(motion));
    }

    fn button(&self, _from: &PeerInfo, button: PointerButton) {
        self.with(|b| b.input.button(button));
    }

    fn key(&self, _from: &PeerInfo, key: &KeyInput) {
        self.with(|b| b.input.key(key.clone()));
    }
}
