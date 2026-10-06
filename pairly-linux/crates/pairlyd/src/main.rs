//! `pairlyd`: headless daemon, normally run as a systemd user service. It runs the Pairly node
//! with the Linux platform adapters and serves `io.github.abhilesh1412.Pairly.Daemon1` on the session bus.
#![forbid(unsafe_code)]

mod battery;
mod clipboard;
mod commands;
mod config;
mod contacts;
mod dbus;
mod input;
mod keyring;
mod laser;
mod media;
mod notification_apps;
mod notifications;
mod platform;
mod power;
mod screen;
mod share;
mod sms;
mod telephony;
mod tray;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use ksni::TrayMethods;
use pairly_core::{NodeConfig, NodeEvent, PairlyNode, Registry};
use pairly_crypto::{KeyStore, MemoryKeyStore};
use pairly_plugins::battery::{BatteryPlugin, BatteryState};
use pairly_plugins::clipboard::ClipboardPlugin;
use pairly_plugins::command::CommandPlugin;
use pairly_plugins::contacts::{ContactsHost, ContactsPlugin};
use pairly_plugins::files::{FilesHost, FilesPlugin};
use pairly_plugins::findmy::FindMyPlugin;
use pairly_plugins::input::InputPlugin;
use pairly_plugins::media::MediaPlugin;
use pairly_plugins::notification::NotificationPlugin;
use pairly_plugins::ping::PingPlugin;
use pairly_plugins::share::SharePlugin;
use pairly_plugins::sms::SmsPlugin;
use pairly_plugins::telephony::TelephonyPlugin;
use pairly_transport_bt::BluetoothTransport;
use pairly_transport_lan::{LanConfig, LanTransport};
use pairly_transport_relay::{RelayConfig, RelayTransport};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{Notify, broadcast, mpsc};
use tokio::task::AbortHandle;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;
use zbus::Connection;
use zbus::object_server::SignalEmitter;

use crate::config::Config;
use crate::dbus::DaemonIface;
use crate::notifications::LinuxNotificationHost;
use crate::platform::{LinuxPlatform, PlatformEvent};
use crate::share::{LinuxShareHost, ShareService};
use crate::tray::PairlyTray;

/// The plugins the D-Bus interface and the tray act through.
#[derive(Clone)]
pub struct Features {
    pub notifications: Arc<NotificationPlugin>,
    pub clipboard: Arc<ClipboardPlugin>,
    pub battery: Arc<BatteryPlugin>,
    pub findmy: Arc<FindMyPlugin>,
    pub share: Arc<ShareService>,
    pub media: Arc<MediaPlugin>,
    pub media_host: Arc<media::LinuxMedia>,
    pub sms: Arc<SmsPlugin>,
    pub commands: Arc<commands::Commands>,
    pub command: Arc<CommandPlugin>,
    pub telephony: Arc<TelephonyPlugin>,
    pub contacts: Arc<ContactsPlugin>,
    pub power: Arc<pairly_plugins::power::PowerPlugin>,
    pub files: Arc<FilesPlugin>,
    pub screen_host: Arc<screen::LinuxScreen>,
}

/// The PC doesn't share its files; it only browses phones.
struct NoFiles;
impl FilesHost for NoFiles {}

/// The PC has no contacts of its own; it only asks phones for theirs.
struct NoContacts;
impl ContactsHost for NoContacts {}

#[derive(Parser)]
#[command(version, about = "Pairly daemon")]
struct Args {
    /// Config file (default: $XDG_CONFIG_HOME/pairly/config.toml).
    #[arg(long)]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("PAIRLY_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    let args = Args::parse();
    let config = Config::load(args.config.as_deref())?;
    info!(name = %config.name, data = %config.data_dir.display(), bus = %config.bus_name, "starting");
    private_dir(&config.data_dir);

