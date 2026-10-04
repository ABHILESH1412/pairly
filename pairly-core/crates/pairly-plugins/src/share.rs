//! File, link and text sharing.
//!
//! A file goes `share.offer` → `share.accept{offset}` → `share.chunk`s (paced by
//! `share.progress`) → `share.done{sha256}` → `share.finished`. Either side may send
//! `share.cancel` at any point.
//!
//! Chunks are unreliable bulk packets, so a transfer never holds up notifications and nothing
//! is buffered twice. The receiver only takes the chunk at the offset it expects. After a
//! reconnect or a transport switch it sends `share.accept` again with the bytes it already has,
//! and the sender continues from there. The SHA-256 of the whole file is checked at the end.
//!
//! File I/O runs on one OS thread per transfer, so the async runtime never waits on a disk.

use std::collections::HashMap;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use pairly_core::{
    CoreError, DeviceId, Envelope, OutboundPacket, PacketBody, PeerInfo, Plugin, PluginCtx,
    Priority, Result,
};
use ring::digest::{Context as Sha256, SHA256};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::clipboard::MAX_TEXT_BYTES;
use crate::peers::{Peers, lock};

/// Bytes per chunk: one chunk plus its envelope fits in a single encrypted frame.
pub const CHUNK_SIZE: usize = 63 * 1024;
/// How far the sender may run ahead of what the receiver has confirmed.
const WINDOW: u64 = 8 * 1024 * 1024;
/// The receiver confirms progress this often.
const PROGRESS_EVERY: u64 = 1024 * 1024;
/// Hosts hear about progress at most this often (state changes are reported right away).
const REPORT_INTERVAL: Duration = Duration::from_millis(250);
/// After a reconnect, the receiver has this long to ask for the rest of a file.
const RESUME_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_NAME_BYTES: usize = 255;
const MAX_MIME_BYTES: usize = 127;

// --- Packets ---

/// A link or a piece of text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareText {
    pub text: String,
    /// The text is a link to open rather than text to keep.
    pub url: bool,
}

impl PacketBody for ShareText {
    const TYPE: &'static str = "share.text";
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareOffer {
    /// Chosen by the sender; every later packet about this file carries it.
    pub transfer: u64,
    pub name: String,
    pub size: u64,
    pub mime: Option<String>,
}

impl PacketBody for ShareOffer {
    const TYPE: &'static str = "share.offer";
}

/// Receiver → sender: send from `offset` on. Sent once to start, and again after a reconnect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareAccept {
    pub transfer: u64,
    pub offset: u64,
}

impl PacketBody for ShareAccept {
    const TYPE: &'static str = "share.accept";
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareChunk {
    pub transfer: u64,
    pub offset: u64,
    #[serde(with = "serde_bytes")]
    pub data: Vec<u8>,
}

impl PacketBody for ShareChunk {
    const TYPE: &'static str = "share.chunk";
}

/// Receiver → sender: everything before `offset` is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareProgress {
    pub transfer: u64,
    pub offset: u64,
}

impl PacketBody for ShareProgress {
    const TYPE: &'static str = "share.progress";
}

/// Sender → receiver, once all bytes are confirmed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareDone {
    pub transfer: u64,
    #[serde(with = "serde_bytes")]
    pub sha256: Vec<u8>,
}

impl PacketBody for ShareDone {
    const TYPE: &'static str = "share.done";
}

/// Receiver → sender: the file is verified and saved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareFinished {
    pub transfer: u64,
}

impl PacketBody for ShareFinished {
    const TYPE: &'static str = "share.finished";
}

/// Either side: stop. `transfer` is always the sender's id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareCancel {
    pub transfer: u64,
    /// Why it failed; `None` when a person cancelled or declined.
    pub reason: Option<String>,
}

impl PacketBody for ShareCancel {
    const TYPE: &'static str = "share.cancel";
}

// --- Public types ---

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferState {
    /// Incoming: waiting for this device to accept. Outgoing: waiting for the peer.
    Waiting,
    Running,
    Done,
    Failed(String),
    Cancelled,
}

