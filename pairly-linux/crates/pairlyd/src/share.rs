//! Files, links and text from and to paired devices.
//!
//! Received files are written to `<download dir>/<name>.part` and renamed once the checksum
//! verifies. Offers are accepted automatically, or with a notification's Accept button
//! (`[share] auto_accept = false`). Links open in the default browser; text goes to the
//! clipboard.

use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use futures_util::stream::{self, BoxStream};
use pairly_core::{DeviceId, PeerInfo};
use pairly_plugins::share::{ShareHost, SharePlugin, Transfer, TransferState};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use zbus::object_server::SignalEmitter;
use zbus::{Connection, Message};

use crate::dbus::DaemonIface;
use crate::notifications;

#[derive(Debug, Clone)]
pub struct Settings {
    pub download_dir: PathBuf,
    pub auto_accept: bool,
    pub open_urls: bool,
}

pub enum Event {
    Offered(Transfer),
    Changed(Transfer),
    Text {
        from: PeerInfo,
        text: String,
        url: bool,
    },
}

/// Plugin callbacks, handed to [`run`].
pub struct LinuxShareHost(mpsc::UnboundedSender<Event>);

impl LinuxShareHost {
    pub fn new() -> (Self, mpsc::UnboundedReceiver<Event>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self(tx), rx)
    }
}

impl ShareHost for LinuxShareHost {
    fn file_offered(&self, transfer: &Transfer) {
        let _ = self.0.send(Event::Offered(transfer.clone()));
    }
    fn transfer_changed(&self, transfer: &Transfer) {
        let _ = self.0.send(Event::Changed(transfer.clone()));
    }
    fn text_received(&self, from: &PeerInfo, text: &str, url: bool) {
        let _ = self.0.send(Event::Text {
            from: from.clone(),
            text: text.to_owned(),
            url,
        });
    }
}

/// What a notification's buttons act on.
enum Note {
    Offer(u64),
    Received(PathBuf),
    Link(String),
}

#[derive(Default)]
struct State {
    /// Accepted incoming transfers → their `.part` file.
    parts: HashMap<u64, PathBuf>,
    /// Notification server id → what it is about.
    notes: HashMap<u32, Note>,
    /// Offer notifications, to close once answered.
    offers: HashMap<u64, u32>,
}

/// Sending and receiving, shared by the D-Bus interface, the tray and [`run`].
pub struct ShareService {
    pub plugin: Arc<SharePlugin>,
    settings: Settings,
    state: Mutex<State>,
}

impl ShareService {
    pub fn new(plugin: Arc<SharePlugin>, settings: Settings) -> Arc<Self> {
        Arc::new(Self {
            plugin,
            settings,
            state: Mutex::default(),
        })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Offer files to `peer`. All paths are checked before anything is sent.
    pub fn send_files(&self, peer: DeviceId, paths: &[String]) -> Result<Vec<u64>> {
        let mut files = Vec::with_capacity(paths.len());
        for path in paths {
            let path = Path::new(path);
            if !path.is_absolute() {
                bail!("{} is not an absolute path", path.display());
            }
            let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
            if !file.metadata()?.is_file() {
                bail!(
                    "{} is not a file (folders can't be sent yet)",
                    path.display()
                );
            }
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            files.push((file, name));
        }
        files
            .into_iter()
            .map(|(file, name)| Ok(self.plugin.send_file(peer, file, &name, None)?))
            .collect()
    }

    /// Accept an offer into the download folder.
    pub fn accept(&self, id: u64) -> Result<()> {
        let transfer = self
            .plugin
            .transfers()
            .into_iter()
            .find(|t| t.id == id && t.incoming)
            .context("no such incoming transfer")?;
        let dir = &self.settings.download_dir;
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let (part, file) = create_part(dir, &transfer.name)?;
        if let Err(e) = self.plugin.accept(id, file) {
            let _ = std::fs::remove_file(&part);
            return Err(e.into());
        }
        info!(id, part = %part.display(), "receiving");
        self.lock().parts.insert(id, part);
        Ok(())
    }

    pub fn cancel(&self, id: u64) -> Result<()> {
        Ok(self.plugin.cancel(id)?)
    }

    /// Move a finished `.part` into place. Returns the final path.
    fn finalize(&self, t: &Transfer) -> Option<PathBuf> {
        let part = self.lock().parts.remove(&t.id)?;
        let target = unique_path(&self.settings.download_dir, &t.name);
        match std::fs::rename(&part, &target) {
            Ok(()) => Some(target),
            Err(e) => {
                warn!(part = %part.display(), error = %e, "can't move the received file");
                Some(part)
            }
        }
    }

    fn discard(&self, id: u64) {
        if let Some(part) = self.lock().parts.remove(&id) {
            let _ = std::fs::remove_file(part);
        }
    }
}

/// Create `<name>.part` (or `<name> (n).part`) without following or replacing anything.
fn create_part(dir: &Path, name: &str) -> Result<(PathBuf, File)> {
    for n in 0..1000 {
        let candidate = dir.join(format!("{}.part", numbered(name, n)));
        match File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => return Ok((candidate, file)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e).with_context(|| format!("creating {}", candidate.display())),
        }
    }
    bail!("too many files named {name} in {}", dir.display())
}

