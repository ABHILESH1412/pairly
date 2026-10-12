//! The relay over WebSockets: `pairly-relay-worker` (a Cloudflare Worker, in this repository)
//! keeps the same rooms as `pairly-relay`, on port 443 over TCP and TLS. That passes networks
//! that block UDP, and idle TCP connections live long enough to need a ping only every
//! [`PING_EVERY`] (QUIC needs one every 15 s), which is what keeps a phone's radio asleep.
//!
//! Each room has its own WebSocket: a *listener* that hears `present` / `absent` as the other
//! device comes and goes, and `call <id>` when it dials in. A call is another WebSocket on each
//! side (`dial`, `answer`), which the relay joins into a pipe once both are there (`matched`).
//! What goes through the pipe is the devices' Noise session, as over QUIC.

use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use pairly_core::transport::BoxDuplex;
use pairly_core::{CoreError, DeviceId, PeerCandidate, Result, TransportEvent, TransportKind};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::sync::{mpsc, watch};
use tokio::task::{AbortHandle, JoinSet};
use tokio::time::{Instant, sleep_until, timeout};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};
use tracing::{debug, info, warn};

use crate::proto::{BadRelayAddr, ROOM_LEN, Room, valid_token};
use crate::{RETRY_MAX, RETRY_MIN, Rooms, candidate_address, err};

/// How often an idle connection pings. Home routers were measured dropping idle connections
/// after 4–5 minutes; mobile networks can be stricter.
pub const PING_EVERY: Duration = Duration::from_secs(120);
/// No answer to a ping in this long: the connection is gone.
const PONG_WAIT: Duration = Duration::from_secs(20);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// A whole call: connecting (TLS can take a few seconds) and the other side answering.
pub(crate) const DIAL_TIMEOUT: Duration = Duration::from_secs(25);
/// How long a call waits for the other side to answer.
const MATCH_TIMEOUT: Duration = Duration::from_secs(10);
/// Bytes per message through a pipe (the Worker takes up to 256 KiB).
const CHUNK: usize = 64 * 1024;
const MAX_MESSAGE: usize = 256 * 1024;
const PIPE_BUFFER: usize = 256 * 1024;

type Ws = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// `wss://TOKEN@host[:port][/path]`: a WebSocket relay and its access token. `ws://` (no TLS)
/// is only for a relay on this computer (`wrangler dev`).
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct WsRelayAddr {
    pub tls: bool,
    pub host: String,
    pub port: u16,
    /// Where the relay is mounted: empty, or `/something` (no trailing slash).
    pub path: String,
    pub token: String,
}

impl WsRelayAddr {
    fn default_port(&self) -> u16 {
        if self.tls { 443 } else { 80 }
    }

    fn authority(&self) -> String {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        if self.port == self.default_port() {
            host
        } else {
            format!("{host}:{}", self.port)
        }
    }

    fn url(&self, role: &str, room: &Room, member: &str, call: Option<&str>) -> String {
        let scheme = if self.tls { "wss" } else { "ws" };
        let call = call.map_or_else(String::new, |c| format!("&call={c}"));
        format!(
            "{scheme}://{}{}/v1/{role}/{}?m={member}{call}",
            self.authority(),
            self.path,
            hex(room)
        )
    }
}

impl fmt::Display for WsRelayAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let scheme = if self.tls { "wss" } else { "ws" };
        write!(f, "{scheme}://{}@{}{}", self.token, self.authority(), self.path)
    }
}

/// Never print the token.
impl fmt::Debug for WsRelayAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "WsRelayAddr({}{})", self.authority(), self.path)
    }
}

fn loopback(host: &str) -> bool {
    host == "localhost"
        || host == "::1"
        || host.parse::<std::net::Ipv4Addr>().is_ok_and(|ip| ip.is_loopback())
}