impl TransferState {
    pub fn is_finished(&self) -> bool {
        matches!(self, Self::Done | Self::Failed(_) | Self::Cancelled)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transfer {
    /// Local id (incoming transfers get their own; the sender's id stays internal).
    pub id: u64,
    pub peer: DeviceId,
    pub peer_name: String,
    pub incoming: bool,
    /// A plain file name: no directories, no leading dots.
    pub name: String,
    pub size: u64,
    pub mime: Option<String>,
    /// Bytes the receiver has confirmed.
    pub bytes: u64,
    pub state: TransferState,
}

pub trait ShareHost: Send + Sync + 'static {
    /// A peer offers a file. Answer with [`SharePlugin::accept`] (handing it a file to write
    /// into) or [`SharePlugin::cancel`], now or later.
    fn file_offered(&self, transfer: &Transfer);
    /// Progress (at most every 250 ms) or a state change. When an incoming transfer reaches
    /// `Done`, its file is complete, verified and closed: move it into place. After `Failed` or
    /// `Cancelled`, the partial file can be deleted.
    fn transfer_changed(&self, transfer: &Transfer);
    /// A link or text from `from`.
    fn text_received(&self, from: &PeerInfo, text: &str, url: bool);
}

/// Only `http(s)` links are opened automatically; anything else is treated as text.
pub fn is_web_url(text: &str) -> bool {
    let t = text.trim();
    let lower = t.get(..8).unwrap_or(t).to_ascii_lowercase();
    (lower.starts_with("https://") || lower.starts_with("http://"))
        && !t.chars().any(char::is_whitespace)
}

/// Make a peer-supplied name safe to create in a downloads folder.
pub fn safe_file_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let cleaned: String = base.chars().filter(|c| !c.is_control()).collect();
    // No hidden files, however the dots and spaces are mixed (". .x").
    let cleaned = cleaned
        .trim_start_matches(|c: char| c == '.' || c.is_whitespace())
        .trim_end();
    let mut out = String::new();
    for c in cleaned.chars() {
        if out.len() + c.len_utf8() > MAX_NAME_BYTES {
            break;
        }
        out.push(c);
    }
    if out.is_empty() { "file".into() } else { out }
}

// --- Plugin ---

pub struct SharePlugin {
    shared: Arc<Shared>,
}

struct Shared {
    host: Arc<dyn ShareHost>,
    peers: Peers,
    transfers: Mutex<HashMap<u64, Entry>>,
}

#[derive(Clone)]
enum Entry {
    In(Arc<Incoming>),
    Out(Arc<Outgoing>),
}

struct Incoming {
    /// The sender's id for this transfer.
    remote: u64,
    info: Mutex<Transfer>,
    /// Feeds the writer thread once accepted.
    writer: Mutex<Option<std_mpsc::Sender<Write>>>,
}

enum Write {
    Chunk(u64, Vec<u8>),
    Done(Vec<u8>),
}

struct Outgoing {
    state: Mutex<SendState>,
    wake: Condvar,
}

struct SendState {
    info: Transfer,
    /// Confirmed by the receiver.
    acked: u64,
    /// Set by `share.accept`: restart from here.
    resume: Option<u64>,
    /// The receiver accepted on the current link.
    ready: bool,
    /// Reconnected at this time, waiting for the receiver to resume.
    reconnected: Option<Instant>,
    stop: bool,
}

impl Outgoing {
    fn lock(&self) -> MutexGuard<'_, SendState> {
        lock(&self.state)
    }
}

impl SharePlugin {
    pub fn new(host: Arc<dyn ShareHost>) -> Arc<Self> {
        Arc::new(Self {
            shared: Arc::new(Shared {
                host,
                peers: Peers::default(),
                transfers: Mutex::default(),
            }),
        })
    }

    /// Send a link or text to a connected device.
    pub fn send_text(&self, peer: DeviceId, text: &str, url: bool) -> Result<()> {
        if text.is_empty() || text.len() > MAX_TEXT_BYTES {
            return Err(CoreError::Violation("text is empty or too large"));
        }
        let body = ShareText {
            text: text.to_owned(),
            url,
        };
        self.shared.peers.send(
            peer,
            OutboundPacket::reliable(&body, Priority::Interactive)?,
        )
    }

