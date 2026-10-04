//! The Wayland clipboard through wl-clipboard (`wl-copy` / `wl-paste`), which works on every
//! compositor with the data-control protocol (Hyprland, Sway, KDE; not GNOME).

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use pairly_plugins::clipboard::ClipboardPlugin;
use tokio::io::{AsyncBufReadExt, BufReader};
use tracing::{debug, info, warn};

/// Password managers (KeePassXC and others) tag secrets with this type: never send them.
const SECRET_HINT: &str = "x-kde-passwordManagerHint";

/// `WAYLAND_DISPLAY`, or the first compositor socket in the runtime dir. A user service can
/// start before the compositor exports its environment to systemd.
fn wayland_display() -> Option<String> {
    if let Some(d) = std::env::var_os("WAYLAND_DISPLAY") {
        return Some(d.to_string_lossy().into_owned());
    }
    let dir = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR")?);
    let mut sockets: Vec<String> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("wayland-") && !n.ends_with(".lock"))
        .collect();
    sockets.sort();
    sockets.into_iter().next()
}

fn command(program: &str) -> Option<Command> {
    let mut cmd = Command::new(program);
    cmd.env("WAYLAND_DISPLAY", wayland_display()?);
    Some(cmd)
}

/// Put text on the clipboard. `wl-copy` forks and keeps serving it in the background.
pub fn set(text: &str) {
    let Some(mut cmd) = command("wl-copy") else {
        warn!("no Wayland display; can't set the clipboard");
        return;
    };
    let spawned = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    match spawned {
        Ok(mut child) => {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            std::thread::spawn(move || child.wait());
        }
        Err(e) => warn!(error = %e, "wl-copy failed (is wl-clipboard installed?)"),
    }
}

/// The clipboard's text, unless it's empty, not text, or marked as a secret.
pub async fn read() -> Option<String> {
    let types = tokio::process::Command::from(command("wl-paste")?)
        .arg("--list-types")
        .output()
        .await
        .ok()?;
    if !types.status.success() || String::from_utf8_lossy(&types.stdout).contains(SECRET_HINT) {
        return None;
    }
    let out = tokio::process::Command::from(command("wl-paste")?)
        .args(["--no-newline", "--type", "text"])
        .output()
        .await
        .ok()?;
    let text = String::from_utf8(out.stdout).ok()?;
    (out.status.success() && !text.is_empty()).then_some(text)
}

/// Send every clipboard change to connected devices (the plugin skips echoes and repeats).
pub async fn watch(plugin: Arc<ClipboardPlugin>) {
    loop {
        let Some(cmd) = command("wl-paste") else {
            debug!("no Wayland display yet");
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        };
        // `--watch echo` prints one line per selection change.
        let child = tokio::process::Command::from(cmd)
            .args(["--watch", "echo"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                warn!(error = %e, "can't watch the clipboard (is wl-clipboard installed?)");
                return;
            }
        };
        info!("watching the clipboard");
        if let Some(stdout) = child.stdout.take() {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(_)) = lines.next_line().await {
                if let Some(text) = read().await {
                    plugin.local_changed(&text);
                }
            }
        }
        let _ = child.wait().await;
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}
