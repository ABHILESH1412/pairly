//! Text messages: the PC asks the phone for conversations and messages (paged), sends texts
//! through it, and hears about new ones as they arrive.
//!
//! Requests carry an id that the phone echoes, so the PC side can `await` the answer.
//!
//! Picture and group messages (MMS) carry their participants and attachments; attachment
//! bytes are fetched separately, in chunks.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use pairly_core::{
    CoreError, DeviceId, Envelope, OutboundPacket, PacketBody, PeerInfo, Plugin, PluginCtx,
    Priority, Result,
};
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;
use tracing::debug;

use crate::peers::{Peers, lock};

const ANSWER_TIMEOUT: Duration = Duration::from_secs(15);
pub const MAX_CONVERSATIONS: usize = 200;
pub const MAX_PAGE: u32 = 100;
/// Attachment bytes per request (and the most a sent attachment may be).
pub const ATTACHMENT_CHUNK: u32 = 768 * 1024;
pub const MAX_SEND_ATTACHMENTS: usize = 5;
const MAX_BODY: usize = 8 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conversation {
    pub thread_id: i64,
    /// Phone numbers in the conversation.
    pub addresses: Vec<String>,
    /// Contact names, parallel to `addresses` (empty string if unknown).
    pub names: Vec<String>,
    /// The latest message.
    pub snippet: String,
    pub date_ms: i64,
    pub read: bool,
}

/// A picture, video or other file in an MMS.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    /// Phone-side id to fetch the bytes with.
    pub part_id: i64,
    pub mime: String,
    pub name: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub id: i64,
    pub thread_id: i64,
    /// The sender (incoming) or, for a one-to-one message, the recipient.
    pub address: String,
    pub body: String,
    pub date_ms: i64,
    /// Sent by the phone's owner.
    pub outgoing: bool,
    pub read: bool,
    /// Everyone in a group message (empty for plain texts).
    #[serde(default)]
    pub participants: Vec<String>,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
}

/// A file to send with a text (which makes it an MMS).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutgoingAttachment {
    pub mime: String,
    pub name: String,
    #[serde(with = "serde_bytes")]
    pub data: Vec<u8>,
}

impl Message {
    fn clamp(mut self) -> Self {
        if self.body.len() > MAX_BODY {
            let mut end = MAX_BODY;
            while !self.body.is_char_boundary(end) {
                end -= 1;
            }
            self.body.truncate(end);
        }
        self
    }
}

/// PC → phone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SmsQuery {
    Conversations,
    Messages {
        thread_id: i64,
        /// Older than this (for paging); `None`: the newest.
        before_ms: Option<i64>,
        limit: u32,
    },
    Attachment {
        part_id: i64,
        offset: u64,
        len: u32,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SmsRequest {
    pub req: u64,
    pub query: SmsQuery,
}

impl PacketBody for SmsRequest {
    const TYPE: &'static str = "sms.request";
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SmsAnswer {
    Conversations(Vec<Conversation>),
    Messages(Vec<Message>),
    Data(#[serde(with = "serde_bytes")] Vec<u8>),
    /// No permission, or the query failed.
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SmsResponse {
    pub req: u64,
    pub answer: SmsAnswer,
}

impl PacketBody for SmsResponse {
    const TYPE: &'static str = "sms.response";
}

/// PC → phone: send a text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SmsSend {
    pub addresses: Vec<String>,
    pub text: String,
    /// With attachments, or to several people, the phone sends an MMS.
    #[serde(default)]
    pub attachments: Vec<OutgoingAttachment>,
}

impl PacketBody for SmsSend {
    const TYPE: &'static str = "sms.send";
}

/// Phone → PC: how sending went (only failures, and picture messages once the carrier
/// accepted them, are reported).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SmsStatus {
    pub ok: bool,
    pub detail: String,
}

impl PacketBody for SmsStatus {
    const TYPE: &'static str = "sms.status";
}

/// Phone → PC: a new message (received, or sent from the phone).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SmsNew {
    pub message: Message,
    pub name: Option<String>,
}

impl PacketBody for SmsNew {
    const TYPE: &'static str = "sms.new";
}

/// The phone answers; the PC hears about new messages. Defaults do nothing, so each side only
/// implements its half.
pub trait SmsHost: Send + Sync + 'static {
    fn conversations(&self) -> std::result::Result<Vec<Conversation>, String> {
        Err("this device has no messages".into())
    }
    fn messages(
        &self,
        _thread_id: i64,
        _before_ms: Option<i64>,
        _limit: u32,
    ) -> std::result::Result<Vec<Message>, String> {
        Err("this device has no messages".into())
    }
    /// Bytes of an MMS attachment, from `offset`, at most `len`.
    fn attachment(
        &self,
        _part_id: i64,
        _offset: u64,
        _len: u32,
    ) -> std::result::Result<Vec<u8>, String> {
        Err("this device has no messages".into())
    }
    fn send(
        &self,
        _addresses: &[String],
        _text: &str,
        _attachments: &[OutgoingAttachment],
    ) -> std::result::Result<(), String> {
        Err("this device can't send texts".into())
    }
    fn received(&self, _from: &PeerInfo, _message: &Message, _name: Option<&str>) {}
    /// How a send went (see [`SmsStatus`]).
    fn status(&self, _from: &PeerInfo, _ok: bool, _detail: &str) {}
}