    /// Offer `file` to a connected device. Returns the transfer id. The file must support
    /// positioned reads (a regular file, not a pipe).
    pub fn send_file(
        &self,
        peer: DeviceId,
        file: File,
        name: &str,
        mime: Option<&str>,
    ) -> Result<u64> {
        let size = file.metadata()?.len();
        let id = rand::random();
        let name = safe_file_name(name);
        let mime = mime
            .filter(|m| m.len() <= MAX_MIME_BYTES)
            .map(str::to_owned);
        let offer = ShareOffer {
            transfer: id,
            name: name.clone(),
            size,
            mime: mime.clone(),
        };
        let packet = OutboundPacket::reliable(&offer, Priority::Control)?;
        let info = Transfer {
            id,
            peer,
            peer_name: self.shared.peers.name(peer).unwrap_or_default(),
            incoming: false,
            name,
            size,
            mime,
            bytes: 0,
            state: TransferState::Waiting,
        };
        let out = Arc::new(Outgoing {
            state: Mutex::new(SendState {
                info: info.clone(),
                acked: 0,
                resume: None,
                ready: false,
                reconnected: None,
                stop: false,
            }),
            wake: Condvar::new(),
        });
        // Registered before the offer goes out, so a quick accept finds it.
        lock(&self.shared.transfers).insert(id, Entry::Out(out.clone()));
        if let Err(e) = self.shared.peers.send(peer, packet) {
            lock(&self.shared.transfers).remove(&id);
            return Err(e);
        }
        info!(%peer, id, name = %info.name, size, "offering file");
        let shared = self.shared.clone();
        std::thread::Builder::new()
            .name("pairly-send".into())
            .spawn(move || send_loop(&shared, &out, &file))?;
        self.shared.host.transfer_changed(&info);
        Ok(id)
    }

    /// Accept an offered file, writing it into `file` (truncated first).
    pub fn accept(&self, id: u64, file: File) -> Result<()> {
        let Some(Entry::In(t)) = self.shared.get(id) else {
            return Err(CoreError::NotFound("incoming transfer"));
        };
        let info = {
            let mut info = lock(&t.info);
            if info.state != TransferState::Waiting {
                return Err(CoreError::Violation("transfer was already answered"));
            }
            info.state = TransferState::Running;
            info.clone()
        };
        file.set_len(0)?;
        let (tx, rx) = std_mpsc::channel();
        *lock(&t.writer) = Some(tx);
        let shared = self.shared.clone();
        let thread = t.clone();
        std::thread::Builder::new()
            .name("pairly-receive".into())
            .spawn(move || receive_loop(&shared, &thread, file, &rx))?;
        // If the peer is offline, `on_connected` asks again.
        let _ = self.shared.send_accept(&t, 0);
        self.shared.host.transfer_changed(&info);
        Ok(())
    }

    /// Cancel a transfer in either direction, or decline an offer.
    pub fn cancel(&self, id: u64) -> Result<()> {
        let entry = self.shared.get(id).ok_or(CoreError::NotFound("transfer"))?;
        let (peer, remote) = match &entry {
            Entry::In(t) => (lock(&t.info).peer, t.remote),
            Entry::Out(t) => (t.lock().info.peer, id),
        };
        let cancel = ShareCancel {
            transfer: remote,
            reason: None,
        };
        if let Ok(packet) = OutboundPacket::reliable(&cancel, Priority::Control) {
            let _ = self.shared.peers.send(peer, packet);
        }
        self.shared.finish(id, TransferState::Cancelled);
        Ok(())
    }

    /// Transfers that haven't finished yet.
    pub fn transfers(&self) -> Vec<Transfer> {
        lock(&self.shared.transfers)
            .values()
            .map(Entry::info)
            .collect()
    }
}