impl FromStr for WsRelayAddr {
    type Err = BadRelayAddr;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        let (tls, rest) = if let Some(rest) = s.strip_prefix("wss://") {
            (true, rest)
        } else if let Some(rest) = s.strip_prefix("ws://") {
            (false, rest)
        } else {
            return Err(BadRelayAddr("must start with wss://"));
        };
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], rest[i..].trim_end_matches('/')),
            None => (rest, ""),
        };
        if !path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.~/".contains(&b))
        {
            return Err(BadRelayAddr("the path may only use A-Z a-z 0-9 - _ . ~ /"));
        }
        let (token, hostport) = authority
            .rsplit_once('@')
            .ok_or(BadRelayAddr("missing the access token (wss://TOKEN@host)"))?;
        if !valid_token(token) {
            return Err(BadRelayAddr("the token may only use A-Z a-z 0-9 - _ . ~"));
        }
        let default = if tls { 443 } else { 80 };
        let (host, port) = if let Some(v6) = hostport.strip_prefix('[') {
            match v6.split_once(']') {
                Some((host, "")) => (host, default),
                Some((host, port)) => (
                    host,
                    port.strip_prefix(':')
                        .and_then(|p| p.parse().ok())
                        .ok_or(BadRelayAddr("bad port"))?,
                ),
                None => return Err(BadRelayAddr("expected [ipv6]")),
            }
        } else {
            match hostport.rsplit_once(':') {
                Some((host, port)) => (host, port.parse().map_err(|_| BadRelayAddr("bad port"))?),
                None => (hostport, default),
            }
        };
        if host.is_empty() || host.chars().any(|c| c.is_whitespace() || "/?#@".contains(c)) {
            return Err(BadRelayAddr("bad host"));
        }
        if !tls && !loopback(host) {
            return Err(BadRelayAddr("use wss:// (ws:// is only for a relay on this computer)"));
        }
        Ok(Self {
            tls,
            host: host.to_owned(),
            port,
            path: path.to_owned(),
            token: token.to_owned(),
        })
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn valid_id(s: &str) -> bool {
    s.len() == ROOM_LEN * 2 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// This device's id in a room: the same every time (so a reconnect replaces the old listener
/// instead of adding one), different for the two devices, and telling the relay nothing.
pub(crate) fn member(room: &Room, me: &DeviceId) -> String {
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
    ctx.update(b"pairly relay member v1");
    ctx.update(room);
    ctx.update(me.as_bytes());
    hex(&ctx.finish().as_ref()[..ROOM_LEN])
}

fn tls_config() -> Result<Arc<rustls::ClientConfig>> {
    static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    if let Some(config) = CONFIG.get() {
        return Ok(config.clone());
    }
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(err)?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(CONFIG.get_or_init(|| Arc::new(config)).clone())
}

#[derive(Debug)]
enum OpenError {
    Denied,
    NoPeer,
    Gone,
    Other(String),
}

impl From<OpenError> for CoreError {
    fn from(e: OpenError) -> Self {
        err(match e {
            OpenError::Denied => "the relay refused (check the access token)".to_owned(),
            OpenError::NoPeer => "the device isn't on the relay".to_owned(),
            OpenError::Gone => "the call ended before it was answered".to_owned(),
            OpenError::Other(why) => why,
        })
    }
}

async fn open(addr: &WsRelayAddr, url: String) -> Result<Ws, OpenError> {
    let other = |e: &dyn fmt::Display| OpenError::Other(e.to_string());
    let mut request = url.into_client_request().map_err(|e| other(&e))?;
    request.headers_mut().insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {}", addr.token)).map_err(|e| other(&e))?,
    );
    let connector = if addr.tls {
        Connector::Rustls(tls_config().map_err(|e| other(&e))?)
    } else {
        Connector::Plain
    };
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE))
        .max_frame_size(Some(MAX_MESSAGE));
    let connect = tokio_tungstenite::connect_async_tls_with_config(
        request,
        Some(config),
        true,
        Some(connector),
    );
    match timeout(CONNECT_TIMEOUT, connect).await {
        Err(_) => Err(OpenError::Other("the relay didn't answer".into())),
        Ok(Ok((ws, _))) => Ok(ws),
        Ok(Err(tungstenite::Error::Http(response))) => Err(match response.status().as_u16() {
            401 | 403 => OpenError::Denied,
            404 => OpenError::NoPeer,
            410 => OpenError::Gone,
            status => OpenError::Other(format!("the relay answered {status}")),
        }),
        Ok(Err(e)) => Err(other(&e)),
    }
}

