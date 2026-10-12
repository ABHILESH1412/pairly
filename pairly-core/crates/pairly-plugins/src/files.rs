//! Browse another device's files: list folders, read and write files in chunks, delete, create
//! folders and rename. A phone shares its storage; a PC shares a folder its user picks, if they
//! turn sharing on, read-only unless they allow changes.
//!
//! The sharing side serves one root and refuses any path that would leave it: `..` components
//! are rejected and every path is resolved (symlinks included) and checked to still be inside
//! the root.

use crate::fileio::AtOffset;
use std::collections::HashMap;
use std::fs::{self, File};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, UNIX_EPOCH};

use async_trait::async_trait;
use pairly_core::{
    CoreError, DeviceId, Envelope, OutboundPacket, PacketBody, Plugin, PluginCtx, Priority, Result,
};
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;
use tracing::debug;

use crate::peers::{Peers, lock};

/// Bytes per read or write request.
pub const CHUNK: u32 = 512 * 1024;
const ANSWER_TIMEOUT: Duration = Duration::from_secs(30);
/// Reads queue behind each other on slow links (Bluetooth): more time for those.
const READ_TIMEOUT: Duration = Duration::from_secs(90);
/// Reads in flight during a download. Each one costs a round trip, which over the internet
/// relay is about half a second: one at a time would cap a download near 1 MB/s.
const READS_IN_FLIGHT: usize = 4;
const MAX_ENTRIES: usize = 10_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub name: String,
    pub dir: bool,
    pub size: u64,
    pub modified_ms: i64,
}