impl Drop for SharePlugin {
    fn drop(&mut self) {
        let ids: Vec<u64> = lock(&self.shared.transfers).keys().copied().collect();
        for id in ids {
            self.shared.finish(id, TransferState::Cancelled);
        }
    }
}

impl Entry {
    fn info(&self) -> Transfer {
        match self {
            Self::In(t) => lock(&t.info).clone(),
            Self::Out(t) => t.lock().info.clone(),
        }
    }
}

impl Shared {
    fn get(&self, id: u64) -> Option<Entry> {
        lock(&self.transfers).get(&id).cloned()
    }

    fn incoming(&self, peer: DeviceId, remote: u64) -> Option<(u64, Arc<Incoming>)> {
        lock(&self.transfers).iter().find_map(|(&id, e)| match e {
            Entry::In(t) if t.remote == remote && lock(&t.info).peer == peer => {
                Some((id, t.clone()))
            }
            _ => None,
        })
    }

    fn outgoing(&self, peer: DeviceId, id: u64) -> Option<Arc<Outgoing>> {
        match self.get(id)? {
            Entry::Out(t) if t.lock().info.peer == peer => Some(t),
            _ => None,
        }
    }

    fn send(&self, peer: DeviceId, packet: Result<OutboundPacket>) {
        if let Err(e) = packet.and_then(|p| self.peers.send(peer, p)) {
            debug!(%peer, error = %e, "share packet not sent");
        }
    }

    fn send_accept(&self, t: &Incoming, offset: u64) -> Result<()> {
        let peer = lock(&t.info).peer;
        let accept = ShareAccept {
            transfer: t.remote,
            offset,
        };
        self.peers
            .send(peer, OutboundPacket::reliable(&accept, Priority::Control)?)
    }

    fn send_cancel(&self, peer: DeviceId, transfer: u64, reason: &str) {
        let cancel = ShareCancel {
            transfer,
            reason: Some(reason.to_owned()),
        };
        self.send(peer, OutboundPacket::reliable(&cancel, Priority::Control));
    }

    /// End a transfer (once) and tell the host.
    fn finish(&self, id: u64, state: TransferState) {
        let Some(entry) = lock(&self.transfers).remove(&id) else {
            return;
        };
        let info = match entry {
            Entry::In(t) => {
                // Dropping the sender ends the writer thread (and closes its file).
                lock(&t.writer).take();
                let mut info = lock(&t.info);
                info.state = state;
                info.clone()
            }
            Entry::Out(t) => {
                let mut st = t.lock();
                st.stop = true;
                st.info.state = state;
                t.wake.notify_all();
                st.info.clone()
            }
        };
        match &info.state {
            TransferState::Failed(reason) => {
                warn!(peer = %info.peer, id, name = %info.name, %reason, "transfer failed");
            }
            state => info!(peer = %info.peer, id, name = %info.name, ?state, "transfer ended"),
        }
        self.host.transfer_changed(&info);
    }
}

/// Running SHA-256 over a file prefix; rewinds (rehashing from the start) when a resume moves
/// the position backwards.
struct PrefixHash {
    ctx: Sha256,
    pos: u64,
}

impl PrefixHash {
    fn new() -> Self {
        Self {
            ctx: Sha256::new(&SHA256),
            pos: 0,
        }
    }

    fn advance_to(&mut self, file: &File, pos: u64, buf: &mut [u8]) -> std::io::Result<()> {
        if pos < self.pos {
            *self = Self::new();
        }
        while self.pos < pos {
            let len = buf
                .len()
                .min(usize::try_from(pos - self.pos).unwrap_or(usize::MAX));
            file.read_exact_at(&mut buf[..len], self.pos)?;
            self.update(&buf[..len]);
        }
        Ok(())
    }

    fn update(&mut self, data: &[u8]) {
        self.ctx.update(data);
        self.pos += data.len() as u64;
    }
}

