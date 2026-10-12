//! The engine: the Pairly node with its features, and what the window shows.
//!
//! The node runs inside the app (Windows has no session bus to talk to a separate service);
//! the tray keeps the app running with the window closed. Every change is sent to the window as
//! one `state` event with the whole picture, which the page renders. Larger things (messages,
//! contacts, a phone's folders, artwork) are asked for by the page and answered on their own.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use anyhow::{Context, Result};
use pairly_core::registry::Registry;
use pairly_core::{DeviceId, DeviceType, NodeConfig, NodeEvent, PairlyNode};
use pairly_crypto::FileKeyStore;
use pairly_plugins::battery::BatteryPlugin;
use pairly_plugins::clipboard::ClipboardPlugin;
use pairly_plugins::command::CommandPlugin;
use pairly_plugins::contacts::ContactsPlugin;
use pairly_plugins::files::FilesPlugin;
use pairly_plugins::findmy::FindMyPlugin;
use pairly_plugins::input::InputPlugin;
use pairly_plugins::media::{MediaPlugin, PlayerState};
use pairly_plugins::notification::NotificationPlugin;
use pairly_plugins::ping::PingPlugin;
use pairly_plugins::power::{PowerAction, PowerPlugin};
use pairly_plugins::screen::ScreenPlugin;
use pairly_plugins::share::{SharePlugin, Transfer, TransferState};
use pairly_plugins::sms::SmsPlugin;
use pairly_plugins::telephony::{CallState, TelephonyPlugin};
use pairly_transport_lan::{LanConfig, LanTransport};
use pairly_transport_relay::{RelayConfig, RelayTransport};
use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tauri_plugin_notification::NotificationExt;