/// Wait for the relay to join a call.
async fn matched(ws: &mut Ws) -> Result<()> {
    let wait = async {
        while let Some(msg) = ws.next().await {
            match msg.map_err(err)? {
                Message::Text(t) if t.as_str() == "matched" => return Ok(()),
                Message::Close(_) => break,
                _ => {}
            }
        }
        Err(err("the relay ended the call"))
    };
    timeout(MATCH_TIMEOUT, wait)
        .await
        .map_err(|_| CoreError::Timeout)?
}

/// Pings on a shared schedule, so all of this device's relay connections wake the radio
/// together rather than each at its own time.
fn next_ping() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    let epoch = *EPOCH.get_or_init(Instant::now);
    let every = PING_EVERY.as_secs();
    epoch + Duration::from_secs((epoch.elapsed().as_secs() / every + 1) * every)
}

/// When to ping, and whether the last ping went unanswered.
struct Keepalive {
    next: Instant,
    /// A ping is out: anything heard before this clears it.
    answer_by: Option<Instant>,
}

impl Keepalive {
    fn new() -> Self {
        Self {
            next: next_ping(),
            answer_by: None,
        }
    }

    fn deadline(&self) -> Instant {
        self.answer_by.map_or(self.next, |a| a.min(self.next))
    }

    fn heard(&mut self) {
        self.answer_by = None;
    }

    /// At [`Self::deadline`]: `Err` if the connection is dead, `Ok(true)` to send a ping now.
    fn due(&mut self) -> Result<bool, ()> {
        let now = Instant::now();
        if self.answer_by.is_some_and(|a| a <= now) {
            return Err(());
        }
        if self.next > now {
            return Ok(false);
        }
        self.next = next_ping();
        self.answer_by.get_or_insert(now + PONG_WAIT);
        Ok(true)
    }
}

fn ping() -> Message {
    Message::Ping(Vec::new().into())
}

/// The matched call as a byte stream for the Noise session.
fn pipe(ws: Ws) -> BoxDuplex {
    let (ours, theirs) = tokio::io::duplex(PIPE_BUFFER);
    tokio::spawn(bridge(ws, ours));
    Box::new(theirs)
}

async fn bridge(mut ws: Ws, local: DuplexStream) {
    let (mut from_session, mut to_session) = tokio::io::split(local);
    let mut buf = vec![0u8; CHUNK];
    let mut keepalive = Keepalive::new();
    let reason = loop {
        tokio::select! {
            n = from_session.read(&mut buf) => match n {
                Ok(0) | Err(_) => break "closed here",
                Ok(n) => {
                    if ws.send(Message::Binary(buf[..n].to_vec().into())).await.is_err() {
                        break "the relay connection failed";
                    }
                }
            },
            msg = ws.next() => match msg {
                Some(Ok(Message::Binary(data))) => {
                    keepalive.heard();
                    if to_session.write_all(&data).await.is_err() {
                        break "closed here";
                    }
                }
                Some(Ok(Message::Close(_))) | None => break "the other side left",
                Some(Err(_)) => break "the relay connection failed",
                Some(Ok(_)) => keepalive.heard(),
            },
            () = sleep_until(keepalive.deadline()) => match keepalive.due() {
                Err(()) => break "no answer to ping",
                Ok(true) => {
                    if ws.send(ping()).await.is_err() {
                        break "the relay connection failed";
                    }
                }
                Ok(false) => {}
            },
        }
    };
    debug!(reason, "relay call ended");
    let _ = ws.close(None).await;
    let _ = to_session.shutdown().await;
}

/// Call the device listening in `room`.
pub(crate) async fn dial(addr: &WsRelayAddr, room: &Room, member: &str) -> Result<BoxDuplex> {
    let mut ws = open(addr, addr.url("dial", room, member, None)).await?;
    matched(&mut ws).await?;
    Ok(pipe(ws))
}

/// Rooms to listen in, on one WebSocket relay.
pub(crate) struct Handle {
    pub rooms: watch::Sender<Rooms>,
    pub addr: Arc<WsRelayAddr>,
}

pub(crate) fn start(
    addr: WsRelayAddr,
    key: String,
    me: DeviceId,
    rooms: Rooms,
    events: mpsc::Sender<TransportEvent>,
) -> Handle {
    let (tx, rx) = watch::channel(rooms);
    let addr = Arc::new(addr);
    tokio::spawn(run(addr.clone(), key, me, rx, events));
    Handle { rooms: tx, addr }
}