fn send_loop(shared: &Shared, t: &Outgoing, file: &File) {
    let (id, peer, size) = {
        let st = t.lock();
        (st.info.id, st.info.peer, st.info.size)
    };
    let mut hash = PrefixHash::new();
    let mut buf = vec![0; CHUNK_SIZE];
    let mut sent = 0u64;
    let mut done_sent = false;
    let mut reported = Instant::now();
    let result: std::result::Result<(), String> = loop {
        // Wait until there is something to do.
        let mut st = t.lock();
        let step = loop {
            if st.stop {
                return;
            }
            if let Some(offset) = st.resume.take() {
                sent = offset.min(size);
                st.acked = sent;
                st.ready = true;
                st.reconnected = None;
                done_sent = false;
            }
            sent = sent.max(st.acked);
            if st.ready && st.acked >= size && !done_sent {
                break Some(true);
            }
            if st.ready && sent < size && sent - st.acked < WINDOW {
                break Some(false);
            }
            // Reconnected, but the receiver never asked for the rest: it forgot the transfer
            // (it restarted, for example).
            if st
                .reconnected
                .is_some_and(|at| at.elapsed() > RESUME_TIMEOUT)
            {
                break None;
            }
            st = t
                .wake
                .wait_timeout(st, Duration::from_secs(1))
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        };
        let Some(all_acked) = step else {
            drop(st);
            break Err("the other device stopped receiving".into());
        };
        if reported.elapsed() >= REPORT_INTERVAL {
            reported = Instant::now();
            st.info.bytes = st.acked;
            let info = st.info.clone();
            drop(st);
            shared.host.transfer_changed(&info);
        } else {
            drop(st);
        }

        if all_acked {
            if let Err(e) = hash.advance_to(file, size, &mut buf) {
                break Err(format!("can't read the file: {e}"));
            }
            let done = ShareDone {
                transfer: id,
                sha256: hash.ctx.clone().finish().as_ref().to_vec(),
            };
            shared.send(peer, OutboundPacket::reliable(&done, Priority::Control));
            done_sent = true;
            continue;
        }

        let len = usize::try_from(size - sent)
            .unwrap_or(usize::MAX)
            .min(CHUNK_SIZE);
        let read = hash
            .advance_to(file, sent, &mut buf)
            .and_then(|()| file.read_exact_at(&mut buf[..len], sent));
        if let Err(e) = read {
            break Err(format!("can't read the file: {e}"));
        }
        hash.update(&buf[..len]);
        let chunk = ShareChunk {
            transfer: id,
            offset: sent,
            data: buf[..len].to_vec(),
        };
        match OutboundPacket::unreliable(&chunk, Priority::Bulk)
            .and_then(|p| shared.peers.send(peer, p))
        {
            Ok(()) => sent += len as u64,
            // Offline: wait for the receiver to resume.
            Err(_) => t.lock().ready = false,
        }
    };
    if let Err(reason) = result {
        shared.send_cancel(peer, id, &reason);
        shared.finish(id, TransferState::Failed(reason));
    }
}

fn receive_loop(shared: &Shared, t: &Incoming, file: File, rx: &std_mpsc::Receiver<Write>) {
    let (id, peer, size) = {
        let info = lock(&t.info);
        (info.id, info.peer, info.size)
    };
    let mut hash = PrefixHash::new();
    let mut confirmed = 0u64;
    let mut reported = Instant::now();
    let result: std::result::Result<(), String> = loop {
        // A closed channel means the transfer was cancelled; `finish` already reported it.
        let Ok(cmd) = rx.recv() else { return };
        match cmd {
            Write::Chunk(offset, data) => {
                let written = hash.pos;
                if offset != written || written + data.len() as u64 > size {
                    continue; // a duplicate after a resume, or out of order
                }
                if let Err(e) = file.write_all_at(&data, offset) {
                    break Err(format!("can't write the file: {e}"));
                }
                hash.update(&data);
                let written = hash.pos;
                if written - confirmed >= PROGRESS_EVERY || written == size {
                    confirmed = written;
                    let progress = ShareProgress {
                        transfer: t.remote,
                        offset: written,
                    };
                    shared.send(
                        peer,
                        OutboundPacket::unreliable(&progress, Priority::Control),
                    );
                }
                let info = {
                    let mut info = lock(&t.info);
                    info.bytes = written;
                    (reported.elapsed() >= REPORT_INTERVAL).then(|| info.clone())
                };
                if let Some(info) = info {
                    reported = Instant::now();
                    shared.host.transfer_changed(&info);
                }
            }
            Write::Done(sha256) => {
                if hash.pos != size {
                    continue;
                }
                if hash.ctx.clone().finish().as_ref() != sha256.as_slice() {
                    break Err("the file was corrupted on the way (checksum mismatch)".into());
                }
                if let Err(e) = file.sync_all() {
                    break Err(format!("can't save the file: {e}"));
                }
                break Ok(());
            }
        }
    };
    drop(file);
    match result {
        Ok(()) => {
            let finished = ShareFinished { transfer: t.remote };
            shared.send(peer, OutboundPacket::reliable(&finished, Priority::Control));
            shared.finish(id, TransferState::Done);
        }
        Err(reason) => {
            shared.send_cancel(peer, t.remote, &reason);
            shared.finish(id, TransferState::Failed(reason));
        }
    }
}

