//! Running as a Flatpak: what changes.
//!
//! Inside the sandbox there's no `systemctl`, `loginctl` or the user's own programs, so power
//! actions and the phone's commands run on the real PC through Flatpak's `flatpak-spawn --host`.
//! There's no systemd service either: the desktop's Background portal starts the daemon at
//! login instead.

use std::collections::HashMap;
use std::path::Path;

use tracing::{debug, info};
use zbus::zvariant::Value;

/// Whether this copy runs in a Flatpak sandbox.
pub fn flatpak() -> bool {
    std::env::var_os("FLATPAK_ID").is_some() || Path::new("/.flatpak-info").exists()
}

/// `program` on the real PC: as is, or through `flatpak-spawn --host` in a sandbox.
pub fn host_command(program: &str) -> std::process::Command {
    if flatpak() {
        let mut cmd = std::process::Command::new("flatpak-spawn");
        cmd.args(["--host", program]);
        cmd
    } else {
        std::process::Command::new(program)
    }
}

/// The same, for async code.
pub fn host_tokio_command(program: &str) -> tokio::process::Command {
    if flatpak() {
        let mut cmd = tokio::process::Command::new("flatpak-spawn");
        cmd.args(["--host", program]);
        cmd
    } else {
        tokio::process::Command::new(program)
    }
}

/// In a sandbox, start the daemon again from the PC once this copy has gone: a fresh sandbox
/// runs what's installed now (after an update) and the new settings.
pub fn relaunch() -> bool {
    let Ok(id) = std::env::var("FLATPAK_ID") else {
        return false;
    };
    // A moment for this copy to exit and free the bus name.
    host_command("sh")
        .args([
            "-c",
            "sleep 2; exec flatpak run --command=pairlyd \"$0\"",
            &id,
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .is_ok()
}

/// In a sandbox, ask the desktop to start the daemon at login (or to stop doing so). The
/// desktop may ask the user once.
pub async fn autostart(conn: &zbus::Connection, on: bool) {
    if !flatpak() {
        return;
    }
    let mut options: HashMap<&str, Value<'_>> = HashMap::new();
    options.insert(
        "reason",
        Value::from("Stay connected to your phone in the background"),
    );
    options.insert("autostart", Value::from(on));
    options.insert("commandline", Value::from(vec!["pairlyd"]));
    let result = conn
        .call_method(
            Some("org.freedesktop.portal.Desktop"),
            "/org/freedesktop/portal/desktop",
            Some("org.freedesktop.portal.Background"),
            "RequestBackground",
            &("", options),
        )
        .await;
    match result {
        Ok(_) => info!(on, "asked the desktop to start Pairly at login"),
        Err(e) => debug!(error = %e, "the Background portal didn't answer"),
    }
}
