//! One logical link per paired device. A session outlives the channels it runs on: when a
//! channel is replaced (transport switch, reconnect), every unacknowledged packet is resent on
//! the new one and the receiver drops duplicates, so nothing is lost or delivered twice.
//!
//! Each attached channel gets a reader task and a writer task. The writer always sends the
//! highest-priority packet first (`Control` > `Interactive` > `Bulk`), so a file transfer
//! never delays a notification.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use ciborium::Value;
use pairly_crypto::DeviceId;
use pairly_proto::packets::{Ack, Identity, Keepalive, KeepaliveAck};
use pairly_proto::{Envelope, PROTOCOL_VERSION, PacketBody};
use tokio::sync::{Notify, mpsc};
use tokio::task::AbortHandle;
use tokio::time::{Instant, MissedTickBehavior};
use tracing::{debug, trace};

use crate::channel::{Channel, ChannelReader, ChannelWriter};
use crate::transport::TransportKind;
use crate::{CoreError, Result};

/// Unreliable packets (pointer motion, keepalives…) queued per priority while the link is slow;
/// newer ones are dropped beyond this rather than piling up.
const MAX_QUEUED_UNRELIABLE: usize = 512;
/// Reliable packets waiting for their ack (kept for resending). File transfers keep about 130
/// in flight, so this is only reached by a stuck or misbehaving peer.
const MAX_UNACKED: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Priority {
    Control = 0,
    Interactive = 1,
    /// Live video: after input (so touches don't wait behind frames), before file transfers.
    /// Its sender watches [`Session::queued`] and skips frames rather than letting them pile up.
    Video = 2,
    Bulk = 3,
}

/// A packet for the session to number and send.
#[derive(Debug, Clone)]
pub struct OutboundPacket {
    pub ty: String,
    pub body: Value,
    /// Resend until acknowledged (survives channel switches).
    pub ack: bool,
    pub priority: Priority,
}

impl OutboundPacket {
    /// Acknowledged delivery: resent across reconnects until the peer acks it.
    pub fn reliable<T: PacketBody>(body: &T, priority: Priority) -> Result<Self> {
        Self::new(body, true, priority)
    }

    /// Best effort: dropped if no channel is up (or it goes down before sending), never resent.
    pub fn unreliable<T: PacketBody>(body: &T, priority: Priority) -> Result<Self> {
        Self::new(body, false, priority)
    }

    fn new<T: PacketBody>(body: &T, ack: bool, priority: Priority) -> Result<Self> {
        let body =
            Value::serialized(body).map_err(|e| pairly_proto::ProtoError::Encode(e.to_string()))?;
        Ok(Self {
            ty: T::TYPE.to_owned(),
            body,
            ack,
            priority,
        })
    }
}

#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// Batch acks for at most this long.
    pub ack_delay: Duration,
    /// ...or until this many are pending.
    pub ack_batch: usize,
    pub keepalive_interval: Duration,
    /// Declare the channel dead after this long without inbound traffic.
    pub keepalive_timeout: Duration,
    /// Number of recent packet ids remembered for duplicate detection.
    pub dedup_window: usize,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            ack_delay: Duration::from_millis(50),
            ack_batch: 32,
            keepalive_interval: Duration::from_secs(15),
            keepalive_timeout: Duration::from_secs(45),
            dedup_window: 4096,
        }
    }
}

#[derive(Debug)]
pub enum SessionEvent {
    /// A channel was attached. Always delivered before packets from that channel.
    Connected {
        generation: u64,
        transport: TransportKind,
        peer: Identity,
    },
    /// A new (non-duplicate, non-control) packet from the peer.
    Packet(Envelope),
    Disconnected {
        generation: u64,
        reason: String,
    },
}

/// Remembers recently seen packet ids. Ids at or below `floor` count as seen.
#[derive(Debug)]
struct DedupWindow {
    floor: u64,
    seen: BTreeSet<u64>,
    cap: usize,
}

