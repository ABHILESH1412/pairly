//! The node owns the identity, registry, transports, sessions and plugins, and drives pairing
//! and connection management. Apps talk to it through [`PairlyNode`] and [`NodeEvent`]s.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use pairly_crypto::sas::{SasCode, rendezvous};
use pairly_crypto::{
    DeviceId, HandshakeKind, IdentityKeypair, Initiate, KeyStore, MemoryKeyStore, PSK_LEN,
    PublicKey,
};
use pairly_proto::packets::{DeviceType, Identity};
use tokio::sync::{Notify, broadcast, mpsc, oneshot};
use tokio::task::{AbortHandle, JoinSet};
use tokio::time::Instant;
use tracing::{debug, info};

use crate::channel::{self, AcceptPolicy, Channel, HANDSHAKE_TIMEOUT};
use crate::pairing;
use crate::platform::{NullPlatform, Platform};
use crate::plugin::{Plugin, PluginCtx, accepts};
use crate::qr::QrInvite;
use crate::registry::{PairedDevice, Registry, unix_now};
use crate::session::{OutboundPacket, Session, SessionConfig, SessionEvent};
use crate::transport::{
    Advertisement, BoxDuplex, PairedPeer, PeerCandidate, Transport, TransportEvent, TransportKind,
};
use crate::{CoreError, Result};

#[derive(Debug, Clone)]
pub struct NodeConfig {
    pub name: String,
    pub device_type: DeviceType,
    pub app_version: String,
    /// Accept inbound pairing requests.
    pub allow_pairing: bool,
    /// How long to wait for both users to confirm the code.
    pub pairing_timeout: Duration,
    pub reconnect_max_backoff: Duration,
    /// How long a pairing QR code stays valid.
    pub qr_timeout: Duration,
    /// The relay this device uses (`pairly-relay://…`); announced to paired peers.
    pub relay: Option<String>,
    /// This device's Bluetooth address, announced to paired peers so they can dial it.
    pub bluetooth: Option<String>,
    /// Pad what we send over relay links to 256-byte steps, so the relay learns less from
    /// message sizes (costs about 128 bytes per frame).
    pub relay_padding: bool,
    pub session: SessionConfig,
}