#[async_trait]
impl Plugin for SharePlugin {
    fn id(&self) -> &'static str {
        "share"
    }

    fn incoming(&self) -> &'static [&'static str] {
        &[
            ShareText::TYPE,
            ShareOffer::TYPE,
            ShareAccept::TYPE,
            ShareChunk::TYPE,
            ShareProgress::TYPE,
            ShareDone::TYPE,
            ShareFinished::TYPE,
            ShareCancel::TYPE,
        ]
    }

    fn outgoing(&self) -> &'static [&'static str] {
        self.incoming()
    }

    async fn on_connected(&self, ctx: &PluginCtx) {
        let shared = &self.shared;
        shared.peers.insert(ctx);
        let entries: Vec<Entry> = lock(&shared.transfers).values().cloned().collect();
        for entry in entries {
            match entry {
                // Ask for the rest of files we were receiving.
                Entry::In(t) => {
                    let (peer, state, bytes) = {
                        let info = lock(&t.info);
                        (info.peer, info.state.clone(), info.bytes)
                    };
                    if peer == ctx.peer() && state == TransferState::Running {
                        let _ = shared.send_accept(&t, bytes);
                    }
                }
                // Pause what we were sending until the receiver says where to continue.
                Entry::Out(t) => {
                    let mut st = t.lock();
                    if st.info.peer == ctx.peer() && st.info.state == TransferState::Running {
                        st.ready = false;
                        st.reconnected = Some(Instant::now());
                        t.wake.notify_all();
                    }
                }
            }
        }
    }

    async fn on_packet(&self, ctx: &PluginCtx, packet: Envelope) {
        if let Err(e) = self.handle(ctx, &packet) {
            debug!(peer = %ctx.peer(), ty = %packet.ty, error = %e, "bad share packet");
        }
    }

    async fn on_disconnected(&self, peer: DeviceId) {
        self.shared.peers.remove(peer);
        for entry in lock(&self.shared.transfers).values() {
            if let Entry::Out(t) = entry {
                let mut st = t.lock();
                if st.info.peer == peer {
                    st.ready = false;
                }
            }
        }
    }
}