    let (platform, platform_events) = LinuxPlatform::new();
    let mut node_config = NodeConfig::new(&config.name, config.device_type);
    node_config.relay.clone_from(&config.relay);
    node_config.relay_padding = config.relay_padding;
    match &config.relay {
        Some(relay) => {
            info!(relay = ?relay.parse::<pairly_transport_relay::RelayAddr>().ok(), "using a relay")
        }
        None => info!("no relay configured: devices are reachable on the local network only"),
    }
    let bluetooth = if config.bluetooth {
        match BluetoothTransport::new().await {
            Ok(bt) => {
                node_config.bluetooth = bt.address().await;
                info!(address = ?node_config.bluetooth, "Bluetooth available");
                Some(bt)
            }
            Err(e) => {
                info!(error = %e, "no Bluetooth");
                None
            }
        }
    } else {
        None
    };
    let qr_timeout = node_config.qr_timeout;
    let input_host = Arc::new(input::LinuxInput::new(
        config.input_backend,
        config.data_dir.clone(),
    ));
    let registry =
        Registry::open(&config.data_dir.join("registry.db")).context("opening registry")?;
    let identity = keyring::identity(&config.data_dir, registry.device_count()?).await?;
    let keystore = MemoryKeyStore::new();
    keystore.store(&identity)?;
    let mut builder = PairlyNode::builder(node_config)
        .keystore(Arc::new(keystore))
        .registry(registry)
        .platform(platform.clone())
        // A PC stays in its devices' relay rooms all the time, so a phone away from home can
        // always reach it.
        .transport(RelayTransport::new(RelayConfig {
            only_when_needed: false,
        }));
    if let Some(bt) = bluetooth {
        builder = builder.transport(bt);
    }
    if config.lan {
        builder = builder.transport(LanTransport::new(LanConfig {
            port: config.lan_port,
            mdns: config.mdns,
        }));
    } else {
        info!("LAN disabled in the config: reachable through the relay only");
    }
    let (notification_host, notification_cmds) = LinuxNotificationHost::new();
    let (share_host, share_events) = LinuxShareHost::new();
    let cache_dir = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cache")
        })
        .join("pairly");
    private_dir(&cache_dir);
    let (media_host, media_events) = media::LinuxMedia::new(cache_dir.clone());
    let (telephony_host, call_events) = telephony::LinuxTelephony::new();
    let (sms_host, sms_events) = sms::LinuxSms::new();
    let (commands, command_runs) = commands::Commands::load(&config.data_dir);
    let command = CommandPlugin::new(commands.clone());
    let telephony = TelephonyPlugin::new(telephony_host.clone());
    telephony_host.set_plugin(&telephony);
    let screen_host = screen::LinuxScreen::new();
    let screen_plugin = pairly_plugins::screen::ScreenPlugin::new(screen_host.clone());
    screen_host.set_plugin(&screen_plugin);
    commands.set_plugin(&command);
    let features = Features {
        notifications: NotificationPlugin::new(Arc::new(notification_host)),
        clipboard: ClipboardPlugin::new(platform.clone()),
        battery: BatteryPlugin::new(platform.clone()),
        findmy: FindMyPlugin::new(platform.clone()),
        share: ShareService::new(SharePlugin::new(Arc::new(share_host)), config.share.clone()),
        media: MediaPlugin::new(media_host.clone()),
        media_host,
        sms: SmsPlugin::new(Arc::new(sms_host)),
        commands: commands.clone(),
        command,
        telephony,
        contacts: ContactsPlugin::new(Arc::new(NoContacts)),
        power: pairly_plugins::power::PowerPlugin::new(Arc::new(power::LinuxPower {
            allowed: config.power_from_phone,
        })),
        files: FilesPlugin::new(Arc::new(NoFiles)),
        screen_host,
    };
    let node = builder
        .plugin(Arc::new(PingPlugin))
        .plugin(features.notifications.clone())
        .plugin(features.clipboard.clone())
        .plugin(features.battery.clone())
        .plugin(features.findmy.clone())
        .plugin(features.share.plugin.clone())
        .plugin(features.media.clone())
        .plugin(features.telephony.clone())
        .plugin(features.contacts.clone())
        .plugin(features.power.clone())
        .plugin(screen_plugin)
        .plugin(features.files.clone())
        .plugin(features.sms.clone())
        .plugin(features.command.clone())
        .plugin(InputPlugin::new(input_host.clone()))
        .start()
        .await
        .context("starting node")?;
    let events = node.subscribe();

    let notification_apps = Arc::new(notification_apps::AppFilter::load(&config.data_dir));
    let conn = zbus::connection::Builder::session()?
        .serve_at(
            pairly_dbus::OBJECT_PATH,
            DaemonIface {
                node: node.clone(),
                features: features.clone(),
                qr_timeout,
                data_dir: config.data_dir.clone(),
                cache_dir: cache_dir.clone(),
                notification_apps: notification_apps.clone(),
            },
        )?
        .name(config.bus_name.as_str())?
        .build()
        .await
        .with_context(|| {
            format!(
                "claiming bus name {} (is another pairlyd running?)",
                config.bus_name
            )
        })?;
    info!(id = %node.device_id(), "ready");

    let quit = Arc::new(Notify::new());
    let tray = if config.tray {
        PairlyTray::new(node.clone(), features.clone(), quit.clone())
            .spawn()
            .await
            .inspect_err(|e| warn!(error = %e, "no tray icon"))
            .ok()
    } else {
        None
    };
    features.screen_host.setup(
        config.screen_share,
        input_host.clone(),
        &config.data_dir,
        conn.clone(),
    );
    let forward = tokio::spawn(forward_events(conn.clone(), events, platform_events, tray));
    tokio::spawn(notifications::run(
        conn.clone(),
        features.notifications.clone(),
        notification_cmds,
        config.notifications.clone(),
        notification_apps.clone(),
    ));
    tokio::spawn(battery::watch(
        features.battery.clone(),
        platform.battery.clone(),
    ));
    tokio::spawn(telephony::run(
        conn.clone(),
        telephony_host,
        call_events,
        config.pause_media_for_calls,
    ));
    tokio::spawn(sms::run(conn.clone(), sms_events));
    tokio::spawn(commands::run(conn.clone(), commands, command_runs));
    tokio::spawn(media::run(
        conn.clone(),
        features.media_host.clone(),
        features.media.clone(),
        media_events,
    ));
    tokio::spawn(share::run(
        conn.clone(),
        features.share.clone(),
        share_events,
    ));
    if config.clipboard_auto {
        tokio::spawn(clipboard::watch(features.clipboard.clone()));
    }

    let mut term = signal(SignalKind::terminate())?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
        () = quit.notified() => info!("quit from the tray"),
    }
    info!("shutting down");
    forward.abort();
    node.shutdown().await;
    Ok(())
}