/// `photo.jpg`, then `photo (1).jpg`, `photo (2).jpg`...
fn numbered(name: &str, n: u32) -> String {
    if n == 0 {
        return name.to_owned();
    }
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => format!("{stem} ({n}).{ext}"),
        _ => format!("{name} ({n})"),
    }
}

fn unique_path(dir: &Path, name: &str) -> PathBuf {
    (0..1000)
        .map(|n| dir.join(numbered(name, n)))
        .find(|p| !p.exists())
        .unwrap_or_else(|| dir.join(name))
}

pub fn to_dbus(t: &Transfer, path: Option<&Path>) -> pairly_dbus::Transfer {
    let (state, error) = match &t.state {
        TransferState::Waiting => ("waiting", ""),
        TransferState::Running => ("running", ""),
        TransferState::Done => ("done", ""),
        TransferState::Failed(reason) => ("failed", reason.as_str()),
        TransferState::Cancelled => ("cancelled", ""),
    };
    pairly_dbus::Transfer {
        id: t.id,
        device: t.peer.to_string(),
        device_name: t.peer_name.clone(),
        incoming: t.incoming,
        name: t.name.clone(),
        size: t.size,
        bytes: t.bytes,
        state: state.to_owned(),
        error: error.to_owned(),
        path: path.map(|p| p.display().to_string()).unwrap_or_default(),
    }
}

/// `1.5 MB`-style sizes for notifications.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "kB", "MB", "GB", "TB"];
    #[allow(clippy::cast_precision_loss)] // display only
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Open a file, folder or link with the default application.
fn open(target: &str) {
    match tokio::process::Command::new("xdg-open").arg(target).spawn() {
        Ok(mut child) => drop(tokio::spawn(async move { child.wait().await })),
        Err(e) => warn!(error = %e, "can't run xdg-open"),
    }
}

/// Select the file in the file manager, or open its folder if none answers.
async fn show_in_folder(conn: &Connection, path: &Path) {
    let uri = file_uri(path);
    let shown = conn
        .call_method(
            Some("org.freedesktop.FileManager1"),
            "/org/freedesktop/FileManager1",
            Some("org.freedesktop.FileManager1"),
            "ShowItems",
            &(vec![uri.as_str()], ""),
        )
        .await;
    if let Err(e) = shown {
        debug!(error = %e, "no FileManager1; opening the folder");
        if let Some(dir) = path.parent() {
            open(&dir.to_string_lossy());
        }
    }
}

fn file_uri(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut uri = String::from("file://");
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"/-_.~".contains(&b) {
            uri.push(char::from(b));
        } else {
            uri.push_str(&format!("%{b:02X}"));
        }
    }
    uri
}