impl NodeConfig {
    pub fn new(name: impl Into<String>, device_type: DeviceType) -> Self {
        Self {
            name: name.into(),
            device_type,
            app_version: env!("CARGO_PKG_VERSION").to_owned(),
            allow_pairing: true,
            pairing_timeout: Duration::from_secs(120),
            reconnect_max_backoff: Duration::from_secs(30),
            qr_timeout: Duration::from_secs(300),
            relay: None,
            bluetooth: None,
            relay_padding: true,
            session: SessionConfig::default(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum NodeEvent {
    DeviceDiscovered {
        id: DeviceId,
        name: Option<String>,
        transport: TransportKind,
    },
    DeviceLost {
        id: DeviceId,
    },
    /// Show `code` and ask the user to confirm; answer with [`PairlyNode::confirm_pair`].
    PairingRequested {
        id: DeviceId,
        name: String,
        code: SasCode,
        incoming: bool,
    },
    Paired {
        id: DeviceId,
        name: String,
    },
    PairingFailed {
        id: DeviceId,
        reason: String,
    },
    Connected {
        id: DeviceId,
        transport: TransportKind,
    },
    Disconnected {
        id: DeviceId,
        reason: String,
    },
    Unpaired {
        id: DeviceId,
    },
}

#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub id: DeviceId,
    pub name: String,
    /// Known once paired.
    pub device_type: Option<DeviceType>,
    pub paired: bool,
    /// The active transport, if connected.
    pub link: Option<TransportKind>,
    pub rtt: Option<Duration>,
}

pub struct NodeBuilder {
    config: NodeConfig,
    keystore: Option<Arc<dyn KeyStore>>,
    registry: Option<Registry>,
    platform: Option<Arc<dyn Platform>>,
    transports: Vec<Arc<dyn Transport>>,
    plugins: Vec<Arc<dyn Plugin>>,
}

impl NodeBuilder {
    pub fn keystore(mut self, keystore: Arc<dyn KeyStore>) -> Self {
        self.keystore = Some(keystore);
        self
    }

    pub fn registry(mut self, registry: Registry) -> Self {
        self.registry = Some(registry);
        self
    }

    pub fn platform(mut self, platform: Arc<dyn Platform>) -> Self {
        self.platform = Some(platform);
        self
    }

    pub fn transport(mut self, transport: Arc<dyn Transport>) -> Self {
        self.transports.push(transport);
        self
    }

    pub fn plugin(mut self, plugin: Arc<dyn Plugin>) -> Self {
        self.plugins.push(plugin);
        self
    }

    pub async fn start(self) -> Result<PairlyNode> {
        let keystore = self
            .keystore
            .unwrap_or_else(|| Arc::new(MemoryKeyStore::new()));
        let identity = pairly_crypto::load_or_generate(keystore.as_ref())?;
        let registry = match self.registry {
            Some(r) => r,
            None => Registry::open_in_memory()?,
        };
        registry.seal_with(pairly_crypto::FieldKey::derive(&identity, "registry"))?;
        let stale = registry.drop_unreadable()?;
        if stale > 0 {
            tracing::warn!(
                devices = stale,
                "removed pairings made under a previous identity; pair those devices again"
            );
        }

        let mut routes = HashMap::new();
        let (mut incoming, mut outgoing) = (Vec::new(), Vec::new());
        for p in &self.plugins {
            for ty in p.incoming() {
                routes.insert((*ty).to_owned(), p.clone());
                incoming.push((*ty).to_owned());
            }
            outgoing.extend(p.outgoing().iter().map(|t| (*t).to_owned()));
        }
        incoming.sort();
        incoming.dedup();
        outgoing.sort();
        outgoing.dedup();
        let local = Identity {
            name: self.config.name.clone(),
            device_type: self.config.device_type,
            app_version: self.config.app_version.clone(),
            session_nonce: rand::random(),
            incoming,
            outgoing,
            relay: self.config.relay.clone(),
            bluetooth: self.config.bluetooth.clone(),
        };
        local.validate()?;

        let (events, _) = broadcast::channel(256);
        let inner = Arc::new(Inner {
            identity,
            local,
            config: self.config,
            registry,
            platform: self.platform.unwrap_or_else(|| Arc::new(NullPlatform)),
            transports: self.transports,
            plugins: self.plugins,
            routes,
            events,
            state: Mutex::new(State::default()),
            tasks: Mutex::new(Vec::new()),
            inbound: Arc::new(tokio::sync::Semaphore::new(MAX_INBOUND_HANDSHAKES)),
        });

        let (tx, rx) = mpsc::channel(256);
        inner.spawn(main_loop(inner.clone(), rx));
        let advert = Advertisement {
            device_id: inner.identity.device_id(),
            name: inner.config.name.clone(),
            device_type: inner.config.device_type,
        };
        for t in &inner.transports {
            t.start(advert.clone(), tx.clone()).await?;
        }
        inner.update_transport_peers();
        info!(id = %inner.identity.device_id(), name = %inner.config.name, "node started");
        Ok(PairlyNode { inner })
    }
}

/// Handle to a running node. Cheap to clone. Call [`PairlyNode::shutdown`] when done.
#[derive(Clone)]
pub struct PairlyNode {
    inner: Arc<Inner>,
}

impl PairlyNode {
    pub fn builder(config: NodeConfig) -> NodeBuilder {
        NodeBuilder {
            config,
            keystore: None,
            registry: None,
            platform: None,
            transports: vec![],
            plugins: vec![],
        }
    }

    pub fn device_id(&self) -> DeviceId {
        self.inner.device_id()
    }

    pub fn public_key(&self) -> PublicKey {
        self.inner.identity.public()
    }

    pub fn name(&self) -> &str {
        &self.inner.config.name
    }

    pub fn subscribe(&self) -> broadcast::Receiver<NodeEvent> {
        self.inner.events.subscribe()
    }

    /// Paired devices (connected or not) followed by discovered, unpaired ones.
    pub fn devices(&self) -> Result<Vec<DeviceInfo>> {
        self.inner.devices()
    }

    /// Start pairing with a discovered device. Progress arrives as [`NodeEvent`]s.
    pub async fn request_pair(&self, id: DeviceId) -> Result<()> {
        self.inner.request_pair(id).await
    }

    /// Show a pairing QR code. Valid for [`NodeConfig::qr_timeout`] and for one pairing;
    /// a new call replaces the previous code.
    pub fn start_qr_pairing(&self) -> Result<QrInvite> {
        self.inner.start_qr_pairing()
    }

    pub fn cancel_qr_pairing(&self) {
        self.inner.lock().qr = None;
    }

    /// Pair with the device whose QR code was scanned. Resolves once paired.
    pub async fn pair_from_qr(&self, uri: &str) -> Result<()> {
        let invite = QrInvite::parse(uri)?;
        self.inner.pair_from_qr(invite).await
    }

    /// The user's answer to a [`NodeEvent::PairingRequested`].
    pub fn confirm_pair(&self, id: DeviceId, accept: bool) -> Result<()> {
        let tx = self
            .inner
            .lock()
            .pairings
            .remove(&id)
            .ok_or(CoreError::UnknownDevice(id))?;
        let _ = tx.send(accept);
        Ok(())
    }

    pub fn unpair(&self, id: DeviceId) -> Result<()> {
        self.inner.unpair(id)
    }

    /// Queue a packet for a connected (or recently connected) device.
    pub fn send(&self, id: DeviceId, packet: OutboundPacket) -> Result<Option<u64>> {
        let st = self.inner.lock();
        let peer = st.peers.get(&id).ok_or(CoreError::NotConnected(id))?;
        if let Some(identity) = &peer.identity
            && !accepts(identity, &packet.ty)
        {
            return Err(CoreError::Unsupported(packet.ty));
        }
        peer.session.send(packet)
    }

    /// Tell every transport that the network changed (apps call this from OS callbacks).
    pub async fn network_changed(&self) {
        for t in &self.inner.transports {
            t.network_changed().await;
        }
    }

    pub async fn shutdown(&self) {
        self.inner.shutdown().await;
    }
}

struct Peer {
    session: Arc<Session>,
    identity: Option<Arc<Identity>>,
    /// Serializes channel replacement for this peer.
    attach_lock: Arc<tokio::sync::Mutex<()>>,
    task: AbortHandle,
}

#[derive(Default)]
struct State {
    candidates: HashMap<DeviceId, Vec<PeerCandidate>>,
    peers: HashMap<DeviceId, Peer>,
    connecting: HashSet<DeviceId>,
    /// Devices whose session is being moved to a better link.
    upgrading: HashSet<DeviceId>,
    /// Wakes a device's reconnect or upgrade loop early when a new way to reach it appears.
    wake: HashMap<DeviceId, Arc<Notify>>,
    pairings: HashMap<DeviceId, oneshot::Sender<bool>>,
    /// Incoming pairing requests are refused until then (one was just declined or ignored).
    pairing_cooldown: Option<Instant>,
    /// The QR code currently on screen.
    qr: Option<ActiveQr>,
    shutdown: bool,
}

struct ActiveQr {
    psk: [u8; PSK_LEN],
    expires: Instant,
}

/// How to dial a candidate (an owned [`Initiate`], so attempts can run in parallel).
#[derive(Clone)]
enum Dial {
    Pair,
    PairPsk([u8; PSK_LEN]),
    Reconnect(PublicKey),
}

impl Dial {
    fn initiate(&self) -> Initiate<'_> {
        match self {
            Self::Pair => Initiate::Pair,
            Self::PairPsk(psk) => Initiate::PairPsk(psk),
            Self::Reconnect(key) => Initiate::Reconnect(key),
        }
    }
}

/// Head start each candidate gets before the next one is dialed.
/// Inbound connections allowed in their handshake at once; more are dropped unanswered, so a
/// flood of half-open connections can't pile up.
const MAX_INBOUND_HANDSHAKES: usize = 16;
/// Pairing prompts waiting for the user at once.
const MAX_PENDING_PAIRINGS: usize = 3;
/// After an incoming request is declined or ignored, refuse new ones this long (the device ID
/// is free to change, so this is per node, not per device).
const PAIRING_COOLDOWN: Duration = Duration::from_secs(10);
const DIAL_STAGGER: Duration = Duration::from_millis(200);
/// First retry delay when moving to a better link fails (doubles up to the maximum).
const UPGRADE_RETRY: Duration = Duration::from_secs(10);
const UPGRADE_RETRY_MAX: Duration = Duration::from_secs(300);

struct Inner {
    identity: IdentityKeypair,
    local: Identity,
    config: NodeConfig,
    registry: Registry,
    platform: Arc<dyn Platform>,
    transports: Vec<Arc<dyn Transport>>,
    plugins: Vec<Arc<dyn Plugin>>,
    routes: HashMap<String, Arc<dyn Plugin>>,
    events: broadcast::Sender<NodeEvent>,
    state: Mutex<State>,
    tasks: Mutex<Vec<AbortHandle>>,
    /// Inbound connections still in their (unauthenticated) handshake.
    inbound: Arc<tokio::sync::Semaphore>,
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn device_id(&self) -> DeviceId {
        self.identity.device_id()
    }

    fn emit(&self, event: NodeEvent) {
        debug!(?event, "node event");
        let _ = self.events.send(event);
    }

    fn spawn(&self, fut: impl Future<Output = ()> + Send + 'static) {
        let handle = tokio::spawn(fut).abort_handle();
        let mut tasks = self.tasks.lock().unwrap_or_else(PoisonError::into_inner);
        tasks.retain(|t| !t.is_finished());
        tasks.push(handle);
    }

    fn is_paired(&self, id: &DeviceId) -> bool {
        self.registry.get(id).ok().flatten().is_some()
    }

    /// The link a device's session is on, if connected.
    fn link(&self, id: &DeviceId) -> Option<TransportKind> {
        self.lock()
            .peers
            .get(id)
            .and_then(|p| p.session.transport())
    }

    fn wake(&self, id: DeviceId) -> Arc<Notify> {
        self.lock().wake.entry(id).or_default().clone()
    }

    /// Tell transports about paired devices and their links (the relay joins their rooms).
    fn update_transport_peers(&self) {
        let devices = match self.registry.list() {
            Ok(d) => d,
            Err(e) => {
                debug!(error = %e, "can't list paired devices");
                return;
            }
        };
        let peers: Vec<PairedPeer> = devices
            .into_iter()
            .map(|d| {
                let mut relays: Vec<String> = self.config.relay.iter().cloned().collect();
                if let Some(r) = d.relay
                    && !relays.contains(&r)
                {
                    relays.push(r);
                }
                PairedPeer {
                    id: d.id,
                    rendezvous: rendezvous(&d.pair_secret),
                    relays,
                    link: self.link(&d.id),
                    bluetooth: d.bluetooth,
                }
            })
            .collect();
        for t in &self.transports {
            t.peers_changed(&peers);
        }
    }

    // ----- discovery ---------------------------------------------------------------------

    fn on_discovered(self: &Arc<Self>, c: PeerCandidate) {
        let Some(id) = c.device_id else { return };
        if id == self.device_id() {
            return;
        }
        let first = {
            let mut st = self.lock();
            if st.shutdown {
                return;
            }
            let list = st.candidates.entry(id).or_default();
            if !list.contains(&c) {
                list.push(c.clone());
            }
            list.len() == 1
        };
        if first {
            self.emit(NodeEvent::DeviceDiscovered {
                id,
                name: c.name.clone(),
                transport: c.transport,
            });
        }
        if !self.is_paired(&id) {
            return;
        }
        // New or seen again: a waiting reconnect or upgrade can try it now.
        self.wake(id).notify_one();
        match self.link(&id) {
            None => self.spawn_connect(id),
            Some(link) if link.rank() < c.transport.rank() => self.spawn_upgrade(id),
            Some(_) => {}
        }
    }

    fn on_lost(&self, c: &PeerCandidate) {
        let Some(id) = c.device_id else { return };
        let gone = {
            let mut st = self.lock();
            let Some(list) = st.candidates.get_mut(&id) else {
                return;
            };
            list.retain(|x| x != c);
            let gone = list.is_empty();
            if gone {
                st.candidates.remove(&id);
            }
            gone
        };
        if gone {
            self.emit(NodeEvent::DeviceLost { id });
        }
    }

    // ----- outbound connections ----------------------------------------------------------

    /// Keep trying to reach a paired device while it is discoverable and not connected.
    fn spawn_connect(self: &Arc<Self>, id: DeviceId) {
        if !self.lock().connecting.insert(id) {
            return;
        }
        let inner = self.clone();
        self.spawn(async move {
            let mut delay = Duration::from_millis(500);
            loop {
                let wanted = {
                    let st = inner.lock();
                    !st.shutdown
                        && st.candidates.get(&id).is_some_and(|c| !c.is_empty())
                        && !st.peers.get(&id).is_some_and(|p| p.session.is_connected())
                };
                if !wanted || !inner.is_paired(&id) {
                    break;
                }
                match inner.connect_once(id).await {
                    Ok(()) => break,
                    Err(e) => debug!(%id, error = %e, "reconnect attempt failed"),
                }
                let wake = inner.wake(id);
                tokio::select! {
                    () = tokio::time::sleep(delay) => {}
                    () = wake.notified() => {}
                }
                delay = (delay * 2).min(inner.config.reconnect_max_backoff);
            }
            inner.lock().connecting.remove(&id);
        });
    }

    /// The device is connected, but a better kind of link to it appeared (back on the home
    /// network while on the relay): open a channel there and move the session onto it. The
    /// session resends anything unacknowledged, so nothing is lost or delivered twice.
    fn spawn_upgrade(self: &Arc<Self>, id: DeviceId) {
        if !self.lock().upgrading.insert(id) {
            return;
        }
        let inner = self.clone();
        self.spawn(async move {
            let mut delay = UPGRADE_RETRY;
            loop {
                if inner.lock().shutdown {
                    break;
                }
                // Disconnected: the reconnect loop takes over.
                let Some(current) = inner.link(&id) else { break };
                let better: Vec<PeerCandidate> = inner
                    .candidates(id)
                    .into_iter()
                    .filter(|c| c.transport.rank() > current.rank())
                    .collect();
                if better.is_empty() {
                    break;
                }
                let pause = match inner.connect_via(id, better).await {
                    Ok(()) => {
                        info!(%id, from = ?current, to = ?inner.link(&id), "moved to a better link");
                        delay = UPGRADE_RETRY;
                        Duration::from_secs(1)
                    }
                    Err(e) => {
                        debug!(%id, error = %e, "better link not reachable yet");
                        let pause = delay;
                        delay = (delay * 2).min(UPGRADE_RETRY_MAX);
                        pause
                    }
                };
                let wake = inner.wake(id);
                tokio::select! {
                    () = tokio::time::sleep(pause) => {}
                    () = wake.notified() => {}
                }
            }
            inner.lock().upgrading.remove(&id);
        });
    }

    async fn connect_via(
        self: &Arc<Self>,
        id: DeviceId,
        candidates: Vec<PeerCandidate>,
    ) -> Result<()> {
        let device = self.registry.get(&id)?.ok_or(CoreError::NotPaired(id))?;
        let ch = self
            .connect_any(candidates, Dial::Reconnect(device.public_key))
            .await?;
        self.establish(ch).await
    }

    async fn connect_once(self: &Arc<Self>, id: DeviceId) -> Result<()> {
        self.connect_via(id, self.candidates(id)).await
    }

    fn candidates(&self, id: DeviceId) -> Vec<PeerCandidate> {
        self.lock().candidates.get(&id).cloned().unwrap_or_default()
    }

    /// Dial all candidates, best kind of link first and each [`DIAL_STAGGER`] after the
    /// previous, and keep the first channel that completes its handshake (the rest are
    /// cancelled). A device often has several addresses, and some (VPNs, other subnets) just
    /// time out.
    async fn connect_any(
        self: &Arc<Self>,
        mut candidates: Vec<PeerCandidate>,
        dial: Dial,
    ) -> Result<Channel> {
        candidates.sort_by_key(|c| std::cmp::Reverse(c.transport.rank()));
        let mut attempts = JoinSet::new();
        for (i, c) in candidates.into_iter().enumerate() {
            let (inner, dial) = (self.clone(), dial.clone());
            let delay = DIAL_STAGGER.saturating_mul(u32::try_from(i).unwrap_or(u32::MAX));
            attempts.spawn(async move {
                tokio::time::sleep(delay).await;
                inner.open(&c, dial.initiate()).await
            });
        }
        let mut last = CoreError::Transport("no address to try".into());
        while let Some(result) = attempts.join_next().await {
            match result {
                Ok(Ok(ch)) => return Ok(ch),
                Ok(Err(e)) => last = e,
                Err(e) => last = CoreError::Transport(e.to_string()),
            }
        }
        Err(last)
    }

    async fn open(&self, c: &PeerCandidate, how: Initiate<'_>) -> Result<Channel> {
        let transport = self
            .transports
            .iter()
            .find(|t| t.kind() == c.transport)
            .ok_or_else(|| CoreError::Transport(format!("no {:?} transport", c.transport)))?;
        let stream = transport.connect(c).await?;
        channel::connect(stream, c.transport, how, &self.identity).await
    }

    // ----- inbound connections -----------------------------------------------------------

    async fn handle_incoming(
        self: &Arc<Self>,
        stream: BoxDuplex,
        transport: TransportKind,
        remote: Option<String>,
        permit: tokio::sync::OwnedSemaphorePermit,
    ) -> Result<()> {
        let ch = channel::accept(stream, transport, &self.identity, &Policy(self)).await;
        // Authenticated (or failed): no longer a half-open connection.
        drop(permit);
        let mut ch = ch?;
        // A QR code pairs exactly once: the first completed handshake consumes it.
        if ch.info.kind == HandshakeKind::PairPsk && self.lock().qr.take().is_none() {
            return Err(CoreError::PairingDisabled);
        }
        match ch.info.kind {
            HandshakeKind::Reconnect => {
                let peer = self.exchange_identity(&mut ch).await?;
                // A paired device dialed us over Bluetooth: now we can dial it back (phones
                // can't tell us their address themselves). Only once it proved its key live: a
                // replayed first handshake message completes the responder's side of IK, but
                // can't decrypt or send the identity.
                if transport == TransportKind::Bluetooth
                    && let Some(address) = remote.as_deref()
                    && pairly_proto::packets::is_bluetooth_address(address)
                    && self
                        .registry
                        .set_bluetooth(&ch.peer_id(), address)
                        .unwrap_or(false)
                {
                    self.update_transport_peers();
                }
                self.attach(ch, peer).await
            }
            HandshakeKind::Pair | HandshakeKind::PairPsk => self.run_pairing(ch).await,
        }
    }

    // ----- sessions ----------------------------------------------------------------------

    async fn exchange_identity(&self, ch: &mut Channel) -> Result<Identity> {
        let exchange = async {
            ch.writer.send_body(&self.local).await?;
            let peer: Identity = ch.reader.recv_body().await?;
            peer.validate()?;
            Ok::<_, CoreError>(peer)
        };
        tokio::time::timeout(HANDSHAKE_TIMEOUT, exchange)
            .await
            .map_err(|_| CoreError::Timeout)?
    }

    async fn establish(self: &Arc<Self>, mut ch: Channel) -> Result<()> {
        let peer = self.exchange_identity(&mut ch).await?;
        self.attach(ch, peer).await
    }

    async fn attach(self: &Arc<Self>, mut ch: Channel, identity: Identity) -> Result<()> {
        let id = ch.peer_id();
        ch.writer
            .set_padding(ch.transport == TransportKind::Relay && self.config.relay_padding);
        if !self.registry.is_paired_key(&ch.info.remote)? {
            return Err(CoreError::NotPaired(id));
        }
        let (session, attach_lock) = {
            let mut st = self.lock();
            if st.shutdown {
                return Err(CoreError::Shutdown);
            }
            let peer = st.peers.entry(id).or_insert_with(|| self.new_peer(id));
            (peer.session.clone(), peer.attach_lock.clone())
        };
        let _guard = attach_lock.lock().await;
        if session.is_connected() && !self.replaces(&session, &ch, &identity) {
            debug!(%id, "keeping the existing channel");
            return Ok(());
        }
        if let Some(peer) = self.lock().peers.get_mut(&id) {
            peer.identity = Some(Arc::new(identity.clone()));
        }
        // The peer may have been renamed since it was paired.
        if let Err(e) = self.registry.set_name(&id, &identity.name) {
            debug!(%id, error = %e, "can't store the peer's name");
        }
        if let Err(e) = self.registry.set_relay(&id, identity.relay.as_deref()) {
            debug!(%id, error = %e, "can't store the peer's relay");
        }
        if let Some(address) = identity.bluetooth.as_deref() {
            match self.registry.set_bluetooth(&id, address) {
                Ok(true) => self.update_transport_peers(),
                Ok(false) => {}
                Err(e) => debug!(%id, error = %e, "can't store the peer's Bluetooth address"),
            }
        }
        let link = ch.transport;
        let previous = session.transport();
        session.attach(ch, identity).await;
        info!(%id, link = link.as_str(), previous = previous.map(TransportKind::as_str), "session on link");
        // Already on a lower-ranked link while a better one is known (e.g. the reconnect raced
        // a dead LAN address and the relay won): keep trying the better one.
        if self
            .candidates(id)
            .iter()
            .any(|c| c.transport.rank() > link.rank())
        {
            self.spawn_upgrade(id);
        }
        Ok(())
    }

    /// Whether `new` should replace the session's live channel. Both sides reach the same
    /// verdict, so a simultaneous connect settles on one channel.
    fn replaces(&self, session: &Session, new: &Channel, identity: &Identity) -> bool {
        if session.peer_session_nonce() != Some(identity.session_nonce) {
            return true; // the peer restarted, so the old channel is dead
        }
        if let Some(existing) = session.transport()
            && existing.rank() != new.transport.rank()
        {
            return new.transport.rank() > existing.rank();
        }
        let Some(existing_local) = session.locally_initiated() else {
            return true;
        };
        if existing_local == new.initiator {
            return true; // same direction: a reconnect
        }
        // Opposite directions: keep the channel opened by the lower device id.
        new.initiator == (self.device_id() < new.peer_id())
    }

    fn new_peer(self: &Arc<Self>, id: DeviceId) -> Peer {
        let (tx, rx) = mpsc::channel(256);
        let session = Arc::new(Session::new(id, self.config.session.clone(), tx));
        let task =
            tokio::spawn(peer_loop(Arc::downgrade(self), id, session.clone(), rx)).abort_handle();
        Peer {
            session,
            identity: None,
            attach_lock: Arc::default(),
            task,
        }
    }

    // ----- pairing -----------------------------------------------------------------------

    async fn request_pair(self: &Arc<Self>, id: DeviceId) -> Result<()> {
        if id == self.device_id() {
            return Err(CoreError::Violation("cannot pair with ourselves"));
        }
        let candidates = self.candidates(id);
        if candidates.is_empty() {
            return Err(CoreError::UnknownDevice(id));
        }
        let ch = self.connect_any(candidates, Dial::Pair).await?;
        if ch.peer_id() != id {
            return Err(CoreError::Violation("a different device answered"));
        }
        let inner = self.clone();
        self.spawn(async move {
            let _ = inner.run_pairing(ch).await;
        });
        Ok(())
    }

    fn start_qr_pairing(&self) -> Result<QrInvite> {
        let addresses = self
            .transports
            .iter()
            .flat_map(|t| t.listen_addresses().into_iter().map(move |a| (t.kind(), a)))
            .collect();
        let invite = QrInvite {
            public_key: self.identity.public(),
            secret: rand::random(),
            addresses,
        };
        let mut st = self.lock();
        if st.shutdown {
            return Err(CoreError::Shutdown);
        }
        st.qr = Some(ActiveQr {
            psk: invite.psk(),
            expires: Instant::now() + self.config.qr_timeout,
        });
        Ok(invite)
    }

    async fn pair_from_qr(self: &Arc<Self>, invite: QrInvite) -> Result<()> {
        let id = invite.device_id();
        if id == self.device_id() {
            return Err(CoreError::Violation("cannot pair with ourselves"));
        }
        let mut candidates: Vec<PeerCandidate> = invite
            .addresses
            .iter()
            .filter(|(kind, _)| self.transports.iter().any(|t| t.kind() == *kind))
            .map(|(kind, address)| PeerCandidate {
                transport: *kind,
                address: address.clone(),
                device_id: Some(id),
                name: None,
            })
            .collect();
        for c in self.candidates(id) {
            if !candidates.contains(&c) {
                candidates.push(c);
            }
        }
        if candidates.is_empty() {
            return Err(CoreError::Violation(
                "no way to reach the device in that code",
            ));
        }
        let ch = self
            .connect_any(candidates, Dial::PairPsk(invite.psk()))
            .await?;
        // The PSK proves we saw the code; the pinned key proves who answered.
        if ch.info.remote != invite.public_key {
            return Err(CoreError::Violation("a different device answered"));
        }
        self.run_pairing(ch).await
    }

    async fn run_pairing(self: &Arc<Self>, mut ch: Channel) -> Result<()> {
        let id = ch.peer_id();
        let result = async {
            let peer = self.exchange_identity(&mut ch).await?;
            let sas = tokio::time::timeout(HANDSHAKE_TIMEOUT, pairing::exchange(&mut ch))
                .await
                .map_err(|_| CoreError::Timeout)??;
            if ch.info.kind == HandshakeKind::PairPsk {
                // Scanning the code was the confirmation; the PSK authenticated it.
                return Ok((peer, sas));
            }
            let (tx, rx) = oneshot::channel();
            {
                let mut st = self.lock();
                if st.shutdown {
                    return Err(CoreError::Shutdown);
                }
                // Don't let unknown devices stack up prompts.
                if !ch.initiator && st.pairings.len() >= MAX_PENDING_PAIRINGS {
                    return Err(CoreError::PairingDisabled);
                }
                st.pairings.insert(id, tx);
            }
            self.emit(NodeEvent::PairingRequested {
                id,
                name: peer.name.clone(),
                code: sas.code,
                incoming: !ch.initiator,
            });
            let confirmed =
                tokio::time::timeout(self.config.pairing_timeout, pairing::confirm(&mut ch, rx))
                    .await;
            {
                // Drop our entry if it is still there (e.g. on timeout), but not a newer request's.
                let mut st = self.lock();
                if st.pairings.get(&id).is_some_and(oneshot::Sender::is_closed) {
                    st.pairings.remove(&id);
                }
            }
            let confirmed = confirmed.map_err(|_| CoreError::Timeout).and_then(|r| r);
            if confirmed.is_err() && !ch.initiator {
                self.lock().pairing_cooldown = Some(Instant::now() + PAIRING_COOLDOWN);
            }
            confirmed?;
            Ok((peer, sas))
        }
        .await;

        let (peer, sas) = match result {
            Ok(ok) => ok,
            Err(e) => {
                self.emit(NodeEvent::PairingFailed {
                    id,
                    reason: e.to_string(),
                });
                return Err(e);
            }
        };
        self.registry.upsert(&PairedDevice {
            id,
            public_key: ch.info.remote,
            name: peer.name.clone(),
            device_type: peer.device_type,
            pair_secret: sas.pair_secret,
            paired_at: unix_now(),
            relay: peer.relay.clone(),
            bluetooth: peer.bluetooth.clone(),
        })?;
        info!(%id, name = %peer.name, "paired");
        self.emit(NodeEvent::Paired {
            id,
            name: peer.name.clone(),
        });
        self.attach(ch, peer).await
    }

    fn unpair(&self, id: DeviceId) -> Result<()> {
        let removed = self.registry.remove(&id)?;
        let peer = self.lock().peers.remove(&id);
        let had_peer = peer.is_some();
        if let Some(peer) = peer {
            peer.task.abort();
            peer.session.detach();
        }
        if removed || had_peer {
            self.emit(NodeEvent::Unpaired { id });
        }
        self.update_transport_peers();
        Ok(())
    }

    fn devices(&self) -> Result<Vec<DeviceInfo>> {
        let paired = self.registry.list()?;
        let st = self.lock();
        let mut out: Vec<DeviceInfo> = paired
            .into_iter()
            .map(|d| {
                let session = st.peers.get(&d.id).map(|p| &p.session);
                DeviceInfo {
                    id: d.id,
                    name: d.name,
                    device_type: Some(d.device_type),
                    paired: true,
                    link: session.and_then(|s| s.transport()),
                    rtt: session.and_then(|s| s.rtt()),
                }
            })
            .collect();
        for (id, candidates) in &st.candidates {
            if out.iter().any(|d| d.id == *id) {
                continue;
            }
            out.push(DeviceInfo {
                id: *id,
                name: candidates
                    .iter()
                    .find_map(|c| c.name.clone())
                    .unwrap_or_else(|| id.to_string()),
                device_type: None,
                paired: false,
                link: None,
                rtt: None,
            });
        }
        Ok(out)
    }

    async fn shutdown(&self) {
        let peers: Vec<Peer> = {
            let mut st = self.lock();
            st.shutdown = true;
            st.pairings.clear();
            st.peers.drain().map(|(_, p)| p).collect()
        };
        for task in std::mem::take(&mut *self.tasks.lock().unwrap_or_else(PoisonError::into_inner))
        {
            task.abort();
        }
        for peer in peers {
            peer.task.abort();
            peer.session.close(Duration::from_millis(500)).await;
        }
        for t in &self.transports {
            t.stop().await;
        }
        info!(id = %self.device_id(), "node stopped");
    }
}

struct Policy<'a>(&'a Inner);

impl AcceptPolicy for Policy<'_> {
    fn is_paired(&self, key: &PublicKey) -> bool {
        self.0.registry.is_paired_key(key).unwrap_or(false)
    }