impl DedupWindow {
    fn new(cap: usize) -> Self {
        Self {
            floor: 0,
            seen: BTreeSet::new(),
            cap,
        }
    }

    fn reset(&mut self) {
        self.floor = 0;
        self.seen.clear();
    }

    /// Returns `true` if `id` is new.
    fn insert(&mut self, id: u64) -> bool {
        if id <= self.floor || !self.seen.insert(id) {
            return false;
        }
        while self.seen.len() > self.cap {
            if let Some(min) = self.seen.pop_first() {
                self.floor = min;
            }
        }
        true
    }
}

struct Link {
    transport: TransportKind,
    locally_initiated: bool,
    last_rx: Instant,
    rtt: Option<Duration>,
    reader: AbortHandle,
    writer: AbortHandle,
}

struct State {
    next_id: u64,
    unacked: BTreeMap<u64, (Envelope, Priority)>,
    queues: [VecDeque<Envelope>; 4],
    pending_acks: Vec<u64>,
    ack_deadline: Option<Instant>,
    dedup: DedupWindow,
    peer_session_nonce: Option<u64>,
    generation: u64,
    link: Option<Link>,
    /// Set by [`Session::close`]: flush acks, then end the channel.
    closing: bool,
}

impl State {
    fn envelope(&mut self, ty: String, body: Value, ack: bool) -> Envelope {
        let id = self.next_id;
        self.next_id += 1;
        Envelope {
            v: PROTOCOL_VERSION,
            id,
            ack,
            ty,
            body,
        }
    }

    fn control<T: PacketBody>(&mut self, body: &T) -> Option<Envelope> {
        let value = Value::serialized(body).ok()?;
        Some(self.envelope(T::TYPE.to_owned(), value, false))
    }
}