type Present = Arc<Mutex<HashMap<Room, PeerCandidate>>>;

/// A listener in each wanted room, until the room sender is dropped.
async fn run(
    addr: Arc<WsRelayAddr>,
    key: String,
    me: DeviceId,
    mut rooms: watch::Receiver<Rooms>,
    events: mpsc::Sender<TransportEvent>,
) {
    info!(relay = ?addr, "using a WebSocket relay");
    let present: Present = Arc::default();
    let mut listeners: HashMap<Room, AbortHandle> = HashMap::new();
    let mut tasks = JoinSet::new();
    loop {
        let wanted = rooms.borrow_and_update().clone();
        listeners.retain(|room, task| {
            let keep = wanted.contains_key(room);
            if !keep {
                task.abort();
            }
            keep
        });
        for (room, id) in &wanted {
            if listeners.contains_key(room) {
                continue;
            }
            let listener = Listener {
                addr: addr.clone(),
                room: *room,
                member: member(room, &me),
                candidate: PeerCandidate {
                    transport: TransportKind::Relay,
                    address: candidate_address(room, &key),
                    device_id: Some(*id),
                    name: None,
                },
                events: events.clone(),
                present: present.clone(),
            };
            listeners.insert(*room, tasks.spawn(listener.run()));
        }
        // Rooms we left: the device is no longer reachable here.
        let gone: Vec<PeerCandidate> = {
            let mut p = present.lock().unwrap_or_else(PoisonError::into_inner);
            let left: Vec<Room> = p
                .keys()
                .filter(|r| !wanted.contains_key(*r))
                .copied()
                .collect();
            left.iter().filter_map(|r| p.remove(r)).collect()
        };
        for c in gone {
            let _ = events.send(TransportEvent::Lost(c)).await;
        }
        if rooms.changed().await.is_err() {
            break;
        }
    }
    tasks.abort_all();
    let lost: Vec<PeerCandidate> = present
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .drain()
        .map(|(_, c)| c)
        .collect();
    for c in lost {
        let _ = events.send(TransportEvent::Lost(c)).await;
    }
}

/// Waits in one room: reports the device's presence and answers its calls.
struct Listener {
    addr: Arc<WsRelayAddr>,
    room: Room,
    member: String,
    candidate: PeerCandidate,
    events: mpsc::Sender<TransportEvent>,
    present: Present,
}

/// `retry`, give or take half, so devices that lost the relay together don't return together.
fn jittered(retry: Duration) -> Duration {
    let mut byte = [0u8; 1];
    let _ = ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut byte);
    retry / 2 + retry / 2 * u32::from(byte[0]) / 255
}

impl Listener {
    async fn run(self) {
        let mut retry = RETRY_MIN;
        loop {
            let started = Instant::now();
            match self.listen_once().await {
                Ok(()) => debug!(room = hex(&self.room[..4]), "relay listener closed"),
                Err(OpenError::Denied) => {
                    warn!("the relay refused (check the access token in its address)");
                    retry = RETRY_MAX;
                }
                Err(e) => debug!(error = ?e, "relay listener failed"),
            }
            self.set_present(false).await;
            if started.elapsed() > RETRY_MAX {
                retry = RETRY_MIN;
            }
            tokio::time::sleep(jittered(retry)).await;
            retry = (retry * 2).min(RETRY_MAX);
        }
    }

    async fn set_present(&self, present: bool) {
        let changed = {
            let mut p = self.present.lock().unwrap_or_else(PoisonError::into_inner);
            if present {
                p.insert(self.room, self.candidate.clone());
                true // "Seen again" also wakes a waiting reconnect.
            } else {
                p.remove(&self.room).is_some()
            }
        };
        if changed {
            let event = if present {
                TransportEvent::Discovered(self.candidate.clone())
            } else {
                TransportEvent::Lost(self.candidate.clone())
            };
            let _ = self.events.send(event).await;
        }
    }