    fn allow_pairing(&self) -> bool {
        let st = self.0.lock();
        self.0.config.allow_pairing
            && !st.shutdown
            && st
                .pairing_cooldown
                .is_none_or(|until| Instant::now() >= until)
    }

    fn pairing_psk(&self) -> Option<[u8; PSK_LEN]> {
        let st = self.0.lock();
        st.qr
            .as_ref()
            .filter(|qr| qr.expires > Instant::now())
            .map(|qr| qr.psk)
    }
}

async fn main_loop(inner: Arc<Inner>, mut rx: mpsc::Receiver<TransportEvent>) {
    while let Some(event) = rx.recv().await {
        match event {
            TransportEvent::Discovered(c) => inner.on_discovered(c),
            TransportEvent::Lost(c) => inner.on_lost(&c),
            TransportEvent::Incoming {
                stream,
                transport,
                remote,
            } => {
                let Ok(permit) = inner.inbound.clone().try_acquire_owned() else {
                    debug!("too many connections in their handshake; dropping one");
                    continue;
                };
                let node = inner.clone();
                inner.spawn(async move {
                    if let Err(e) = node
                        .handle_incoming(stream, transport, remote, permit)
                        .await
                    {
                        debug!(error = %e, "inbound connection failed");
                    }
                });
            }
        }
    }
}