pub struct SmsPlugin {
    host: Arc<dyn SmsHost>,
    peers: Peers,
    next_req: AtomicU64,
    pending: std::sync::Mutex<HashMap<u64, oneshot::Sender<SmsAnswer>>>,
}

impl SmsPlugin {
    pub fn new(host: Arc<dyn SmsHost>) -> Arc<Self> {
        Arc::new(Self {
            host,
            peers: Peers::default(),
            next_req: AtomicU64::new(1),
            pending: std::sync::Mutex::default(),
        })
    }

    async fn ask(&self, peer: DeviceId, query: SmsQuery) -> Result<SmsAnswer> {
        let req = self.next_req.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        lock(&self.pending).insert(req, tx);
        let sent = OutboundPacket::reliable(&SmsRequest { req, query }, Priority::Interactive)
            .and_then(|p| self.peers.send(peer, p));
        if let Err(e) = sent {
            lock(&self.pending).remove(&req);
            return Err(e);
        }
        let answer = tokio::time::timeout(ANSWER_TIMEOUT, rx).await;
        lock(&self.pending).remove(&req);
        answer
            .map_err(|_| CoreError::Timeout)?
            .map_err(|_| CoreError::Closed)
    }

    /// A phone's conversations, newest first.
    pub async fn conversations(&self, peer: DeviceId) -> Result<Vec<Conversation>> {
        match self.ask(peer, SmsQuery::Conversations).await? {
            SmsAnswer::Conversations(c) => Ok(c),
            SmsAnswer::Error(e) => Err(CoreError::Transport(e)),
            _ => Err(CoreError::Violation("wrong answer")),
        }
    }

    /// Messages in a thread, oldest first, ending before `before_ms`.
    pub async fn messages(
        &self,
        peer: DeviceId,
        thread_id: i64,
        before_ms: Option<i64>,
        limit: u32,
    ) -> Result<Vec<Message>> {
        let query = SmsQuery::Messages {
            thread_id,
            before_ms,
            limit: limit.min(MAX_PAGE),
        };
        match self.ask(peer, query).await? {
            SmsAnswer::Messages(m) => Ok(m),
            SmsAnswer::Error(e) => Err(CoreError::Transport(e)),
            _ => Err(CoreError::Violation("wrong answer")),
        }
    }