/// Turn node and platform events into D-Bus signals and desktop notifications.
async fn forward_events(
    conn: Connection,
    mut node_events: broadcast::Receiver<NodeEvent>,
    mut platform_events: mpsc::UnboundedReceiver<PlatformEvent>,
    tray: Option<ksni::Handle<PairlyTray>>,
) {
    let Ok(emitter) = SignalEmitter::new(&conn, pairly_dbus::OBJECT_PATH) else {
        return;
    };
    let mut ringing: Option<AbortHandle> = None;
    loop {
        let result = tokio::select! {
            event = node_events.recv() => match event {
                Ok(event) => {
                    if let Some(tray) = &tray {
                        tray.update(PairlyTray::refresh).await;
                    }
                    on_node_event(&conn, &emitter, event).await
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(n, "dropped node events");
                    Ok(())
                }
                Err(broadcast::error::RecvError::Closed) => return,
            },
            Some(event) = platform_events.recv() => {
                if let Some(tray) = &tray {
                    tray.update(PairlyTray::refresh).await;
                }
                on_platform_event(&conn, &emitter, event, &mut ringing).await
            }
        };
        if let Err(e) = result {
            warn!(error = %e, "failed to emit D-Bus signal");
        }
    }
}

async fn on_node_event(
    conn: &Connection,
    emitter: &SignalEmitter<'_>,
    event: NodeEvent,
) -> zbus::Result<()> {
    match event {
        NodeEvent::DeviceDiscovered { id, .. }
        | NodeEvent::DeviceLost { id }
        | NodeEvent::Connected { id, .. }
        | NodeEvent::Disconnected { id, .. }
        | NodeEvent::Unpaired { id } => DaemonIface::device_changed(emitter, &id.to_string()).await,
        NodeEvent::PairingRequested {
            id,
            name,
            code,
            incoming,
        } => {
            DaemonIface::pairing_requested(
                emitter,
                &id.to_string(),
                &name,
                &code.to_string(),
                incoming,
            )
            .await?;
            if incoming {
                let body =
                    format!("Code {code}. Accept only if the other device shows the same code.");
                notifications::show_simple(conn, format!("Pairing request from {name}"), body);
            }
            Ok(())
        }
        NodeEvent::Paired { id, name } => {
            DaemonIface::pairing_finished(emitter, &id.to_string(), true, "").await?;
            DaemonIface::device_changed(emitter, &id.to_string()).await?;
            notifications::show_simple(conn, format!("Paired with {name}"), "");
            Ok(())
        }
        NodeEvent::PairingFailed { id, reason } => {
            DaemonIface::pairing_finished(emitter, &id.to_string(), false, &reason).await
        }
    }
}