/// Delivers one peer's session events to plugins, in order.
async fn peer_loop(
    inner: Weak<Inner>,
    id: DeviceId,
    session: Arc<Session>,
    mut rx: mpsc::Receiver<SessionEvent>,
) {
    let mut ctx: Option<PluginCtx> = None;
    while let Some(event) = rx.recv().await {
        let Some(inner) = inner.upgrade() else { return };
        match event {
            SessionEvent::Connected {
                transport, peer, ..
            } => {
                let c = PluginCtx {
                    peer: id,
                    identity: Arc::new(peer),
                    session: session.clone(),
                    platform: inner.platform.clone(),
                };
                // Plugins first, so anyone reacting to `Connected` can use them right away.
                for p in &inner.plugins {
                    p.on_connected(&c).await;
                }
                ctx = Some(c);
                inner.emit(NodeEvent::Connected { id, transport });
                inner.update_transport_peers();
            }
            SessionEvent::Packet(env) => {
                let Some(c) = &ctx else { continue };
                match inner.routes.get(&env.ty) {
                    Some(p) => p.on_packet(c, env).await,
                    None => debug!(%id, ty = %env.ty, "no plugin for packet"),
                }
            }
            SessionEvent::Disconnected { generation, reason } => {
                if generation != session.generation() {
                    continue;
                }
                inner.emit(NodeEvent::Disconnected { id, reason });
                inner.update_transport_peers();
                for p in &inner.plugins {
                    p.on_disconnected(id).await;
                }
                inner.spawn_connect(id);
            }
        }
    }
}