struct Shared {
    peer: DeviceId,
    config: SessionConfig,
    epoch: Instant,
    state: Mutex<State>,
    wake: Notify,
    /// Signalled by the writer once a graceful close has flushed.
    closed: Notify,
    events: mpsc::Sender<SessionEvent>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn now_ms(&self) -> u64 {
        u64::try_from(self.epoch.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
}

enum Next {
    Send(Envelope),
    Wait {
        ack_deadline: Option<Instant>,
        rx_deadline: Instant,
    },
    Close,
    Stale,
}

#[derive(Clone, Copy)]
enum Role {
    Reader,
    Writer,
}

pub struct Session {
    shared: Arc<Shared>,
}

impl Session {
    pub fn new(peer: DeviceId, config: SessionConfig, events: mpsc::Sender<SessionEvent>) -> Self {
        let state = State {
            next_id: 1,
            unacked: BTreeMap::new(),
            queues: Default::default(),
            pending_acks: Vec::new(),
            ack_deadline: None,
            dedup: DedupWindow::new(config.dedup_window),
            peer_session_nonce: None,
            generation: 0,
            link: None,
            closing: false,
        };
        Self {
            shared: Arc::new(Shared {
                peer,
                config,
                epoch: Instant::now(),
                state: Mutex::new(state),
                wake: Notify::new(),
                closed: Notify::new(),
                events,
            }),
        }
    }

    pub fn peer(&self) -> DeviceId {
        self.shared.peer
    }

    /// Run the session over `channel`, replacing any current channel. Unacked packets are
    /// resent on it. Returns the new generation.
    pub async fn attach(&self, channel: Channel, peer: Identity) -> u64 {
        let generation = {
            let mut st = self.shared.lock();
            st.generation += 1;
            if let Some(old) = st.link.take() {
                old.reader.abort();
                old.writer.abort();
            }
            st.generation
        };
        // Sent before the reader task exists, so it precedes this channel's packets.
        let _ = self
            .shared
            .events
            .send(SessionEvent::Connected {
                generation,
                transport: channel.transport,
                peer: peer.clone(),
            })
            .await;

        let mut st = self.shared.lock();
        if st.generation != generation {
            return generation; // superseded by a concurrent attach
        }
        st.closing = false;
        if st.peer_session_nonce != Some(peer.session_nonce) {
            // The peer restarted (or this is the first channel): its ids start over.
            st.dedup.reset();
            st.peer_session_nonce = Some(peer.session_nonce);
        }
        // Start the new channel with everything unacked, in id order. Unreliable packets still
        // queued for the old channel are dropped: they were meant for a link that is gone (and a
        // file transfer resumes from what the receiver actually got).
        for q in &mut st.queues {
            q.clear();
        }
        let resend: Vec<_> = st.unacked.values().cloned().collect();
        for (env, priority) in resend {
            st.queues[priority as usize].push_back(env);
        }
        debug!(peer = %self.shared.peer, generation, transport = ?channel.transport,
            resent = st.unacked.len(), "channel attached");

        let Channel {
            reader,
            writer,
            initiator,
            transport,
            ..
        } = channel;
        let reader =
            tokio::spawn(reader_loop(self.shared.clone(), reader, generation)).abort_handle();
        let writer =
            tokio::spawn(writer_loop(self.shared.clone(), writer, generation)).abort_handle();
        st.link = Some(Link {
            transport,
            locally_initiated: initiator,
            last_rx: Instant::now(),
            rtt: None,
            reader,
            writer,
        });
        drop(st);
        self.shared.wake.notify_one();
        generation
    }

    /// Gracefully end the current channel: send pending acks (so the peer won't resend packets
    /// we already handled), half-close the stream, then detach. Gives up after `max_wait`.
    pub async fn close(&self, max_wait: Duration) {
        {
            let mut st = self.shared.lock();
            if st.link.is_none() {
                return;
            }
            st.closing = true;
        }
        self.shared.wake.notify_one();
        let _ = tokio::time::timeout(max_wait, self.shared.closed.notified()).await;
        self.detach();
    }

    /// Drop the current channel immediately, without events (unpair, shutdown).
    pub fn detach(&self) {
        let mut st = self.shared.lock();
        st.generation += 1;
        if let Some(link) = st.link.take() {
            link.reader.abort();
            link.writer.abort();
        }
    }

    /// Queue a packet. Returns its id, or `None` if it was unreliable and was dropped (no
    /// channel is up, or the queue is full). A reliable packet is refused with
    /// [`CoreError::Backlog`] when too many are already waiting for their acks.
    pub fn send(&self, packet: OutboundPacket) -> Result<Option<u64>, CoreError> {
        let mut st = self.shared.lock();
        if !packet.ack
            && (st.link.is_none()
                || st.queues[packet.priority as usize].len() >= MAX_QUEUED_UNRELIABLE)
        {
            return Ok(None);
        }
        if packet.ack && st.unacked.len() >= MAX_UNACKED {
            return Err(CoreError::Backlog(self.shared.peer));
        }
        let env = st.envelope(packet.ty, packet.body, packet.ack);
        let id = env.id;
        if packet.ack {
            st.unacked.insert(id, (env.clone(), packet.priority));
        }
        if st.link.is_some() {
            st.queues[packet.priority as usize].push_back(env);
            drop(st);
            self.shared.wake.notify_one();
        }
        Ok(Some(id))
    }

    /// How many packets of `priority` are waiting to be written.
    pub fn queued(&self, priority: Priority) -> usize {
        self.shared.lock().queues[priority as usize].len()
    }

    /// Forget the unreliable packets of `priority` still waiting (a live stream skipping
    /// ahead). Reliable ones stay.
    pub fn drop_unreliable(&self, priority: Priority) {
        self.shared.lock().queues[priority as usize].retain(|env| env.ack);
    }

    pub fn is_connected(&self) -> bool {
        self.shared.lock().link.is_some()
    }

    pub fn generation(&self) -> u64 {
        self.shared.lock().generation
    }

    pub fn transport(&self) -> Option<TransportKind> {
        self.shared.lock().link.as_ref().map(|l| l.transport)
    }

    /// Whether the current channel was opened by us.
    pub fn locally_initiated(&self) -> Option<bool> {
        self.shared
            .lock()
            .link
            .as_ref()
            .map(|l| l.locally_initiated)
    }

    pub fn peer_session_nonce(&self) -> Option<u64> {
        self.shared.lock().peer_session_nonce
    }

    pub fn rtt(&self) -> Option<Duration> {
        self.shared.lock().link.as_ref().and_then(|l| l.rtt)
    }

    pub fn unacked_len(&self) -> usize {
        self.shared.lock().unacked.len()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.detach();
    }
}

async fn reader_loop(shared: Arc<Shared>, mut reader: ChannelReader, generation: u64) {
    let reason = loop {
        match reader.recv().await {
            Ok(Some(env)) => {
                if let Some(env) = on_inbound(&shared, env, generation)
                    && shared.events.send(SessionEvent::Packet(env)).await.is_err()
                {
                    return; // session dropped
                }
            }
            Ok(None) => break "closed by peer".to_owned(),
            Err(e) => break e.to_string(),
        }
    };
    link_lost(&shared, generation, reason, Role::Reader).await;
}

/// Handle control packets and dedup. Returns the envelope if it should be delivered.
fn on_inbound(shared: &Shared, env: Envelope, generation: u64) -> Option<Envelope> {
    let now = Instant::now();
    let mut st = shared.lock();
    if st.generation != generation {
        return None;
    }
    if let Some(link) = st.link.as_mut() {
        link.last_rx = now;
    }
    match env.ty.as_str() {
        Ack::TYPE => {
            for id in env.body::<Ack>().ok()?.ids {
                st.unacked.remove(&id);
            }
            None
        }
        Keepalive::TYPE => {
            let t = env.body::<Keepalive>().ok()?.t;
            let reply = st.control(&KeepaliveAck { t })?;
            st.queues[Priority::Control as usize].push_back(reply);
            drop(st);
            shared.wake.notify_one();
            None
        }
        KeepaliveAck::TYPE => {
            let t = env.body::<KeepaliveAck>().ok()?.t;
            let rtt = Duration::from_millis(shared.now_ms().saturating_sub(t));
            if let Some(link) = st.link.as_mut() {
                link.rtt = Some(rtt);
            }
            None
        }
        _ => {
            if env.ack {
                // Ack duplicates too: our previous ack may have been lost with the old channel.
                st.pending_acks.push(env.id);
                if st.ack_deadline.is_none() {
                    st.ack_deadline = Some(now + shared.config.ack_delay);
                }
                shared.wake.notify_one();
            }
            if st.dedup.insert(env.id) {
                Some(env)
            } else {
                trace!(peer = %shared.peer, id = env.id, "dropped duplicate");
                None
            }
        }
    }
}

async fn writer_loop(shared: Arc<Shared>, mut writer: ChannelWriter, generation: u64) {
    let interval = shared.config.keepalive_interval;
    let mut keepalive = tokio::time::interval_at(Instant::now() + interval, interval);
    keepalive.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let reason = loop {
        match next_outbound(&shared, generation) {
            Next::Stale => return,
            Next::Close => {
                writer.shutdown().await;
                shared.closed.notify_one();
                return;
            }
            Next::Send(env) => {
                if let Err(e) = writer.send(&env).await {
                    break e.to_string();
                }
            }
            Next::Wait {
                ack_deadline,
                rx_deadline,
            } => {
                tokio::select! {
                    () = shared.wake.notified() => {}
                    () = tokio::time::sleep_until(ack_deadline.unwrap_or(rx_deadline)),
                        if ack_deadline.is_some() => {}
                    _ = keepalive.tick() => {
                        let t = shared.now_ms();
                        let mut st = shared.lock();
                        if let Some(env) = st.control(&Keepalive { t }) {
                            st.queues[Priority::Control as usize].push_back(env);
                        }
                    }
                    () = tokio::time::sleep_until(rx_deadline) => {
                        let expired = shared.lock().link.as_ref()
                            .is_some_and(|l| l.last_rx + shared.config.keepalive_timeout <= Instant::now());
                        if expired {
                            break "keepalive timeout".to_owned();
                        }
                    }
                }
            }
        }
    };
    writer.shutdown().await;
    link_lost(&shared, generation, reason, Role::Writer).await;
}

fn next_outbound(shared: &Shared, generation: u64) -> Next {
    let mut st = shared.lock();
    if st.generation != generation {
        return Next::Stale;
    }
    let now = Instant::now();
    let acks_due = st.closing
        || st.pending_acks.len() >= shared.config.ack_batch
        || st.ack_deadline.is_some_and(|d| d <= now);
    if !st.pending_acks.is_empty() && acks_due {
        let ids = std::mem::take(&mut st.pending_acks);
        st.ack_deadline = None;
        if let Some(env) = st.control(&Ack { ids }) {
            return Next::Send(env);
        }
    }
    if st.closing {
        return Next::Close;
    }
    for priority in 0..st.queues.len() {
        if let Some(env) = st.queues[priority].pop_front() {
            return Next::Send(env);
        }
    }
    let last_rx = st.link.as_ref().map_or(now, |l| l.last_rx);
    Next::Wait {
        ack_deadline: st.ack_deadline,
        rx_deadline: last_rx + shared.config.keepalive_timeout,
    }
}

async fn link_lost(shared: &Shared, generation: u64, reason: String, role: Role) {
    {
        let mut st = shared.lock();
        if st.generation != generation {
            return;
        }
        let Some(link) = st.link.take() else { return };
        // Stop the other half; the calling task finishes on its own.
        match role {
            Role::Reader => link.writer.abort(),
            Role::Writer => link.reader.abort(),
        }
    }
    debug!(peer = %shared.peer, generation, %reason, "channel lost");
    let _ = shared
        .events
        .send(SessionEvent::Disconnected { generation, reason })
        .await;
}

#[cfg(test)]
mod tests {
    use pairly_crypto::IdentityKeypair;
    use pairly_proto::packets::DeviceType;
    use serde::{Deserialize, Serialize};

    use super::*;
    use crate::channel::tests::channel_pair;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Msg {
        n: u64,
    }
    impl PacketBody for Msg {
        const TYPE: &'static str = "test.msg";
    }

    fn identity(nonce: u64) -> Identity {
        Identity {
            name: "peer".into(),
            device_type: DeviceType::Desktop,
            app_version: "0".into(),
            session_nonce: nonce,
            incoming: vec![],
            outgoing: vec![],
            relay: None,
            bluetooth: None,
        }
    }

    struct Pair {
        a: Session,
        b: Session,
        a_events: mpsc::Receiver<SessionEvent>,
        b_events: mpsc::Receiver<SessionEvent>,
        ka: IdentityKeypair,
        kb: IdentityKeypair,
    }

    impl Pair {
        fn new(config: SessionConfig) -> Self {
            let (ka, kb) = (IdentityKeypair::generate(), IdentityKeypair::generate());
            let (ta, a_events) = mpsc::channel(1024);
            let (tb, b_events) = mpsc::channel(1024);
            Self {
                a: Session::new(kb.device_id(), config.clone(), ta),
                b: Session::new(ka.device_id(), config, tb),
                a_events,
                b_events,
                ka,
                kb,
            }
        }

        async fn connect(&mut self) {
            let (ca, cb) = channel_pair(&self.ka, &self.kb).await;
            self.a.attach(ca, identity(2)).await;
            self.b.attach(cb, identity(1)).await;
            assert!(matches!(
                next(&mut self.a_events).await,
                SessionEvent::Connected { .. }
            ));
            assert!(matches!(
                next(&mut self.b_events).await,
                SessionEvent::Connected { .. }
            ));
        }
    }

    async fn next(rx: &mut mpsc::Receiver<SessionEvent>) -> SessionEvent {
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("event")
            .expect("open")
    }

    async fn expect_msg(rx: &mut mpsc::Receiver<SessionEvent>) -> u64 {
        match next(rx).await {
            SessionEvent::Packet(env) => env.body::<Msg>().unwrap().n,
            other => panic!("expected packet, got {other:?}"),
        }
    }

    async fn assert_quiet(rx: &mut mpsc::Receiver<SessionEvent>) {
        let res = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await;
        assert!(res.is_err(), "unexpected event: {res:?}");
    }

    fn msg(n: u64, priority: Priority) -> OutboundPacket {
        OutboundPacket::reliable(&Msg { n }, priority).unwrap()
    }

    #[test]
    fn dedup_window() {
        let mut w = DedupWindow::new(3);
        assert!(w.insert(1) && w.insert(2) && w.insert(5));
        assert!(!w.insert(2));
        assert!(w.insert(6)); // evicts 1 → floor 1
        assert!(!w.insert(1));
        assert!(w.insert(3) && w.insert(4)); // within the window
        w.reset();
        assert!(w.insert(1));
    }

    #[test]
    fn backlog_is_bounded() {
        let (tx, _rx) = mpsc::channel(1);
        let s = Session::new(
            IdentityKeypair::generate().device_id(),
            SessionConfig::default(),
            tx,
        );
        // No channel: reliable packets wait for one, up to the cap.
        for n in 0..MAX_UNACKED as u64 {
            assert!(s.send(msg(n, Priority::Bulk)).unwrap().is_some());
        }
        assert!(matches!(
            s.send(msg(0, Priority::Bulk)),
            Err(CoreError::Backlog(_))
        ));
        // Unreliable ones are just dropped.
        let unreliable = OutboundPacket::unreliable(&Msg { n: 1 }, Priority::Control).unwrap();
        assert!(s.send(unreliable).unwrap().is_none());
    }

    #[tokio::test]
    async fn delivers_and_acks() {
        let mut p = Pair::new(SessionConfig::default());
        p.connect().await;
        for n in 0..10 {
            p.a.send(msg(n, Priority::Interactive)).unwrap();
        }
        for n in 0..10 {
            assert_eq!(expect_msg(&mut p.b_events).await, n);
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            while p.a.unacked_len() > 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("acks arrive");
    }

    #[tokio::test]
    async fn migration_resends_unacked_exactly_once() {
        // Acks are never flushed, so everything stays unacked across the switch.
        let config = SessionConfig {
            ack_delay: Duration::from_secs(3600),
            ack_batch: usize::MAX,
            ..Default::default()
        };
        let mut p = Pair::new(config);
        p.connect().await;
        for n in 0..20 {
            p.a.send(msg(n, Priority::Interactive)).unwrap();
        }
        for n in 0..20 {
            assert_eq!(expect_msg(&mut p.b_events).await, n);
        }
        assert_eq!(p.a.unacked_len(), 20);

        // Channel dies. Packets sent meanwhile are kept for later.
        p.a.detach();
        p.b.detach();
        for n in 20..25 {
            p.a.send(msg(n, Priority::Interactive)).unwrap();
        }
        assert!(
            p.a.send(OutboundPacket::unreliable(&Msg { n: 99 }, Priority::Control).unwrap())
                .unwrap()
                .is_none()
        );

        // New channel: all 25 are resent, B drops the 20 it already has.
        p.connect().await;
        for n in 20..25 {
            assert_eq!(expect_msg(&mut p.b_events).await, n);
        }
        assert_quiet(&mut p.b_events).await;
    }

    #[tokio::test]
    async fn close_flushes_pending_acks() {
        // Acks would otherwise wait an hour.
        let config = SessionConfig {
            ack_delay: Duration::from_secs(3600),
            ack_batch: usize::MAX,
            ..Default::default()
        };
        let mut p = Pair::new(config);
        p.connect().await;
        p.a.send(msg(1, Priority::Interactive)).unwrap();
        assert_eq!(expect_msg(&mut p.b_events).await, 1);
        assert_eq!(p.a.unacked_len(), 1);
        p.b.close(Duration::from_secs(1)).await;
        assert!(!p.b.is_connected());
        tokio::time::timeout(Duration::from_secs(5), async {
            while p.a.unacked_len() > 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("ack flushed on close");
        // A then sees the clean EOF.
        assert!(matches!(
            next(&mut p.a_events).await,
            SessionEvent::Disconnected { .. }
        ));
    }

    #[tokio::test]
    async fn control_beats_bulk() {
        let config = SessionConfig::default();
        let mut p = Pair::new(config);
        // Queue while disconnected so the writer sees everything at once.
        for n in 0..5 {
            p.a.send(msg(n, Priority::Bulk)).unwrap();
        }
        p.a.send(msg(100, Priority::Control)).unwrap();
        p.a.send(msg(50, Priority::Interactive)).unwrap();
        p.connect().await;
        let order: Vec<u64> = futures_collect(&mut p.b_events, 7).await;
        assert_eq!(order, vec![100, 50, 0, 1, 2, 3, 4]);
    }

    async fn futures_collect(rx: &mut mpsc::Receiver<SessionEvent>, n: usize) -> Vec<u64> {
        let mut out = Vec::new();
        for _ in 0..n {
            out.push(expect_msg(rx).await);
        }
        out
    }

    #[tokio::test]
    async fn peer_restart_resets_dedup() {
        let config = SessionConfig {
            ack_delay: Duration::from_secs(3600),
            ack_batch: usize::MAX,
            ..Default::default()
        };
        let mut p = Pair::new(config.clone());
        p.connect().await;
        p.a.send(msg(1, Priority::Interactive)).unwrap();
        assert_eq!(expect_msg(&mut p.b_events).await, 1);

        // A restarts: new session (ids start at 1 again) and a new session nonce.
        let (ta, a_events) = mpsc::channel(1024);
        p.a = Session::new(p.kb.device_id(), config, ta);
        p.a_events = a_events;
        p.b.detach();
        let (ca, cb) = channel_pair(&p.ka, &p.kb).await;
        p.a.attach(ca, identity(2)).await;
        p.b.attach(cb, identity(777)).await;
        let _ = next(&mut p.b_events).await; // Connected
        p.a.send(msg(2, Priority::Interactive)).unwrap(); // id 1 again
        assert_eq!(expect_msg(&mut p.b_events).await, 2);
    }

    #[tokio::test]
    async fn keepalive_measures_rtt_and_detects_silence() {
        let config = SessionConfig {
            keepalive_interval: Duration::from_millis(20),
            keepalive_timeout: Duration::from_millis(150),
            ..Default::default()
        };
        let mut p = Pair::new(config.clone());
        p.connect().await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(p.a.rtt().is_some(), "rtt measured from keepalive echo");
        assert!(p.a.is_connected());

        // A silent peer: the other end of A's channel never answers.
        let (ca, _cb_silent) = channel_pair(&p.ka, &p.kb).await;
        let generation = p.a.attach(ca, identity(2)).await;
        let _ = next(&mut p.a_events).await; // Connected
        match next(&mut p.a_events).await {
            SessionEvent::Disconnected {
                generation: g,
                reason,
            } => {
                assert_eq!(g, generation);
                assert_eq!(reason, "keepalive timeout");
            }
            other => panic!("expected disconnect, got {other:?}"),
        }
        assert!(!p.a.is_connected());
    }
}