async fn on_platform_event(
    conn: &Connection,
    emitter: &SignalEmitter<'_>,
    event: PlatformEvent,
    ringing: &mut Option<AbortHandle>,
) -> zbus::Result<()> {
    match event {
        PlatformEvent::Ping {
            from,
            name,
            message,
        } => {
            let message = message.unwrap_or_default();
            DaemonIface::ping_received(emitter, &from.to_string(), &name, &message).await?;
            notifications::show_simple(conn, format!("Ping from {name}"), message);
            Ok(())
        }
        PlatformEvent::Battery {
            from,
            name,
            state,
            previous,
        } => {
            if is_low(state) && !previous.is_some_and(is_low) {
                notifications::show_simple(
                    conn,
                    format!("{name} battery low"),
                    format!("{}% left", state.percent),
                );
            }
            DaemonIface::device_changed(emitter, &from.to_string()).await
        }
        PlatformEvent::Ring { name, on } => {
            if let Some(task) = ringing.take() {
                task.abort();
            }
            if on {
                notifications::show_simple(
                    conn,
                    format!("{name} is looking for this PC"),
                    "Ringing for 30 seconds",
                );
                *ringing = Some(tokio::spawn(ring_for(RING_DURATION)).abort_handle());
            }
            Ok(())
        }
    }
}

const LOW_BATTERY: u8 = 15;
const RING_DURATION: std::time::Duration = std::time::Duration::from_secs(30);
const RING_SOUND: &str = "/usr/share/sounds/freedesktop/stereo/alarm-clock-elapsed.oga";

fn is_low(state: BatteryState) -> bool {
    state.percent <= LOW_BATTERY && !state.charging
}

/// The PC's own speakers: the first output that isn't Bluetooth. Finding a PC by sound only
/// works if the PC itself makes the noise, not earbuds that happen to be the default output.
async fn built_in_output() -> Option<String> {
    let out = tokio::process::Command::new("pw-dump")
        .output()
        .await
        .ok()?;
    let objects: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).ok()?;
    objects.iter().find_map(|o| {
        let props = o.get("info")?.get("props")?;
        let is_sink = props.get("media.class")?.as_str()? == "Audio/Sink";
        let bluetooth =
            props.get("device.api").and_then(serde_json::Value::as_str) == Some("bluez5");
        (is_sink && !bluetooth)
            .then(|| props.get("node.name")?.as_str().map(str::to_owned))
            .flatten()
    })
}

/// Find my PC: play the alarm sound on a loop, on the built-in speakers at full stream volume.
async fn ring_for(duration: std::time::Duration) {
    let target = built_in_output().await;
    info!(output = target.as_deref().unwrap_or("default"), "ringing");
    let until = tokio::time::Instant::now() + duration;
    while tokio::time::Instant::now() < until {
        let mut play = tokio::process::Command::new("pw-play");
        play.args(["--volume", "1.0"]);
        if let Some(target) = &target {
            play.args(["--target", target]);
        }
        let played = play.arg(RING_SOUND).kill_on_drop(true).status().await;
        if !played.is_ok_and(|s| s.success()) {
            warn!("can't play the ring sound (pw-play missing?)");
            return;
        }
    }
}

/// Our data (pairing secrets, contacts) and cache (message pictures) folders are readable only by
/// this user, whatever the home folder's permissions.
fn private_dir(dir: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let made = std::fs::create_dir_all(dir)
        .and_then(|()| std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)));
    if let Err(e) = made {
        warn!(dir = %dir.display(), error = %e, "can't make the folder private");
    }
}