use crate::commands::{Commands, CommandsHost};
use crate::hosts::{Host, NoShare};
use crate::platform::toast::{self, Toast};
use crate::platform::{self, clipboard::Clipboard, input::RemoteInput, media::LocalMedia};
use crate::screen::Screens;
use crate::settings::{Settings, computer_name};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The features the window and the PC use while the node runs.
#[derive(Clone)]
pub struct Features {
    pub clipboard: Arc<ClipboardPlugin>,
    pub battery: Arc<BatteryPlugin>,
    pub findmy: Arc<FindMyPlugin>,
    pub notifications: Arc<NotificationPlugin>,
    pub share: Arc<SharePlugin>,
    pub power: Arc<PowerPlugin>,
    pub media: Arc<MediaPlugin>,
    pub sms: Arc<SmsPlugin>,
    pub telephony: Arc<TelephonyPlugin>,
    pub contacts: Arc<ContactsPlugin>,
    pub files: Arc<FilesPlugin>,
    pub command: Arc<CommandPlugin>,
    pub screen: Arc<ScreenPlugin>,
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

/// A call on a paired phone, while it rings or lasts.
#[derive(Debug, Clone, Serialize)]
pub struct CallView {
    /// ringing or talking.
    pub state: String,
    pub caller: String,
    pub number: String,
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
    players: Vec<PlayerState>,
    /// When `players` arrived (Unix ms), for moving their positions on.
    players_at: u64,
    call: Option<CallView>,
    /// This PC's screen is being shown on it.
    sharing: bool,
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
    /// This PC's players, as phones see them.
    local_players: Vec<PlayerState>,
    /// The relay is set but couldn't be used.
    relay_error: Option<String>,
    /// Bluetooth: "on", "off" (switched off) or why it isn't there.
    bluetooth: String,
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

pub struct Backend {
    pub app: AppHandle,
    pub data_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub downloads: PathBuf,
    settings: Mutex<Settings>,
    node: tokio::sync::Mutex<Option<PairlyNode>>,
    /// A clone of the running node, for quick synchronous reads.
    current: Mutex<Option<(PairlyNode, Features)>>,
    pub pairing: Mutex<Option<Pairing>>,
    qr: Mutex<Option<String>>,
    pub update: Mutex<UpdateView>,
    /// Phones we asked to ring (the page shows "Stop").
    pub ringing: Mutex<BTreeSet<String>>,
    /// Incoming files being written: transfer id → (partial file, final file).
    pub receiving: Mutex<HashMap<u64, (PathBuf, PathBuf)>>,
    pub clipboard: Arc<Clipboard>,
    pub input: RemoteInput,
    /// This PC is ringing (the phone is looking for it).
    pub rung: Mutex<bool>,
    pub media: Arc<LocalMedia>,
    pub commands: Arc<Commands>,
    pub screens: Arc<Screens>,
    /// Artwork from phones: (device, key) → a `data:` URL.
    pub peer_art: Mutex<HashMap<(DeviceId, String), String>>,
    /// When each device's players last arrived.
    pub players_at: Mutex<HashMap<DeviceId, u64>>,
    /// Calls on phones, while they ring or last.
    pub calls: Mutex<HashMap<DeviceId, CallView>>,
    /// This PC's players paused for a call, to resume after it.
    pub paused_for_call: Mutex<Vec<String>>,
    /// Phone notifications shown here: (device, notification id) → toast tag.
    pub shown_notes: Mutex<HashMap<(DeviceId, String), String>>,
    relay_error: Mutex<Option<String>>,
    bluetooth_state: Mutex<String>,
    /// Apps on this PC that posted notifications: name → when last (Unix ms).
    seen_apps: Mutex<std::collections::BTreeMap<String, u64>>,
}

impl Backend {
    pub fn new(app: AppHandle, data_dir: PathBuf, downloads: PathBuf) -> Arc<Self> {
        let settings = Settings::load(&data_dir);
        let cache_dir = data_dir.join("cache");
        let (commands, runs) = Commands::load(&data_dir);
        let data_dir_for_apps = data_dir.clone();
        let this = Arc::new_cyclic(|weak: &std::sync::Weak<Self>| {
            let weak_media = weak.clone();
            Self {
                app,
                data_dir,
                cache_dir,
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
                media: LocalMedia::start(move || {
                    if let Some(this) = weak_media.upgrade() {
                        if let Some((_, f)) = this.running() {
                            f.media.local_changed();
                        }
                        this.emit();
                    }
                }),
                commands,
                screens: Screens::new(weak.clone()),
                peer_art: Mutex::default(),
                players_at: Mutex::default(),
                calls: Mutex::default(),
                paused_for_call: Mutex::default(),
                shown_notes: Mutex::default(),
                relay_error: Mutex::new(None),
                bluetooth_state: Mutex::new(String::new()),
                seen_apps: Mutex::new(load_seen_apps(&data_dir_for_apps)),
            }
        });
        // The app's name and icon on its notifications.
        let icon = this.data_dir.join("pairly.png");
        if std::fs::create_dir_all(&this.data_dir).is_ok() {
            let _ = std::fs::write(&icon, include_bytes!("../icons/128x128.png"));
        }
        toast::register(Some(&icon));
        // Commands paired phones run here, one at a time.
        let weak = Arc::downgrade(&this);
        tauri::async_runtime::spawn(crate::commands::run_queue(
            this.commands.clone(),
            runs,
            move |title, body| {
                if let Some(this) = weak.upgrade() {
                    this.notify(title, body);
                }
            },
        ));
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
        // This PC's notifications, for paired phones (where Windows allows reading them).
        crate::platform::listener::start(Arc::downgrade(&this));
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

    pub fn running(&self) -> Option<(PairlyNode, Features)> {
        lock(&self.current).clone()
    }

    pub fn node(&self) -> Result<PairlyNode> {
        self.running().map(|(n, _)| n).context("Pairly is off")
    }

    pub fn features(&self) -> Result<Features> {
        self.running().map(|(_, f)| f).context("Pairly is off")
    }

    pub fn device_name(&self, id: DeviceId) -> String {
        self.node()
            .ok()
            .and_then(|n| n.devices().ok())
            .and_then(|list| list.into_iter().find(|d| d.id == id))
            .map(|d| d.name)
            .unwrap_or_else(|| "Your device".into())
    }

    /// A desktop or laptop (rather than a phone or tablet).
    pub fn is_pc(&self, id: DeviceId) -> bool {
        self.node()
            .ok()
            .and_then(|n| n.devices().ok())
            .and_then(|list| list.into_iter().find(|d| d.id == id))
            .is_some_and(|d| {
                matches!(
                    d.device_type,
                    Some(DeviceType::Desktop | DeviceType::Laptop)
                )
            })
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
        config.relay_padding = settings.relay_padding;
        *lock(&self.relay_error) = None;
        if let Some(relay) = settings
            .relay
            .as_deref()
            .map(str::trim)
            .filter(|r| !r.is_empty())
        {
            match relay.parse::<pairly_transport_relay::RelayAddr>() {
                Ok(_) => config.relay = Some(relay.to_owned()),
                Err(e) => {
                    tracing::warn!(error = %e, "the relay address isn't valid");
                    *lock(&self.relay_error) = Some(format!("The relay address isn't valid: {e}"));
                }
            }
        }
        let bluetooth = if settings.bluetooth {
            match crate::platform::bluetooth::BluetoothTransport::new() {
                Ok(bt) => {
                    config.bluetooth = bt.address();
                    *lock(&self.bluetooth_state) = "on".into();
                    tracing::info!(address = ?config.bluetooth, "Bluetooth available");
                    Some(bt)
                }
                Err(e) => {
                    tracing::info!(error = %e, "no Bluetooth");
                    *lock(&self.bluetooth_state) = e.to_string();
                    None
                }
            }
        } else {
            *lock(&self.bluetooth_state) = "off".into();
            None
        };
        let host = Arc::new(Host(Arc::downgrade(self)));
        let features = Features {
            clipboard: ClipboardPlugin::new(host.clone()),
            battery: BatteryPlugin::new(host.clone()),
            findmy: FindMyPlugin::new(host.clone()),
            notifications: NotificationPlugin::new(host.clone()),
            share: SharePlugin::new(host.clone()),
            power: PowerPlugin::new(host.clone()),
            media: MediaPlugin::new(host.clone()),
            sms: SmsPlugin::new(host.clone()),
            telephony: TelephonyPlugin::new(host.clone()),
            contacts: ContactsPlugin::new(Arc::new(NoShare)),
            files: FilesPlugin::new(Arc::new(NoShare)),
            command: CommandPlugin::new(Arc::new(CommandsHost(self.commands.clone()))),
            screen: ScreenPlugin::new(host.clone()),
        };
        self.commands.set_plugin(Some(&features.command));
        // Development: a second Pairly on this PC can't share the usual port.
        let port = std::env::var("PAIRLY_LAN_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(pairly_transport_lan::DEFAULT_PORT);
        let mut builder = PairlyNode::builder(config)
            .keystore(Arc::new(FileKeyStore::new(
                self.data_dir.join("identity.key"),
            )))
            .registry(Registry::open(&self.data_dir.join("registry.db"))?)
            .platform(host.clone())
            // A PC stays in its devices' relay rooms all the time, so a phone away from home
            // can always reach it.
            .transport(RelayTransport::new(RelayConfig {
                only_when_needed: false,
            }))
            .transport(LanTransport::new(LanConfig { port, mdns: true }));
        if let Some(bt) = bluetooth {
            builder = builder.transport(bt);
        }
        let node = builder
            .plugin(Arc::new(PingPlugin))
            .plugin(features.clipboard.clone())
            .plugin(features.battery.clone())
            .plugin(features.findmy.clone())
            .plugin(features.notifications.clone())
            .plugin(features.share.clone())
            .plugin(features.power.clone())
            .plugin(features.media.clone())
            .plugin(features.sms.clone())
            .plugin(features.telephony.clone())
            .plugin(features.contacts.clone())
            .plugin(features.files.clone())
            .plugin(features.command.clone())
            .plugin(features.screen.clone())
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
        self.commands.set_plugin(None);
        self.screens.stop_all();
        lock(&self.calls).clear();
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
        if on {
            self.start().await
        } else {
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

    /// Use a relay (or none, with an empty address); Pairly restarts to take it.
    pub async fn set_relay(self: &Arc<Self>, address: &str) -> Result<()> {
        let address = address.trim();
        if !address.is_empty() {
            address
                .parse::<pairly_transport_relay::RelayAddr>()
                .map_err(|e| anyhow::anyhow!("that isn't a relay address: {e}"))?;
        }
        self.change_settings(|s| {
            s.relay = (!address.is_empty()).then(|| address.to_owned());
        });
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
                if *incoming {
                    self.notify(
                        &format!("Pairing request from {name}"),
                        &format!(
                            "Code {code}. Accept only if the other device shows the same code."
                        ),
                    );
                }
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
            NodeEvent::Disconnected { id, .. } | NodeEvent::Unpaired { id } => {
                // Its notifications and calls aren't current any more.
                if lock(&self.calls).remove(id).is_some() {
                    toast::remove(&id.to_string(), "call");
                }
                lock(&self.shown_notes).retain(|(d, _), _| d != id);
                lock(&self.players_at).remove(id);
                toast::remove_group(&id.to_string());
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
        let calls = lock(&self.calls).clone();
        let players_at = lock(&self.players_at).clone();
        let sharing = self.screens.casting();
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
                            players: f.media.peer_players(d.id),
                            players_at: players_at.get(&d.id).copied().unwrap_or(0),
                            call: calls.get(&d.id).cloned(),
                            sharing: sharing.contains(&d.id),
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
            local_players: self.media.players(),
            relay_error: lock(&self.relay_error).clone(),
            bluetooth: lock(&self.bluetooth_state).clone(),
        };
        let _ = self.app.emit_to("main", "state", snapshot);
    }

    /// A plain notification.
    pub fn notify(&self, title: &str, body: &str) {
        let weak = self.screens.backend();
        let shown = toast::show(
            &Toast {
                title: title.to_owned(),
                body: body.to_owned(),
                ..Toast::default()
            },
            Some(Box::new(move |event| {
                if let (toast::ToastEvent::Opened, Some(b)) = (event, weak.upgrade()) {
                    b.show_window();
                }
            })),
        );
        if !shown {
            let _ = self
                .app
                .notification()
                .builder()
                .title(title)
                .body(body)
                .show();
        }
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
        self.features()?
            .share
            .send_text(parse(id)?, text.trim(), url)?;
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

    /// Control a phone's player from the window.
    pub fn media_control(&self, id: &str, player: &str, action: &str, value: i64) -> Result<()> {
        use pairly_plugins::media::MediaAction;
        let action = match action {
            "play" => MediaAction::Play,
            "pause" => MediaAction::Pause,
            "play_pause" => MediaAction::PlayPause,
            "stop" => MediaAction::Stop,
            "next" => MediaAction::Next,
            "previous" => MediaAction::Previous,
            "seek" => MediaAction::Seek(value),
            "set_position" => MediaAction::SetPosition(u64::try_from(value).unwrap_or(0)),
            "set_volume" => MediaAction::SetVolume(value.clamp(0, 100) as u8),
            other => anyhow::bail!("unknown media action {other}"),
        };
        Ok(self.features()?.media.command(parse(id)?, player, action)?)
    }

    /// A phone's artwork as a `data:` URL (or the `https` link it is).
    pub fn artwork(&self, id: &str, key: &str) -> Option<String> {
        if key.starts_with("https://") {
            return Some(key.to_owned());
        }
        lock(&self.peer_art)
            .get(&(parse(id).ok()?, key.to_owned()))
            .cloned()
    }

    // ----- incoming files ------------------------------------------------------------------

    /// A free name in `dir`: "photo.jpg", else "photo (2).jpg", and so on.
    pub fn free_name(dir: &Path, name: &str) -> PathBuf {
        let name = pairly_plugins::share::safe_file_name(name);
        let path = Path::new(&name);
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "file".into());
        let ext = path
            .extension()
            .map(|e| format!(".{}", e.to_string_lossy()))
            .unwrap_or_default();
        let taken = |p: &Path| p.exists() || part_path(p).exists();
        let mut candidate = dir.join(&name);
        let mut n = 2;
        while taken(&candidate) {
            candidate = dir.join(format!("{stem} ({n}){ext}"));
            n += 1;
        }
        candidate
    }

    pub fn accept_file(&self, t: &Transfer) -> Result<()> {
        std::fs::create_dir_all(&self.downloads)?;
        let target = Self::free_name(&self.downloads, &t.name);
        let part = part_path(&target);
        let file = std::fs::File::create(&part)?;
        lock(&self.receiving).insert(t.id, (part, target));
        self.features()?.share.accept(t.id, file)?;
        Ok(())
    }

    /// The call on a phone changed: show it, and pause or resume this PC's music.
    pub fn call_changed(
        self: &Arc<Self>,
        id: DeviceId,
        name: &str,
        state: CallState,
        caller: &str,
        number: &str,
    ) {
        let group = id.to_string();
        match state {
            CallState::Ringing | CallState::Talking => {
                let ringing = state == CallState::Ringing;
                lock(&self.calls).insert(
                    id,
                    CallView {
                        state: if ringing { "ringing" } else { "talking" }.into(),
                        caller: caller.to_owned(),
                        number: number.to_owned(),
                    },
                );
                let buttons = if ringing {
                    vec![
                        toast::Button {
                            id: "answer".into(),
                            label: "Answer".into(),
                        },
                        toast::Button {
                            id: "speaker".into(),
                            label: "Speaker".into(),
                        },
                        toast::Button {
                            id: "reject".into(),
                            label: "Reject".into(),
                        },
                    ]
                } else {
                    vec![toast::Button {
                        id: "hangup".into(),
                        label: "Hang Up".into(),
                    }]
                };
                let weak = Arc::downgrade(self);
                toast::show(
                    &Toast {
                        title: if ringing {
                            format!("Incoming call from {caller}")
                        } else {
                            format!("On a call with {caller}")
                        },
                        body: format!("On {name}"),
                        buttons,
                        call: ringing,
                        silent: !ringing,
                        tag: "call".into(),
                        group: group.clone(),
                        ..Toast::default()
                    },
                    Some(Box::new(move |event| {
                        let Some(b) = weak.upgrade() else { return };
                        match event {
                            toast::ToastEvent::Clicked(action) => {
                                if let Err(e) = b.call_action(&group, &action) {
                                    b.notify("Couldn't reach the phone", &e.to_string());
                                }
                            }
                            toast::ToastEvent::Opened => b.show_window(),
                            _ => {}
                        }
                    })),
                );
                if self.settings().pause_media_for_calls {
                    let this = Arc::downgrade(self);
                    std::thread::spawn(move || {
                        let Some(this) = this.upgrade() else { return };
                        let paused = this.media.pause_playing();
                        let mut list = lock(&this.paused_for_call);
                        for p in paused {
                            if !list.contains(&p) {
                                list.push(p);
                            }
                        }
                    });
                }
            }
            CallState::Missed | CallState::Ended => {
                lock(&self.calls).remove(&id);
                toast::remove(&group, "call");
                if state == CallState::Missed {
                    toast::show(
                        &Toast {
                            title: format!("Missed call from {caller}"),
                            body: format!("On {name}"),
                            tag: "missed".into(),
                            group,
                            ..Toast::default()
                        },
                        None,
                    );
                }
                let paused = std::mem::take(&mut *lock(&self.paused_for_call));
                self.media.resume(paused);
            }
        }
        self.emit();
    }
}

pub fn part_path(target: &Path) -> PathBuf {
    target.with_file_name(format!(
        "{}.part",
        target.file_name().unwrap_or_default().to_string_lossy()
    ))
}

pub fn parse(id: &str) -> Result<DeviceId> {
    id.parse().map_err(|_| anyhow::anyhow!("not a device id"))
}

/// A `data:` URL for an image, its type guessed from its first bytes.
pub fn data_url(bytes: &[u8]) -> String {
    use base64::Engine;
    let mime = image_mime(bytes);
    format!(
        "data:{mime};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    )
}

pub fn image_mime(bytes: &[u8]) -> &'static str {
    match bytes {
        [0xFF, 0xD8, ..] => "image/jpeg",
        [0x89, b'P', b'N', b'G', ..] => "image/png",
        [b'G', b'I', b'F', ..] => "image/gif",
        [
            b'R',
            b'I',
            b'F',
            b'F',
            _,
            _,
            _,
            _,
            b'W',
            b'E',
            b'B',
            b'P',
            ..,
        ] => "image/webp",
        [b'B', b'M', ..] => "image/bmp",
        _ => "application/octet-stream",
    }
}

fn load_seen_apps(dir: &Path) -> std::collections::BTreeMap<String, u64> {
    std::fs::read(dir.join("notification-apps.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

impl Backend {
    /// An app on this PC posted a notification (for the list in Settings).
    pub fn note_app(&self, app: &str) {
        if app.is_empty() {
            return;
        }
        let mut apps = lock(&self.seen_apps);
        // To the hour: no need to write the file on every notification.
        let hour = now_ms() / 3_600_000 * 3_600_000;
        if apps.get(app) == Some(&hour) {
            return;
        }
        apps.insert(app.to_owned(), hour);
        if let Ok(json) = serde_json::to_vec_pretty(&*apps) {
            let _ = std::fs::write(self.data_dir.join("notification-apps.json"), json);
        }
    }

    /// Apps that posted notifications here, newest first, and whether they go to phones.
    pub fn notification_apps(&self) -> Vec<(String, bool)> {
        let muted = self.settings().muted_apps;
        let mut apps: Vec<(String, u64)> = lock(&self.seen_apps)
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        apps.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        apps.into_iter()
            .map(|(name, _)| {
                let send = !muted.contains(&name);
                (name, send)
            })
            .collect()
    }
}