/// Paths are relative to the shared root, `/`-separated (`"DCIM/Camera"`; `""` is the root).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileOp {
    List {
        path: String,
    },
    Read {
        path: String,
        offset: u64,
        len: u32,
    },
    Write {
        path: String,
        offset: u64,
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
        /// Start a new file (the first chunk).
        create: bool,
    },
    Delete {
        path: String,
    },
    Mkdir {
        path: String,
    },
    Rename {
        from: String,
        to: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileAnswer {
    Entries(Vec<Entry>),
    Data(#[serde(with = "serde_bytes")] Vec<u8>),
    Done,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilesRequest {
    pub req: u64,
    pub op: FileOp,
}

impl PacketBody for FilesRequest {
    const TYPE: &'static str = "files.request";
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilesResponse {
    pub req: u64,
    pub answer: std::result::Result<FileAnswer, String>,
}

impl PacketBody for FilesResponse {
    const TYPE: &'static str = "files.response";
}

pub trait FilesHost: Send + Sync + 'static {
    /// The folder this device shares, or why it can't (no permission, nothing shared).
    fn root(&self) -> std::result::Result<PathBuf, String> {
        Err("this device doesn't share its files".into())
    }

    /// Whether paired devices may upload, rename and delete (else they can only look and
    /// download).
    fn allow_changes(&self) -> bool {
        true
    }

    /// Whether hidden files (`.name`, and on Windows hidden or system files) are shown.
    fn show_hidden(&self) -> bool {
        true
    }
}

/// What a request may do in a share.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rules {
    pub allow_changes: bool,
    pub show_hidden: bool,
}

impl Rules {
    pub const ALL: Self = Self {
        allow_changes: true,
        show_hidden: true,
    };

    fn of(host: &dyn FilesHost) -> Self {
        Self {
            allow_changes: host.allow_changes(),
            show_hidden: host.show_hidden(),
        }
    }
}

fn is_hidden_name(name: &str) -> bool {
    name.starts_with('.')
}

#[cfg(windows)]
fn is_hidden_entry(name: &str, meta: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const HIDDEN: u32 = 0x2;
    const SYSTEM: u32 = 0x4;
    is_hidden_name(name) || meta.file_attributes() & (HIDDEN | SYSTEM) != 0
}

#[cfg(not(windows))]
fn is_hidden_entry(name: &str, _: &fs::Metadata) -> bool {
    is_hidden_name(name)
}

type Answer = std::result::Result<FileAnswer, String>;

pub struct FilesPlugin {
    host: Arc<dyn FilesHost>,
    peers: Peers,
    next_req: AtomicU64,
    pending: std::sync::Mutex<HashMap<u64, oneshot::Sender<Answer>>>,
}

/// Resolve a peer-supplied relative path inside `root`, refusing anything that escapes it.
/// `must_exist`: the path itself must exist (else only its parent must).
pub fn resolve(root: &Path, rel: &str, must_exist: bool) -> std::result::Result<PathBuf, String> {
    let mut path = root.to_path_buf();
    for part in rel.split('/').filter(|p| !p.is_empty()) {
        match Path::new(part).components().next() {
            Some(Component::Normal(_)) if !part.contains('\\') => path.push(part),
            _ => return Err(format!("not allowed: {rel}")),
        }
    }
    let root = root
        .canonicalize()
        .map_err(|e| format!("shared folder unavailable: {e}"))?;
    let checked = if must_exist {
        path.canonicalize().map_err(|e| e.to_string())?
    } else {
        let parent = path.parent().ok_or("not allowed")?;
        let name = path.file_name().ok_or("not allowed")?;
        parent.canonicalize().map_err(|e| e.to_string())?.join(name)
    };
    if checked.starts_with(&root) {
        Ok(checked)
    } else {
        Err(format!("not allowed: {rel}"))
    }
}

/// Run a request as `rules` allow.
pub fn run(root: &Path, op: FileOp, rules: Rules) -> Answer {
    let err = |e: std::io::Error| e.to_string();
    let paths: Vec<&str> = match &op {
        FileOp::List { path }
        | FileOp::Read { path, .. }
        | FileOp::Write { path, .. }
        | FileOp::Delete { path }
        | FileOp::Mkdir { path } => vec![path],
        FileOp::Rename { from, to } => vec![from, to],
    };
    if !rules.show_hidden && paths.iter().any(|p| p.split('/').any(is_hidden_name)) {
        return Err("not allowed: hidden files aren't shared".into());
    }
    let changes = !matches!(op, FileOp::List { .. } | FileOp::Read { .. });
    if changes && !rules.allow_changes {
        return Err("this device only lets you look and download".into());
    }
    match op {
        FileOp::List { path } => {
            let dir = resolve(root, &path, true)?;
            let mut entries = Vec::new();
            for e in fs::read_dir(dir).map_err(err)?.flatten().take(MAX_ENTRIES) {
                let Ok(meta) = e.metadata() else { continue };
                let name = e.file_name().to_string_lossy().into_owned();
                if !rules.show_hidden && is_hidden_entry(&name, &meta) {
                    continue;
                }
                entries.push(Entry {
                    name,
                    dir: meta.is_dir(),
                    size: if meta.is_dir() { 0 } else { meta.len() },
                    modified_ms: meta
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(0)),
                });
            }
            entries.sort_by(|a, b| {
                b.dir
                    .cmp(&a.dir)
                    .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            });
            Ok(FileAnswer::Entries(entries))
        }
        FileOp::Read { path, offset, len } => {
            let file = File::open(resolve(root, &path, true)?).map_err(err)?;
            let mut buf = vec![0u8; len.min(CHUNK) as usize];
            let n = file.read_at(&mut buf, offset).map_err(err)?;
            buf.truncate(n);
            Ok(FileAnswer::Data(buf))
        }
        FileOp::Write {
            path,
            offset,
            data,
            create,
        } => {
            let target = resolve(root, &path, !create)?;
            let file = fs::OpenOptions::new()
                .write(true)
                .create(create)
                .truncate(create)
                .open(target)
                .map_err(err)?;
            file.write_all_at(&data, offset).map_err(err)?;
            Ok(FileAnswer::Done)
        }
        FileOp::Delete { path } => {
            if path.split('/').all(str::is_empty) {
                return Err("refusing to delete the whole storage".into());
            }
            let target = resolve(root, &path, true)?;
            if target.is_dir() {
                fs::remove_dir_all(target).map_err(err)?;
            } else {
                fs::remove_file(target).map_err(err)?;
            }
            Ok(FileAnswer::Done)
        }
        FileOp::Mkdir { path } => {
            fs::create_dir(resolve(root, &path, false)?).map_err(err)?;
            Ok(FileAnswer::Done)
        }
        FileOp::Rename { from, to } => {
            let from = resolve(root, &from, true)?;
            let to = resolve(root, &to, false)?;
            if to.exists() {
                return Err("something with that name already exists".into());
            }
            fs::rename(from, to).map_err(err)?;
            Ok(FileAnswer::Done)
        }
    }
}

impl FilesPlugin {
    pub fn new(host: Arc<dyn FilesHost>) -> Arc<Self> {
        Arc::new(Self {
            host,
            peers: Peers::default(),
            next_req: AtomicU64::new(1),
            pending: std::sync::Mutex::default(),
        })
    }

    /// Run `op` on a peer's files.
    pub async fn ask(&self, peer: DeviceId, op: FileOp) -> Result<FileAnswer> {
        let timeout = match op {
            FileOp::Read { .. } => READ_TIMEOUT,
            _ => ANSWER_TIMEOUT,
        };
        let req = self.next_req.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        lock(&self.pending).insert(req, tx);
        let priority = match op {
            FileOp::Read { .. } | FileOp::Write { .. } => Priority::Bulk,
            _ => Priority::Interactive,
        };
        let sent = OutboundPacket::reliable(&FilesRequest { req, op }, priority)
            .and_then(|p| self.peers.send(peer, p));
        if let Err(e) = sent {
            lock(&self.pending).remove(&req);
            return Err(e);
        }
        let answer = tokio::time::timeout(timeout, rx).await;
        lock(&self.pending).remove(&req);
        answer
            .map_err(|_| CoreError::Timeout)?
            .map_err(|_| CoreError::Closed)?
            .map_err(CoreError::Transport)
    }

    pub async fn list(&self, peer: DeviceId, path: &str) -> Result<Vec<Entry>> {
        match self
            .ask(
                peer,
                FileOp::List {
                    path: path.to_owned(),
                },
            )
            .await?
        {
            FileAnswer::Entries(e) => Ok(e),
            _ => Err(CoreError::Violation("wrong answer")),
        }
    }

    pub async fn read(&self, peer: DeviceId, path: &str, offset: u64, len: u32) -> Result<Vec<u8>> {
        let op = FileOp::Read {
            path: path.to_owned(),
            offset,
            len,
        };
        match self.ask(peer, op).await? {
            FileAnswer::Data(d) => Ok(d),
            _ => Err(CoreError::Violation("wrong answer")),
        }
    }

    /// Download a whole file (`size` as listed), handing each piece to `write` in order and the
    /// bytes so far to `progress`. Several pieces are requested at once. Returns the size.
    pub async fn download(
        &self,
        peer: DeviceId,
        path: &str,
        size: u64,
        mut write: impl FnMut(&[u8]) -> std::io::Result<()>,
        mut progress: impl FnMut(u64),
    ) -> Result<u64> {
        use futures::StreamExt;
        let chunk = u64::from(CHUNK);
        let pieces = size.div_ceil(chunk).max(1);
        let mut reads = futures::stream::iter(0..pieces)
            .map(|i| self.read(peer, path, i * chunk, CHUNK))
            .buffered(READS_IN_FLIGHT);
        let mut done = 0u64;
        let mut more = false;
        while let Some(data) = reads.next().await {
            let data = data?;
            write(&data)?;
            done += data.len() as u64;
            progress(done);
            more = data.len() as u64 == chunk;
            if !more {
                break;
            }
        }
        drop(reads);
        // The file grew since it was listed: read on to its end.
        while more {
            let data = self.read(peer, path, done, CHUNK).await?;
            write(&data)?;
            done += data.len() as u64;
            progress(done);
            more = data.len() as u64 == chunk;
        }
        Ok(done)
    }
}

#[async_trait]
impl Plugin for FilesPlugin {
    fn id(&self) -> &'static str {
        "files"
    }

    fn incoming(&self) -> &'static [&'static str] {
        &[FilesRequest::TYPE, FilesResponse::TYPE]
    }

    fn outgoing(&self) -> &'static [&'static str] {
        self.incoming()
    }

    async fn on_connected(&self, ctx: &PluginCtx) {
        self.peers.insert(ctx);
    }

    async fn on_packet(&self, ctx: &PluginCtx, packet: Envelope) {
        match packet.ty.as_str() {
            FilesRequest::TYPE => match packet.body::<FilesRequest>() {
                Ok(r) => {
                    let (host, ctx) = (self.host.clone(), ctx.clone());
                    tokio::task::spawn_blocking(move || {
                        let priority = match r.op {
                            FileOp::Read { .. } => Priority::Bulk,
                            _ => Priority::Interactive,
                        };
                        let rules = Rules::of(host.as_ref());
                        let answer = host.root().and_then(|root| run(&root, r.op, rules));
                        if let Err(e) = &answer {
                            debug!(error = %e, "file request refused");
                        }
                        let response = FilesResponse { req: r.req, answer };
                        if let Ok(p) = OutboundPacket::reliable(&response, priority) {
                            let _ = ctx.send(p);
                        }
                    });
                }
                Err(e) => debug!(peer = %ctx.peer(), error = %e, "bad files.request"),
            },
            FilesResponse::TYPE => match packet.body::<FilesResponse>() {
                Ok(r) => {
                    if let Some(tx) = lock(&self.pending).remove(&r.req) {
                        let _ = tx.send(r.answer);
                    }
                }
                Err(e) => debug!(peer = %ctx.peer(), error = %e, "bad files.response"),
            },
            _ => {}
        }
    }

    async fn on_disconnected(&self, peer: DeviceId) {
        self.peers.remove(peer);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    #[cfg(unix)] // makes a symlink
    fn paths_stay_inside_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("storage");
        fs::create_dir_all(root.join("DCIM")).unwrap();
        fs::write(dir.path().join("secret"), "x").unwrap();
        std::os::unix::fs::symlink(dir.path().join("secret"), root.join("link")).unwrap();

        assert!(resolve(&root, "DCIM", true).is_ok());
        assert!(resolve(&root, "", true).is_ok());
        assert!(resolve(&root, "../secret", true).is_err());
        assert!(resolve(&root, "DCIM/../../secret", false).is_err());
        assert!(
            resolve(&root, "link", true).is_err(),
            "symlink out of the root"
        );
        assert!(resolve(&root, "a\\..\\b", false).is_err());

        // Write, read, list, rename, delete.
        assert_eq!(
            run(
                &root,
                FileOp::Write {
                    path: "DCIM/a.txt".into(),
                    offset: 0,
                    data: b"hello".to_vec(),
                    create: true
                },
                Rules::ALL
            ),
            Ok(FileAnswer::Done)
        );
        assert_eq!(
            run(
                &root,
                FileOp::Write {
                    path: "DCIM/a.txt".into(),
                    offset: 5,
                    data: b" world".to_vec(),
                    create: false
                },
                Rules::ALL
            ),
            Ok(FileAnswer::Done)
        );
        assert_eq!(
            run(
                &root,
                FileOp::Read {
                    path: "DCIM/a.txt".into(),
                    offset: 6,
                    len: 100
                },
                Rules::ALL
            ),
            Ok(FileAnswer::Data(b"world".to_vec()))
        );
        let Ok(FileAnswer::Entries(e)) = run(
            &root,
            FileOp::List {
                path: "DCIM".into(),
            },
            Rules::ALL,
        ) else {
            panic!()
        };
        assert_eq!((e[0].name.as_str(), e[0].size), ("a.txt", 11));
        assert_eq!(
            run(
                &root,
                FileOp::Rename {
                    from: "DCIM/a.txt".into(),
                    to: "DCIM/b.txt".into()
                },
                Rules::ALL
            ),
            Ok(FileAnswer::Done)
        );
        assert_eq!(
            run(&root, FileOp::Mkdir { path: "New".into() }, Rules::ALL),
            Ok(FileAnswer::Done)
        );
        assert!(run(&root, FileOp::Delete { path: "".into() }, Rules::ALL).is_err());
        assert_eq!(
            run(
                &root,
                FileOp::Delete {
                    path: "DCIM/b.txt".into()
                },
                Rules::ALL
            ),
            Ok(FileAnswer::Done)
        );
    }

    #[test]
    fn read_only_shares_and_hidden_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".ssh")).unwrap();
        fs::write(root.join(".ssh/id"), "secret").unwrap();
        fs::write(root.join("notes.txt"), "hi").unwrap();
        let pc = Rules {
            allow_changes: false,
            show_hidden: false,
        };

        let Ok(FileAnswer::Entries(e)) = run(root, FileOp::List { path: "".into() }, pc) else {
            panic!()
        };
        let names: Vec<&str> = e.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["notes.txt"], "hidden entries aren't listed");
        let read = |path: &str| FileOp::Read {
            path: path.into(),
            offset: 0,
            len: 10,
        };
        assert_eq!(
            run(root, read("notes.txt"), pc),
            Ok(FileAnswer::Data(b"hi".to_vec()))
        );
        assert!(
            run(root, read(".ssh/id"), pc).is_err(),
            "hidden files can't be read"
        );
        assert!(
            run(
                root,
                FileOp::List {
                    path: ".ssh".into()
                },
                pc
            )
            .is_err()
        );

        // Read-only: no changes of any kind.
        for op in [
            FileOp::Write {
                path: "new.txt".into(),
                offset: 0,
                data: b"x".to_vec(),
                create: true,
            },
            FileOp::Delete {
                path: "notes.txt".into(),
            },
            FileOp::Mkdir { path: "d".into() },
            FileOp::Rename {
                from: "notes.txt".into(),
                to: "n.txt".into(),
            },
        ] {
            assert!(run(root, op, pc).is_err());
        }
        assert!(root.join("notes.txt").exists() && !root.join("new.txt").exists());

        // With changes allowed, hidden names still can't be created.
        let open = Rules {
            allow_changes: true,
            ..pc
        };
        let mkdir = |path: &str| FileOp::Mkdir { path: path.into() };
        assert!(run(root, mkdir(".config"), open).is_err());
        assert_eq!(run(root, mkdir("d"), open), Ok(FileAnswer::Done));
    }
}