fn preview(text: &str) -> String {
    const MAX: usize = 200;
    match text.char_indices().nth(MAX) {
        Some((i, _)) => format!("{}…", &text[..i]),
        None => text.to_owned(),
    }
}

/// Handle plugin events and the notification buttons until the event channel closes.
pub async fn run(
    conn: Connection,
    service: Arc<ShareService>,
    mut events: mpsc::UnboundedReceiver<Event>,
) {
    let signals = async {
        let proxy = zbus::Proxy::new(
            &conn,
            "org.freedesktop.Notifications",
            "/org/freedesktop/Notifications",
            "org.freedesktop.Notifications",
        )
        .await?;
        let actions = proxy.receive_signal("ActionInvoked").await?;
        let closed = proxy.receive_signal("NotificationClosed").await?;
        Ok::<_, zbus::Error>((actions, closed))
    };
    let (mut actions, mut closed): (BoxStream<'static, Message>, BoxStream<'static, Message>) =
        match signals.await {
            Ok((actions, closed)) => (actions.boxed(), closed.boxed()),
            Err(e) => {
                warn!(error = %e, "can't listen to notification signals; no Accept buttons");
                (stream::pending().boxed(), stream::pending().boxed())
            }
        };
    let ui = Ui {
        markup: notifications::supports_markup(&conn).await,
        emitter: SignalEmitter::new(&conn, pairly_dbus::OBJECT_PATH).ok(),
        conn: conn.clone(),
    };
    loop {
        tokio::select! {
            event = events.recv() => match event {
                Some(event) => ui.on_event(&service, event).await,
                None => return,
            },
            Some(m) = actions.next() => {
                if let Ok((id, key)) = m.body().deserialize::<(u32, String)>() {
                    ui.on_action(&service, id, &key).await;
                }
            }
            Some(m) = closed.next() => {
                if let Ok((id, _reason)) = m.body().deserialize::<(u32, u32)>() {
                    service.lock().notes.remove(&id);
                }
            }
        }
    }
}

struct Ui {
    conn: Connection,
    emitter: Option<SignalEmitter<'static>>,
    markup: bool,
}

impl Ui {
    fn text(&self, s: &str) -> String {
        if self.markup {
            notifications::escape_markup(s)
        } else {
            s.to_owned()
        }
    }

    async fn notify(&self, summary: &str, body: &str, actions: &[(&str, &str)]) -> Option<u32> {
        notifications::notify(
            &self.conn,
            summary,
            &self.text(body),
            actions,
            !actions.is_empty(),
        )
        .await
    }

    async fn signal(&self, t: &Transfer, path: Option<&Path>) {
        if let Some(emitter) = &self.emitter
            && let Err(e) = DaemonIface::transfer_changed(emitter, to_dbus(t, path)).await
        {
            debug!(error = %e, "can't emit TransferChanged");
        }
    }

    async fn on_event(&self, service: &ShareService, event: Event) {
        match event {
            Event::Offered(t) => {
                self.signal(&t, None).await;
                if service.settings.auto_accept {
                    if let Err(e) = service.accept(t.id) {
                        warn!(error = %e, "can't accept the file");
                        let _ = service.cancel(t.id);
                    }
                    return;
                }
                let summary = format!("{} wants to send you a file", t.peer_name);
                let body = format!("{} ({})", t.name, human_size(t.size));
                let actions = [("accept", "Accept"), ("decline", "Decline")];
                if let Some(id) = self.notify(&summary, &body, &actions).await {
                    let mut st = service.lock();
                    st.notes.insert(id, Note::Offer(t.id));
                    st.offers.insert(t.id, id);
                }
            }
            Event::Changed(t) => self.on_changed(service, &t).await,
            Event::Text { from, text, url } => {
                if url && service.settings.open_urls {
                    info!(from = %from.id, "opening a link");
                    open(&text);
                    let _ = self
                        .notify(&format!("Opened a link from {}", from.name), &text, &[])
                        .await;
                } else if url {
                    let summary = format!("Link from {}", from.name);
                    if let Some(id) = self.notify(&summary, &text, &[("open", "Open")]).await {
                        service.lock().notes.insert(id, Note::Link(text));
                    }
                } else {
                    crate::clipboard::set(&text);
                    let summary = format!("Text from {} copied", from.name);
                    let _ = self.notify(&summary, &preview(&text), &[]).await;
                }
            }
        }
    }

