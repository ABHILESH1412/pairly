//! The Wayland clipboard through wl-clipboard (`wl-copy` / `wl-paste`), which works on every
//! compositor with the data-control protocol (Hyprland, Sway, KDE; not GNOME). Text, and
//! images (a screenshot, an image copied in the browser, an image file copied in the file
//! manager).

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
pub fn wayland_display() -> Option<String> {
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

/// What was copied.
pub enum Content {
    Text(String),
    /// (MIME type, bytes).
    Image(String, Vec<u8>),
}

/// Image types sent, best first.
const IMAGE_TYPES: [&str; 5] = [
    "image/png",
    "image/jpeg",
    "image/webp",
    "image/gif",
    "image/bmp",
];

/// Put text on the clipboard. `wl-copy` forks and keeps serving it in the background.
pub fn set(text: &str) {
    copy(None, text.as_bytes());
}

/// Put an image on the clipboard.
pub fn set_image(mime: &str, data: &[u8]) {
    copy(Some(mime), data);
}

fn copy(mime: Option<&str>, data: &[u8]) {
    let Some(mut cmd) = command("wl-copy") else {
        warn!("no Wayland display; can't set the clipboard");
        return;
    };
    if let Some(mime) = mime {
        cmd.args(["--type", mime]);
    }
    let spawned = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    match spawned {
        Ok(mut child) => {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(data);
            }
            std::thread::spawn(move || child.wait());
        }
        Err(e) => warn!(error = %e, "wl-copy failed (is wl-clipboard installed?)"),
    }
}

/// What the clipboard holds: the picture when one image file was copied (file managers offer
/// its path as text too), else text when there is any (a spreadsheet's cells come with a
/// picture of them too), else an image. Nothing when it's empty or marked as a secret.
pub async fn read_any() -> Option<Content> {
    let types = tokio::process::Command::from(command("wl-paste")?)
        .arg("--list-types")
        .output()
        .await
        .ok()?;
    let types = String::from_utf8_lossy(&types.stdout).into_owned();
    if types.contains(SECRET_HINT) {
        return None;
    }
    let offered: Vec<&str> = types.lines().map(str::trim).collect();
    if offered.contains(&"text/uri-list")
        && let Some(image) = copied_image_file().await
    {
        return Some(image);
    }
    let text = offered
        .iter()
        .any(|t| t.starts_with("text/plain") || ["UTF8_STRING", "STRING", "TEXT"].contains(t));
    if !text && let Some(mime) = IMAGE_TYPES.iter().find(|m| offered.contains(m)) {
        let out = tokio::process::Command::from(command("wl-paste")?)
            .args(["--type", mime])
            .output()
            .await
            .ok()?;
        let fits = out.stdout.len() <= pairly_plugins::clipboard::MAX_IMAGE_BYTES;
        return (out.status.success() && !out.stdout.is_empty() && fits)
            .then(|| Content::Image((*mime).to_owned(), out.stdout));
    }
    read_text().await.map(Content::Text)
}

/// The picture in the one copied file, when it's an image.
async fn copied_image_file() -> Option<Content> {
    let out = tokio::process::Command::from(command("wl-paste")?)
        .args(["--no-newline", "--type", "text/uri-list"])
        .output()
        .await
        .ok()?;
    let list = String::from_utf8(out.stdout).ok()?;
    let mut uris = list
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'));
    let (Some(uri), None) = (uris.next(), uris.next()) else {
        return None;
    };
    let path = crate::media::file_url_path(uri)?;
    let size = tokio::fs::metadata(&path).await.ok()?.len();
    if !(1..=pairly_plugins::clipboard::MAX_IMAGE_BYTES as u64).contains(&size) {
        return None;
    }
    let data = tokio::fs::read(&path).await.ok()?;
    let mime = pairly_plugins::clipboard::image_type(&data)?;
    debug!(file = %path.display(), mime, "a copied image file");
    Some(Content::Image(mime.to_owned(), data))
}

async fn read_text() -> Option<String> {
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
            .stderr(Stdio::piped())
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
                match read_any().await {
                    Some(Content::Text(text)) => plugin.local_changed(&text),
                    Some(Content::Image(mime, data)) => plugin.local_image_changed(&mime, &data),
                    None => {}
                }
            }
        }
        let mut error = String::new();
        if let Some(mut stderr) = child.stderr.take() {
            use tokio::io::AsyncReadExt;
            let _ = stderr.read_to_string(&mut error).await;
        }
        let _ = child.wait().await;
        // The desktop doesn't let apps watch the clipboard (GNOME, or Hyprland and Sway for
        // a sandboxed Flatpak): trying again won't help.
        if error.contains("data-control") {
            warn!(
                "this desktop doesn't let Pairly watch the clipboard: send it with the \
                 Send Clipboard button instead"
            );
            return;
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}
