//! Tray icon (StatusNotifierItem) via ksni: connection state and quick actions. Shown by
//! Waybar's `tray` module, KDE Plasma, and GNOME with the AppIndicator extension.

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use ksni::menu::{StandardItem, SubMenu};
use ksni::{MenuItem, ToolTip, Tray};
use pairly_core::{DeviceInfo, PairlyNode};
use tokio::sync::Notify;
use tracing::warn;

use crate::Features;

pub struct PairlyTray {
    node: PairlyNode,
    features: Features,
    devices: Vec<DeviceInfo>,
    quit: Arc<Notify>,
}

impl PairlyTray {
    pub fn new(node: PairlyNode, features: Features, quit: Arc<Notify>) -> Self {
        let devices = node.devices().unwrap_or_default();
        Self {
            node,
            features,
            devices,
            quit,
        }
    }

    pub fn refresh(&mut self) {
        self.devices = self.node.devices().unwrap_or_default();
    }

    fn connected(&self) -> Vec<&str> {
        self.devices
            .iter()
            .filter(|d| d.paired && d.link.is_some())
            .map(|d| d.name.as_str())
            .collect()
    }
}

impl Tray for PairlyTray {
    fn id(&self) -> String {
        "io.github.abhilesh1412.Pairly".into()
    }

    fn title(&self) -> String {
        "Pairly".into()
    }

    fn icon_name(&self) -> String {
        "io.github.abhilesh1412.Pairly-symbolic".into()
    }

    fn tool_tip(&self) -> ToolTip {
        let connected = self.connected();
        let description = if connected.is_empty() {
            "No devices connected".to_owned()
        } else {
            format!("Connected to {}", connected.join(", "))
        };
        ToolTip {
            title: "Pairly".into(),
            description,
            ..Default::default()
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        open_ui(false);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let mut items: Vec<MenuItem<Self>> = vec![
            StandardItem {
                label: "Open Pairly".into(),
                activate: Box::new(|_: &mut Self| open_ui(false)),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Pair a New Device…".into(),
                activate: Box::new(|_: &mut Self| open_ui(true)),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
        ];
        let paired: Vec<&DeviceInfo> = self.devices.iter().filter(|d| d.paired).collect();
        if paired.is_empty() {
            items.push(
                StandardItem {
                    label: "No paired devices".into(),
                    enabled: false,
                    ..Default::default()
                }
                .into(),
            );
        }
        for d in paired {
            let (id, connected) = (d.id, d.link.is_some());
            let ping = StandardItem {
                label: "Send Ping".into(),
                enabled: connected,
                activate: Box::new(move |tray: &mut Self| {
                    if let Ok(packet) = pairly_plugins::ping::packet(None) {
                        let _ = tray.node.send(id, packet);
                    }
                }),
                ..Default::default()
            };
            let ring = StandardItem {
                label: "Ring".into(),
                enabled: connected,
                activate: Box::new(move |tray: &mut Self| {
                    let _ = tray.features.findmy.ring(id, true);
                }),
                ..Default::default()
            };
            let clip = StandardItem {
                label: "Send Clipboard".into(),
                enabled: connected,
                activate: Box::new(move |tray: &mut Self| {
                    let clipboard = tray.features.clipboard.clone();
                    tokio::spawn(async move {
                        if let Some(text) = crate::clipboard::read().await {
                            let _ = clipboard.send_to(id, &text);
                        }
                    });
                }),
                ..Default::default()
            };
            let files = StandardItem {
                label: "Send Files…".into(),
                enabled: connected,
                activate: Box::new(move |_: &mut Self| {
                    launch_ui(&["--send".into(), "--to".into(), id.to_string()]);
                }),
                ..Default::default()
            };
            let status = match (connected, self.features.battery.peer_state(id)) {
                (false, _) => "offline".to_owned(),
                (true, Some(b)) => format!("connected · {}%", b.percent),
                (true, None) => "connected".to_owned(),
            };
            items.push(
                SubMenu {
                    label: format!("{} — {status}", d.name),
                    submenu: vec![files.into(), clip.into(), ring.into(), ping.into()],
                    ..Default::default()
                }
                .into(),
            );
        }
        items.push(MenuItem::Separator);
        items.push(
            StandardItem {
                label: "Quit Pairly".into(),
                icon_name: "application-exit-symbolic".into(),
                activate: Box::new(|tray: &mut Self| tray.quit.notify_one()),
                ..Default::default()
            }
            .into(),
        );
        items
    }
}

/// Launch (or raise) the GTK app.
fn open_ui(pair: bool) {
    let args = if pair {
        vec!["--pair".into()]
    } else {
        Vec::new()
    };
    launch_ui(&args);
}

/// Run `pairly-gtk` with `args`, preferring the binary next to ours.
fn launch_ui(args: &[String]) {
    let exe = std::env::current_exe()
        .ok()
        .map(|p| p.with_file_name("pairly-gtk"))
        .filter(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from("pairly-gtk"));
    match Command::new(exe).args(args).spawn() {
        // Reap it when it exits so it doesn't linger as a zombie.
        Ok(mut child) => drop(std::thread::spawn(move || child.wait())),
        Err(e) => warn!(error = %e, "could not start pairly-gtk"),
    }
}
