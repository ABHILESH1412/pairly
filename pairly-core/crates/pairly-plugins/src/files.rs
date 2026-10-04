//! Browse a phone's storage from the PC: list folders, read and write files in chunks, delete,
//! create folders and rename.
//!
//! The phone side serves one root (its shared storage) and refuses any path that would leave
//! it: `..` components are rejected and every path is resolved (symlinks included) and checked
//! to still be inside the root.

use std::collections::HashMap;
use std::fs::{self, File};
use std::os::unix::fs::FileExt;
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

fn run(root: &Path, op: FileOp) -> Answer {
    let err = |e: std::io::Error| e.to_string();
    match op {
        FileOp::List { path } => {
            let dir = resolve(root, &path, true)?;
            let mut entries = Vec::new();
            for e in fs::read_dir(dir).map_err(err)?.flatten().take(MAX_ENTRIES) {
                let Ok(meta) = e.metadata() else { continue };
                entries.push(Entry {
                    name: e.file_name().to_string_lossy().into_owned(),
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
        let answer = tokio::time::timeout(ANSWER_TIMEOUT, rx).await;
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
                        let answer = host.root().and_then(|root| run(&root, r.op));
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
                }
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
                }
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
                }
            ),
            Ok(FileAnswer::Data(b"world".to_vec()))
        );
        let Ok(FileAnswer::Entries(e)) = run(
            &root,
            FileOp::List {
                path: "DCIM".into(),
            },
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
                }
            ),
            Ok(FileAnswer::Done)
        );
        assert_eq!(
            run(&root, FileOp::Mkdir { path: "New".into() }),
            Ok(FileAnswer::Done)
        );
        assert!(run(&root, FileOp::Delete { path: "".into() }).is_err());
        assert_eq!(
            run(
                &root,
                FileOp::Delete {
                    path: "DCIM/b.txt".into()
                }
            ),
            Ok(FileAnswer::Done)
        );
    }
}