    async fn on_changed(&self, service: &ShareService, t: &Transfer) {
        if t.state != TransferState::Waiting {
            let offer = service.lock().offers.remove(&t.id);
            if let Some(server_id) = offer {
                service.lock().notes.remove(&server_id);
                notifications::close(&self.conn, server_id).await;
            }
        }
        let path = (t.incoming && t.state == TransferState::Done)
            .then(|| service.finalize(t))
            .flatten();
        if t.incoming && matches!(t.state, TransferState::Failed(_) | TransferState::Cancelled) {
            service.discard(t.id);
        }
        self.signal(t, path.as_deref()).await;

        match (&t.state, t.incoming) {
            (TransferState::Done, true) => {
                let summary = format!("Received {}", t.name);
                let body = format!("From {} · {}", t.peer_name, human_size(t.size));
                let actions = [("open", "Open"), ("folder", "Show in Folder")];
                if let Some(path) = path
                    && let Some(id) = self.notify(&summary, &body, &actions).await
                {
                    service.lock().notes.insert(id, Note::Received(path));
                }
            }
            (TransferState::Done, false) => {
                let summary = format!("Sent {} to {}", t.name, t.peer_name);
                let _ = self.notify(&summary, &human_size(t.size), &[]).await;
            }
            (TransferState::Failed(reason), incoming) => {
                let summary = if incoming {
                    format!("Couldn't receive {}", t.name)
                } else {
                    format!("Couldn't send {} to {}", t.name, t.peer_name)
                };
                let _ = self.notify(&summary, reason, &[]).await;
            }
            _ => {}
        }
    }

    async fn on_action(&self, service: &ShareService, server_id: u32, key: &str) {
        let Some(note) = service.lock().notes.remove(&server_id) else {
            return;
        };
        match (note, key) {
            (Note::Offer(id), "accept") => {
                if let Err(e) = service.accept(id) {
                    warn!(error = %e, "can't accept the file");
                    let _ = self
                        .notify("Couldn't accept the file", &format!("{e:#}"), &[])
                        .await;
                }
            }
            (Note::Offer(id), "decline") => {
                let _ = service.cancel(id);
            }
            (Note::Received(path), "open") => open(&path.to_string_lossy()),
            (Note::Received(path), "folder") => show_in_folder(&self.conn, &path).await,
            (Note::Link(url), "open") => open(&url),
            (note, _) => {
                // Clicking the notification body ("default"): keep the buttons usable.
                service.lock().notes.insert(server_id, note);
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn numbering_keeps_the_extension() {
        assert_eq!(numbered("photo.jpg", 0), "photo.jpg");
        assert_eq!(numbered("photo.jpg", 2), "photo (2).jpg");
        assert_eq!(numbered("archive.tar.gz", 1), "archive.tar (1).gz");
        assert_eq!(numbered("README", 1), "README (1)");
        assert_eq!(numbered("bashrc", 1), "bashrc (1)");
    }

    #[test]
    fn parts_never_replace_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt.part"), "keep").unwrap();
        let (part, _) = create_part(dir.path(), "a.txt").unwrap();
        assert_eq!(part, dir.path().join("a (1).txt.part"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt.part")).unwrap(),
            "keep"
        );
        std::fs::write(dir.path().join("a.txt"), "old").unwrap();
        assert_eq!(
            unique_path(dir.path(), "a.txt"),
            dir.path().join("a (1).txt")
        );
    }

    #[test]
    fn sizes_and_uris() {
        assert_eq!(human_size(999), "999 B");
        assert_eq!(human_size(1_500_000), "1.5 MB");
        assert_eq!(
            file_uri(Path::new("/home/u/My File#1.txt")),
            "file:///home/u/My%20File%231.txt"
        );
    }
}