    /// One listening connection, until it closes or fails.
    async fn listen_once(&self) -> Result<(), OpenError> {
        let url = self.addr.url("listen", &self.room, &self.member, None);
        let mut ws = open(&self.addr, url).await?;
        let mut keepalive = Keepalive::new();
        loop {
            tokio::select! {
                msg = ws.next() => match msg {
                    Some(Ok(Message::Text(text))) => {
                        keepalive.heard();
                        self.on_text(text.as_str()).await;
                    }
                    Some(Ok(Message::Close(_))) | None => return Ok(()),
                    Some(Err(e)) => return Err(OpenError::Other(e.to_string())),
                    Some(Ok(_)) => keepalive.heard(),
                },
                () = sleep_until(keepalive.deadline()) => match keepalive.due() {
                    Err(()) => return Err(OpenError::Other("no answer to ping".into())),
                    Ok(true) => ws.send(ping()).await.map_err(|e| OpenError::Other(e.to_string()))?,
                    Ok(false) => {}
                },
            }
        }
    }

    async fn on_text(&self, text: &str) {
        match text.split_once(' ') {
            None if text == "present" => self.set_present(true).await,
            None if text == "absent" => self.set_present(false).await,
            Some(("call", call)) if valid_id(call) => {
                let addr = self.addr.clone();
                let url = addr.url("answer", &self.room, &self.member, Some(call));
                let events = self.events.clone();
                tokio::spawn(async move {
                    let answered = async {
                        let mut ws = open(&addr, url).await?;
                        matched(&mut ws).await?;
                        Ok::<_, CoreError>(pipe(ws))
                    };
                    match answered.await {
                        Ok(stream) => {
                            let _ = events
                                .send(TransportEvent::Incoming {
                                    stream,
                                    transport: TransportKind::Relay,
                                    remote: None,
                                })
                                .await;
                        }
                        Err(e) => debug!(error = %e, "couldn't answer a relay call"),
                    }
                });
            }
            _ => debug!(text, "unexpected message from the relay"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_parse_and_print() {
        let a: WsRelayAddr = "wss://s3cret_1@pairly-relay.me.workers.dev"
            .parse()
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            (a.tls, a.host.as_str(), a.port, a.path.as_str(), a.token.as_str()),
            (true, "pairly-relay.me.workers.dev", 443, "", "s3cret_1")
        );
        assert_eq!(a.to_string(), "wss://s3cret_1@pairly-relay.me.workers.dev");
        assert!(!format!("{a:?}").contains("s3cret"));
        let room = [0xab; ROOM_LEN];
        assert_eq!(
            a.url("answer", &room, "m1", Some("c2")),
            format!("wss://pairly-relay.me.workers.dev/v1/answer/{}?m=m1&call=c2", hex(&room))
        );

        let b: WsRelayAddr = "wss://t@example.com:8443/relay/".parse().unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((b.port, b.path.as_str()), (8443, "/relay"));
        assert_eq!(b.to_string(), "wss://t@example.com:8443/relay");
        let local: WsRelayAddr = "ws://t@127.0.0.1:8787".parse().unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((local.tls, local.port), (false, 8787));

        for bad in [
            "wss://example.com",
            "ws://t@example.com",
            "https://t@example.com",
            "wss://bad token@example.com",
            "wss://t@example.com:x",
            "wss://t@/path",
            "wss://t@example.com/a?b",
        ] {
            assert!(bad.parse::<WsRelayAddr>().is_err(), "{bad}");
        }
    }

    #[test]
    fn members_differ_per_device_and_stay_put() {
        let room = [7; ROOM_LEN];
        let id = || pairly_crypto::IdentityKeypair::generate().device_id();
        let (a, b) = (id(), id());
        assert_eq!(member(&room, &a), member(&room, &a));
        assert_ne!(member(&room, &a), member(&room, &b));
        assert_ne!(member(&room, &a), member(&[8; ROOM_LEN], &a));
        assert!(valid_id(&member(&room, &a)));
    }

    #[tokio::test]
    async fn pings_are_due_on_schedule_and_missing_answers_are_noticed() {
        tokio::time::pause();
        let mut k = Keepalive::new();
        assert_eq!(k.due(), Ok(false));
        tokio::time::sleep_until(k.deadline()).await;
        assert_eq!(k.due(), Ok(true), "time to ping");
        tokio::time::sleep(PONG_WAIT / 2).await;
        k.heard();
        tokio::time::sleep_until(k.deadline()).await;
        assert_eq!(k.due(), Ok(true), "answered, so the next ping is on schedule");
        tokio::time::sleep_until(k.deadline()).await;
        assert_eq!(k.due(), Err(()), "no answer in time");
    }
}