impl SharePlugin {
    fn handle(&self, ctx: &PluginCtx, packet: &Envelope) -> Result<()> {
        let shared = &self.shared;
        let peer = ctx.peer();
        match packet.ty.as_str() {
            ShareText::TYPE => {
                let body: ShareText = packet.body()?;
                if !body.text.is_empty() && body.text.len() <= MAX_TEXT_BYTES {
                    let url = body.url && is_web_url(&body.text);
                    shared
                        .host
                        .text_received(&ctx.peer_info(), body.text.trim_end(), url);
                }
            }
            ShareOffer::TYPE => {
                let offer: ShareOffer = packet.body()?;
                if shared.incoming(peer, offer.transfer).is_some() {
                    return Ok(());
                }
                let info = Transfer {
                    id: rand::random(),
                    peer,
                    peer_name: ctx.peer_name().to_owned(),
                    incoming: true,
                    name: safe_file_name(&offer.name),
                    size: offer.size,
                    mime: offer.mime.filter(|m| m.len() <= MAX_MIME_BYTES),
                    bytes: 0,
                    state: TransferState::Waiting,
                };
                info!(%peer, id = info.id, name = %info.name, size = info.size, "file offered");
                let t = Arc::new(Incoming {
                    remote: offer.transfer,
                    info: Mutex::new(info.clone()),
                    writer: Mutex::default(),
                });
                lock(&shared.transfers).insert(info.id, Entry::In(t));
                shared.host.file_offered(&info);
            }
            ShareAccept::TYPE => {
                let accept: ShareAccept = packet.body()?;
                let Some(t) = shared.outgoing(peer, accept.transfer) else {
                    shared.send_cancel(peer, accept.transfer, "unknown transfer");
                    return Ok(());
                };
                let info = {
                    let mut st = t.lock();
                    st.resume = Some(accept.offset);
                    t.wake.notify_all();
                    (st.info.state == TransferState::Waiting).then(|| {
                        st.info.state = TransferState::Running;
                        st.info.clone()
                    })
                };
                if let Some(info) = info {
                    shared.host.transfer_changed(&info);
                }
            }
            ShareChunk::TYPE => {
                let chunk: ShareChunk = packet.body()?;
                if let Some((_, t)) = shared.incoming(peer, chunk.transfer)
                    && let Some(writer) = lock(&t.writer).as_ref()
                {
                    let _ = writer.send(Write::Chunk(chunk.offset, chunk.data));
                }
            }
            ShareProgress::TYPE => {
                let progress: ShareProgress = packet.body()?;
                if let Some(t) = shared.outgoing(peer, progress.transfer) {
                    let mut st = t.lock();
                    st.acked = st.acked.max(progress.offset.min(st.info.size));
                    t.wake.notify_all();
                }
            }
            ShareDone::TYPE => {
                let done: ShareDone = packet.body()?;
                match shared.incoming(peer, done.transfer) {
                    Some((_, t)) => {
                        if let Some(writer) = lock(&t.writer).as_ref() {
                            let _ = writer.send(Write::Done(done.sha256));
                        }
                    }
                    None => shared.send_cancel(peer, done.transfer, "unknown transfer"),
                }
            }
            ShareFinished::TYPE => {
                let finished: ShareFinished = packet.body()?;
                if let Some(t) = shared.outgoing(peer, finished.transfer) {
                    let id = {
                        let mut st = t.lock();
                        st.info.bytes = st.info.size;
                        st.info.id
                    };
                    shared.finish(id, TransferState::Done);
                }
            }
            ShareCancel::TYPE => {
                let cancel: ShareCancel = packet.body()?;
                let state = cancel
                    .reason
                    .map_or(TransferState::Cancelled, TransferState::Failed);
                if let Some(t) = shared.outgoing(peer, cancel.transfer) {
                    let id = t.lock().info.id;
                    shared.finish(id, state);
                } else if let Some((id, _)) = shared.incoming(peer, cancel.transfer) {
                    shared.finish(id, state);
                }
            }
            _ => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_are_made_safe() {
        assert_eq!(safe_file_name("photo.jpg"), "photo.jpg");
        assert_eq!(safe_file_name("../../.bashrc"), "bashrc");
        assert_eq!(safe_file_name("C:\\Users\\x\\evil.exe"), "evil.exe");
        assert_eq!(safe_file_name("a\nb\u{7}.txt"), "ab.txt");
        assert_eq!(safe_file_name(".."), "file");
        assert_eq!(safe_file_name(". .x"), "x");
        assert_eq!(safe_file_name(""), "file");
        assert!(safe_file_name(&"é".repeat(300)).len() <= MAX_NAME_BYTES);
    }

    #[test]
    fn only_web_links_open() {
        assert!(is_web_url("https://example.com/a?b=c"));
        assert!(is_web_url("HTTP://example.com"));
        assert!(!is_web_url("file:///etc/passwd"));
        assert!(!is_web_url("javascript:alert(1)"));
        assert!(!is_web_url("https://example.com and more"));
    }
}
