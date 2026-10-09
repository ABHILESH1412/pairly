//! The graphical session's environment (where the display is, which compositor), for the
//! windows and programs the daemon starts.
//!
//! The daemon usually starts at login before the desktop has told systemd where its display
//! is, so its own environment lacks `WAYLAND_DISPLAY` and the rest: a window started with it
//! fails ("Failed to initialize GTK"). So every launch asks systemd for the session's current
//! values instead, and falls back to finding the Wayland socket.

use std::process::Command;

/// What a window or a desktop program needs to find the display and the compositor.
const KEYS: &[&str] = &[
    "WAYLAND_DISPLAY",
    "DISPLAY",
    "XAUTHORITY",
    "HYPRLAND_INSTANCE_SIGNATURE",
    "SWAYSOCK",
    "NIRI_SOCKET",
    "XDG_CURRENT_DESKTOP",
    "XDG_SESSION_DESKTOP",
    "XDG_SESSION_TYPE",
];

/// The session's display variables, as systemd knows them now.
pub fn vars() -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    if let Ok(o) = Command::new("systemctl")
        .args(["--user", "show-environment"])
        .output()
        && o.status.success()
    {
        for line in String::from_utf8_lossy(&o.stdout).lines() {
            if let Some((key, value)) = line.split_once('=')
                && KEYS.contains(&key)
                // Values with special characters come quoted ($'…'); these never need them.
                && !value.starts_with("$'")
            {
                out.push((key.to_owned(), value.to_owned()));
            }
        }
    }
    let has = |key: &str| out.iter().any(|(k, _)| k == key) || std::env::var_os(key).is_some();
    if !has("WAYLAND_DISPLAY")
        && let Some(display) = crate::clipboard::wayland_display()
    {
        out.push(("WAYLAND_DISPLAY".into(), display));
    }
    out
}

/// A command for `program` that will find the display.
pub fn command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut cmd = Command::new(program);
    apply(&mut cmd);
    cmd
}

/// `cmd` with the session's display variables set (overriding the daemon's stale ones).
pub fn apply(cmd: &mut Command) -> &mut Command {
    cmd.envs(vars())
}