    /// The whole of an MMS attachment (fetched in chunks).
    pub async fn attachment(&self, peer: DeviceId, part_id: i64, max: u64) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        loop {
            let query = SmsQuery::Attachment {
                part_id,
                offset: out.len() as u64,
                len: ATTACHMENT_CHUNK,
            };
            match self.ask(peer, query).await? {
                SmsAnswer::Data(d) if d.is_empty() => return Ok(out),
                SmsAnswer::Data(d) => {
                    out.extend_from_slice(&d);
                    if out.len() as u64 > max {
                        return Err(CoreError::Violation("attachment too large"));
                    }
                }
                SmsAnswer::Error(e) => return Err(CoreError::Transport(e)),
                _ => return Err(CoreError::Violation("wrong answer")),
            }
        }
    }

    /// Ask a phone to send a text (an MMS with attachments or several recipients).
    pub fn send(
        &self,
        peer: DeviceId,
        addresses: Vec<String>,
        text: &str,
        attachments: Vec<OutgoingAttachment>,
    ) -> Result<()> {
        let empty = text.trim().is_empty() && attachments.is_empty();
        if addresses.is_empty() || empty || text.len() > MAX_BODY {
            return Err(CoreError::Violation("empty or too long"));
        }
        if attachments.len() > MAX_SEND_ATTACHMENTS
            || attachments
                .iter()
                .any(|a| a.data.len() > ATTACHMENT_CHUNK as usize)
        {
            return Err(CoreError::Violation(
                "attachments too large for a picture message",
            ));
        }
        let send = SmsSend {
            addresses,
            text: text.to_owned(),
            attachments,
        };
        self.peers.send(
            peer,
            OutboundPacket::reliable(&send, Priority::Interactive)?,
        )
    }

    /// A message arrived on (or was sent from) this phone.
    pub fn new_message(&self, message: Message, name: Option<String>) {
        let new = SmsNew {
            message: message.clamp(),
            name,
        };
        if let Ok(packet) = OutboundPacket::reliable(&new, Priority::Interactive) {
            self.peers.broadcast(&packet);
        }
    }

    /// Tell the PCs how a send went.
    pub fn report(&self, ok: bool, detail: &str) {
        let status = SmsStatus {
            ok,
            detail: detail.chars().take(500).collect(),
        };
        if let Ok(packet) = OutboundPacket::reliable(&status, Priority::Interactive) {
            self.peers.broadcast(&packet);
        }
    }

    /// Answer on a blocking thread: the phone's host queries a database.
    fn answer(&self, ctx: &PluginCtx, req: u64, query: SmsQuery) {
        let (host, ctx) = (self.host.clone(), ctx.clone());
        tokio::task::spawn_blocking(move || {
            let answer = match query {
                SmsQuery::Conversations => host.conversations().map(|mut c| {
                    c.truncate(MAX_CONVERSATIONS);
                    SmsAnswer::Conversations(c)
                }),
                SmsQuery::Messages {
                    thread_id,
                    before_ms,
                    limit,
                } => host
                    .messages(thread_id, before_ms, limit.min(MAX_PAGE))
                    .map(|m| SmsAnswer::Messages(m.into_iter().map(Message::clamp).collect())),
                SmsQuery::Attachment {
                    part_id,
                    offset,
                    len,
                } => host
                    .attachment(part_id, offset, len.min(ATTACHMENT_CHUNK))
                    .map(SmsAnswer::Data),
            }
            .unwrap_or_else(SmsAnswer::Error);
            if let Ok(packet) =
                OutboundPacket::reliable(&SmsResponse { req, answer }, Priority::Interactive)
            {
                let _ = ctx.send(packet);
            }
        });
    }
}

#[async_trait]
impl Plugin for SmsPlugin {
    fn id(&self) -> &'static str {
        "sms"
    }

    fn incoming(&self) -> &'static [&'static str] {
        &[
            SmsRequest::TYPE,
            SmsResponse::TYPE,
            SmsSend::TYPE,
            SmsNew::TYPE,
            SmsStatus::TYPE,
        ]
    }

    fn outgoing(&self) -> &'static [&'static str] {
        self.incoming()
    }

    async fn on_connected(&self, ctx: &PluginCtx) {
        self.peers.insert(ctx);
    }

    async fn on_packet(&self, ctx: &PluginCtx, packet: Envelope) {
        let peer = ctx.peer();
        match packet.ty.as_str() {
            SmsRequest::TYPE => match packet.body::<SmsRequest>() {
                Ok(r) => self.answer(ctx, r.req, r.query),
                Err(e) => debug!(%peer, error = %e, "bad sms.request"),
            },
            SmsResponse::TYPE => match packet.body::<SmsResponse>() {
                Ok(r) => {
                    if let Some(tx) = lock(&self.pending).remove(&r.req) {
                        let _ = tx.send(r.answer);
                    }
                }
                Err(e) => debug!(%peer, error = %e, "bad sms.response"),
            },
            SmsSend::TYPE => match packet.body::<SmsSend>() {
                Ok(s) => {
                    let (host, ctx) = (self.host.clone(), ctx.clone());
                    tokio::task::spawn_blocking(move || {
                        if let Err(e) = host.send(&s.addresses, &s.text, &s.attachments) {
                            debug!(error = %e, "couldn't send the text");
                            let status = SmsStatus {
                                ok: false,
                                detail: e,
                            };
                            if let Ok(packet) =
                                OutboundPacket::reliable(&status, Priority::Interactive)
                            {
                                let _ = ctx.send(packet);
                            }
                        }
                    });
                }
                Err(e) => debug!(%peer, error = %e, "bad sms.send"),
            },
            SmsNew::TYPE => match packet.body::<SmsNew>() {
                Ok(n) => {
                    self.host
                        .received(&ctx.peer_info(), &n.message.clamp(), n.name.as_deref());
                }
                Err(e) => debug!(%peer, error = %e, "bad sms.new"),
            },
            SmsStatus::TYPE => match packet.body::<SmsStatus>() {
                Ok(st) => self.host.status(&ctx.peer_info(), st.ok, &st.detail),
                Err(e) => debug!(%peer, error = %e, "bad sms.status"),
            },
            _ => {}
        }
    }

    async fn on_disconnected(&self, peer: DeviceId) {
        self.peers.remove(peer);
    }
}
