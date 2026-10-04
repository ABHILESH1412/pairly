//! Bluetooth transport: registers an RFCOMM profile with the Pairly service UUID in BlueZ and
//! exposes connections to bonded devices as byte streams for the core.
//!
//! There is no discovery: Bluetooth only reaches devices we know. A paired peer's address comes
//! from its identity packet (PCs announce theirs) or from its first Bluetooth connection to us
//! (phones can't read their own address), and each known address is offered as a candidate.
//! Dialing asks BlueZ to connect the profile; BlueZ looks up the peer's RFCOMM channel via SDP
//! and hands the socket to our profile, the same way inbound connections arrive.
//!
//! Both sides must be bonded at the OS level first. Pairly's Noise pairing still runs on top,
//! so Bluetooth's own link security is just an extra layer.
#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use bluer::rfcomm::{Profile, ProfileHandle};
use bluer::{Address, Uuid};
use futures_util::StreamExt;
use pairly_core::transport::{Advertisement, BLUETOOTH_SERVICE_UUID, BoxDuplex, PairedPeer};
use pairly_core::{
    CoreError, KnownAddresses, PeerCandidate, Result, Transport, TransportEvent, TransportKind,
};
use tokio::sync::{mpsc, oneshot};
use tokio::task::AbortHandle;
use tracing::{debug, info, warn};

const RFCOMM_CHANNEL: u16 = 22;

/// Paging a device that is out of range takes a while to fail.
const DIAL_TIMEOUT: Duration = Duration::from_secs(20);

fn err(e: impl std::fmt::Display) -> CoreError {
    CoreError::Transport(e.to_string())
}

fn service_uuid() -> Uuid {
    Uuid::from_str(BLUETOOTH_SERVICE_UUID).expect("the service UUID constant is valid")
}

type Pending = Arc<Mutex<HashMap<Address, oneshot::Sender<bluer::rfcomm::Stream>>>>;

struct Running {
    adapter: bluer::Adapter,
    events: mpsc::Sender<TransportEvent>,
    /// Our own dials, waiting for BlueZ to hand over the socket.
    pending: Pending,
    /// Candidates we reported.
    known: KnownAddresses,
    accept: AbortHandle,
}

pub struct BluetoothTransport {
    session: bluer::Session,
    adapter: bluer::Adapter,
    running: Mutex<Option<Running>>,
}

impl BluetoothTransport {
    /// Connect to BlueZ. Fails if there is no Bluetooth adapter or `bluetoothd` isn't running.
    pub async fn new() -> Result<Arc<Self>> {
        let session = bluer::Session::new().await.map_err(err)?;
        let adapter = session.default_adapter().await.map_err(err)?;
        Ok(Arc::new(Self {
            session,
            adapter,
            running: Mutex::new(None),
        }))
    }

    /// This PC's Bluetooth address, to announce to paired devices.
    pub async fn address(&self) -> Option<String> {
        self.adapter.address().await.ok().map(|a| a.to_string())
    }

    fn lock(&self) -> MutexGuard<'_, Option<Running>> {
        self.running.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[async_trait]
impl Transport for BluetoothTransport {
    fn kind(&self) -> TransportKind {
        TransportKind::Bluetooth
    }

    async fn start(
        &self,
        _advert: Advertisement,
        events: mpsc::Sender<TransportEvent>,
    ) -> Result<()> {
        let profile = Profile {
            uuid: service_uuid(),
            name: Some("Pairly".into()),
            // Both: we accept connections and dial them.
            role: None,
            // Without a channel BlueZ starts no listener for a custom UUID, so it publishes no
            // SDP record and phones can't find us. 22 is clear of the standard profiles (DUN,
            // HFP, OBEX use low and 9-13 channels).
            channel: Some(RFCOMM_CHANNEL),
            // Only bonded devices, without asking the user every time.
            require_authentication: Some(true),
            require_authorization: Some(false),
            auto_connect: Some(false),
            ..Default::default()
        };
        let handle = self.session.register_profile(profile).await.map_err(err)?;
        let pending = Pending::default();
        let accept =
            tokio::spawn(accept_loop(handle, pending.clone(), events.clone())).abort_handle();
        info!(
            adapter = self.adapter.name(),
            "Bluetooth profile registered"
        );
        *self.lock() = Some(Running {
            adapter: self.adapter.clone(),
            events,
            pending,
            known: KnownAddresses::new(TransportKind::Bluetooth),
            accept,
        });
        Ok(())
    }

    fn peers_changed(&self, peers: &[PairedPeer]) {
        let mut guard = self.lock();
        let Some(running) = guard.as_mut() else {
            return;
        };
        let events = running.known.update(
            peers
                .iter()
                .filter_map(|p| Some((p.bluetooth.clone()?, p.id))),
        );
        if events.is_empty() {
            return;
        }
        let tx = running.events.clone();
        tokio::spawn(async move {
            for e in events {
                let _ = tx.send(e).await;
            }
        });
    }

    async fn connect(&self, candidate: &PeerCandidate) -> Result<BoxDuplex> {
        let address = Address::from_str(&candidate.address).map_err(err)?;
        let (adapter, pending) = {
            let guard = self.lock();
            let running = guard.as_ref().ok_or_else(|| err("Bluetooth not started"))?;
            (running.adapter.clone(), running.pending.clone())
        };
        if !adapter.is_powered().await.unwrap_or(false) {
            return Err(err("Bluetooth is off"));
        }
        let device = adapter.device(address).map_err(err)?;
        if !device.is_paired().await.unwrap_or(false) {
            return Err(err(format!(
                "{address} isn't paired with this PC in Bluetooth settings"
            )));
        }
        let (tx, rx) = oneshot::channel();
        lock(&pending).insert(address, tx);
        let dial = async {
            device.connect_profile(&service_uuid()).await.map_err(err)?;
            rx.await
                .map_err(|_| err("BlueZ didn't hand over the connection"))
        };
        let result = tokio::time::timeout(DIAL_TIMEOUT, dial).await;
        lock(&pending).remove(&address);
        let stream = result.map_err(|_| CoreError::Timeout)??;
        debug!(%address, "Bluetooth connected");
        Ok(Box::new(stream))
    }

    async fn stop(&self) {
        if let Some(running) = self.lock().take() {
            // Dropping the profile handle (in the accept task) unregisters it.
            running.accept.abort();
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// BlueZ delivers every RFCOMM connection for our UUID here: answers to our own dials (handed to
/// the waiting `connect`) and devices calling us.
async fn accept_loop(
    mut handle: ProfileHandle,
    pending: Pending,
    events: mpsc::Sender<TransportEvent>,
) {
    while let Some(request) = handle.next().await {
        let address = request.device();
        let waiting = lock(&pending).remove(&address);
        let stream = match request.accept() {
            Ok(s) => s,
            Err(e) => {
                warn!(%address, error = %e, "can't accept a Bluetooth connection");
                continue;
            }
        };
        match waiting {
            Some(dialer) => {
                let _ = dialer.send(stream);
            }
            None => {
                debug!(%address, "Bluetooth connection from a device");
                let incoming = TransportEvent::Incoming {
                    stream: Box::new(stream),
                    transport: TransportKind::Bluetooth,
                    remote: Some(address.to_string()),
                };
                if events.send(incoming).await.is_err() {
                    return;
                }
            }
        }
    }
    warn!("BlueZ dropped the Pairly Bluetooth profile");
}
