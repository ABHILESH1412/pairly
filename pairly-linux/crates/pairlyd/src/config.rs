//! `config.toml`, by default at `$XDG_CONFIG_HOME/pairly/config.toml`. Every key is optional.
//!
//! ```toml
//! name = "My Laptop"          # default: hostname
//! device_type = "laptop"      # default: laptop if a battery exists, else desktop
//! data_dir = "/path"          # default: $XDG_DATA_HOME/pairly
//! bus_name = "io.github.abhilesh1412.Pairly.Daemon"
//! tray = true                 # show a tray icon
//!
//! [lan]
//! port = 47100
//! mdns = true
//! enabled = true              # false: no LAN at all (only the relay)
//!
//! [clipboard]
//! auto = true                 # send this PC's clipboard on every copy
//!
//! [input]
//! backend = "auto"            # remote input: auto, wayland, portal or uinput
//!
//! [telephony]
//! pause_media = true          # pause this PC's music while the phone rings or is in a call
//!
//! [power]
//! from_phone = true           # paired phones may lock, power off or restart this PC
//!
//! [bluetooth]
//! enabled = true              # reach bonded devices over Bluetooth when there is no network
//!
//! [relay]
//! address = "pairly-relay://TOKEN@relay.example.com:47200/PIN"   # printed by pairly-relay
//! padding = true              # hide exact message sizes from the relay (256-byte steps)
//!
//! [share]
//! download_dir = "~/Downloads/Pairly"   # default: the XDG download folder
//! auto_accept = true          # take files from paired devices without asking
//! open_urls = true            # open received links right away (else: a notification)
//!
//! [notifications]
//! send = true                 # forward this PC's notifications
//! show = true                 # show notifications from paired devices
//! ignore_apps = ["Spotify"]   # app names never forwarded
//! reply = "auto"              # "inline", "dialog" or "auto" (inline when the server allows)
//! dismiss_on_phone = true     # closing a phone's notification here clears it on the phone
//! ```

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use pairly_core::DeviceType;
use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    name: Option<String>,
    device_type: Option<String>,
    data_dir: Option<PathBuf>,
    bus_name: Option<String>,
    tray: Option<bool>,
    #[serde(default)]
    lan: LanFile,
    #[serde(default)]
    notifications: NotificationsFile,
    #[serde(default)]
    clipboard: ClipboardFile,
    #[serde(default)]
    share: ShareFile,
    #[serde(default)]
    relay: RelayFile,
    #[serde(default)]
    bluetooth: BluetoothFile,
    #[serde(default)]
    telephony: TelephonyFile,
    #[serde(default)]
    input: InputFile,
    #[serde(default)]
    power: PowerFile,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct PowerFile {
    from_phone: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct InputFile {
    backend: Option<crate::input::BackendChoice>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct TelephonyFile {
    pause_media: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct BluetoothFile {
    enabled: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RelayFile {
    address: Option<String>,
    padding: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ShareFile {
    download_dir: Option<PathBuf>,
    auto_accept: Option<bool>,
    open_urls: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClipboardFile {
    auto: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct NotificationsFile {
    send: Option<bool>,
    show: Option<bool>,
    #[serde(default)]
    ignore_apps: Vec<String>,
    reply: Option<crate::notifications::ReplyMode>,
    dismiss_on_phone: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct LanFile {
    port: Option<u16>,
    mdns: Option<bool>,
    enabled: Option<bool>,
}

/// Fully resolved settings.
#[derive(Debug, Clone)]
pub struct Config {
    pub name: String,
    pub device_type: DeviceType,
    pub data_dir: PathBuf,
    pub bus_name: String,
    pub tray: bool,
    pub lan_port: u16,
    pub mdns: bool,
    pub lan: bool,
    pub notifications: crate::notifications::Settings,
    /// Send every copy automatically (otherwise only on request).
    pub clipboard_auto: bool,
    pub share: crate::share::Settings,
    /// The relay for reaching devices over the internet; paired phones learn it from us.
    pub relay: Option<String>,
    /// Pad what we send over the relay to 256-byte steps.
    pub relay_padding: bool,
    pub bluetooth: bool,
    pub pause_media_for_calls: bool,
    pub input_backend: crate::input::BackendChoice,
    /// Paired devices may lock, power off or restart this PC.
    pub power_from_phone: bool,
}

impl Config {
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let path = path.map_or_else(default_path, Path::to_path_buf);
        let file: File = match std::fs::read_to_string(&path) {
            Ok(text) => {
                toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => File::default(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let device_type = match file.device_type.as_deref() {
            None => detect_device_type(),
            Some(s) => match DeviceType::parse(s) {
                Some(t) => t,
                None => bail!("device_type must be desktop, laptop, phone or tablet, not {s:?}"),
            },
        };
        let relay = file
            .relay
            .address
            .map(|a| a.trim().to_owned())
            .filter(|a| !a.is_empty());
        if let Some(a) = &relay {
            a.parse::<pairly_transport_relay::RelayAddr>()
                .with_context(|| format!("[relay] address in {}", path.display()))?;
        }
        Ok(Self {
            relay,
            relay_padding: file.relay.padding.unwrap_or(true),
            bluetooth: file.bluetooth.enabled.unwrap_or(true),
            pause_media_for_calls: file.telephony.pause_media.unwrap_or(true),
            input_backend: file.input.backend.unwrap_or_default(),
            power_from_phone: file.power.from_phone.unwrap_or(true),
            name: file.name.unwrap_or_else(hostname),
            device_type,
            data_dir: file
                .data_dir
                .unwrap_or_else(|| xdg_dir("XDG_DATA_HOME", ".local/share").join("pairly")),
            bus_name: file
                .bus_name
                .unwrap_or_else(|| pairly_dbus::BUS_NAME.to_owned()),
            tray: file.tray.unwrap_or(true),
            lan_port: file.lan.port.unwrap_or(pairly_transport_lan::DEFAULT_PORT),
            mdns: file.lan.mdns.unwrap_or(true),
            lan: file.lan.enabled.unwrap_or(true),
            clipboard_auto: file.clipboard.auto.unwrap_or(true),
            share: crate::share::Settings {
                download_dir: file
                    .share
                    .download_dir
                    .map(|d| expand_home(&d))
                    .unwrap_or_else(download_dir),
                auto_accept: file.share.auto_accept.unwrap_or(true),
                open_urls: file.share.open_urls.unwrap_or(true),
            },
            notifications: crate::notifications::Settings {
                send: file.notifications.send.unwrap_or(true),
                show: file.notifications.show.unwrap_or(true),
                ignore_apps: file.notifications.ignore_apps,
                reply: file
                    .notifications
                    .reply
                    .unwrap_or(crate::notifications::ReplyMode::Auto),
                dismiss_on_phone: file.notifications.dismiss_on_phone.unwrap_or(true),
            },
        })
    }
}

fn default_path() -> PathBuf {
    xdg_dir("XDG_CONFIG_HOME", ".config").join("pairly/config.toml")
}

fn xdg_dir(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(fallback)
        })
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

fn expand_home(path: &Path) -> PathBuf {
    match path.strip_prefix("~") {
        Ok(rest) => home().join(rest),
        Err(_) => path.to_path_buf(),
    }
}

/// `XDG_DOWNLOAD_DIR` from `user-dirs.dirs`, else `~/Downloads`.
fn download_dir() -> PathBuf {
    let dirs = xdg_dir("XDG_CONFIG_HOME", ".config").join("user-dirs.dirs");
    std::fs::read_to_string(dirs)
        .ok()
        .and_then(|text| parse_download_dir(&text))
        .unwrap_or_else(|| home().join("Downloads"))
}

fn parse_download_dir(user_dirs: &str) -> Option<PathBuf> {
    let line = user_dirs
        .lines()
        .find_map(|l| l.trim().strip_prefix("XDG_DOWNLOAD_DIR="))?;
    let value = line.trim().trim_matches('"');
    let path = match value.strip_prefix("$HOME") {
        Some(rest) => home().join(rest.trim_start_matches('/')),
        None => PathBuf::from(value),
    };
    // A download dir equal to $HOME means "unset" in xdg-user-dirs.
    (path.is_absolute() && path != home()).then_some(path)
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|h| h.trim().to_owned())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "Linux".to_owned())
}

fn detect_device_type() -> DeviceType {
    let has_battery = std::fs::read_dir("/sys/class/power_supply")
        .map(|dir| {
            dir.flatten()
                .any(|e| e.file_name().to_string_lossy().starts_with("BAT"))
        })
        .unwrap_or(false);
    if has_battery {
        DeviceType::Laptop
    } else {
        DeviceType::Desktop
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_xdg_download_dir() {
        let home = home();
        let text =
            "# comment\nXDG_DESKTOP_DIR=\"$HOME/Desktop\"\nXDG_DOWNLOAD_DIR=\"$HOME/Fetched\"\n";
        assert_eq!(parse_download_dir(text), Some(home.join("Fetched")));
        assert_eq!(
            parse_download_dir("XDG_DOWNLOAD_DIR=\"/data/dl\""),
            Some(PathBuf::from("/data/dl"))
        );
        assert_eq!(parse_download_dir("XDG_DOWNLOAD_DIR=\"$HOME/\""), None);
        assert_eq!(parse_download_dir(""), None);
    }
}
