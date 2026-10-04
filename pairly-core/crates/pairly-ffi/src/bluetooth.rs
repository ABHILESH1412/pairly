//! Bluetooth on Android: Kotlin owns the RFCOMM sockets (only the Java API can open them) and
//! Rust drives them through [`BluetoothHandler`] and [`BluetoothSocket`]. Each socket becomes a
//! normal byte stream for the core: a thread does the blocking reads, and writes run on Tokio's
//! blocking pool, in order.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use pairly_core::transport::{Advertisement, BoxDuplex, PairedPeer};
use pairly_core::{
    CoreError, KnownAddresses, PeerCandidate, Result, Transport, TransportEvent, TransportKind,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tracing::debug;

use crate::PairlyError;

const DIAL_TIMEOUT: Duration = Duration::from_secs(20);
const READ_SIZE: u32 = 16 * 1024;

/// One RFCOMM connection, implemented in Kotlin. Called from Rust threads.
#[uniffi::export(with_foreign)]
pub trait BluetoothSocket: Send + Sync {
    /// Block until data arrives; at most `max` bytes. Empty at the end of the stream.
    fn read(&self, max: u32) -> Result<Vec<u8>, PairlyError>;
    fn write(&self, data: Vec<u8>) -> Result<(), PairlyError>;
    /// Close the socket; unblocks a pending `read`. (Not `close`: UniFFI's generated Kotlin
    /// classes already have that.)
    fn disconnect(&self);
}

/// Opens RFCOMM connections, implemented in Kotlin.
#[uniffi::export(with_foreign)]
pub trait BluetoothHandler: Send + Sync {
    /// Connect to Pairly on the bonded device `address` (blocking).
    fn connect(&self, address: String) -> Result<Arc<dyn BluetoothSocket>, PairlyError>;
}

/// Pairly's RFCOMM service UUID, for Kotlin's server socket.
#[uniffi::export]
pub fn bluetooth_service_uuid() -> String {
    pairly_core::transport::BLUETOOTH_SERVICE_UUID.to_owned()
}

/// Wrap a socket as a byte stream: core ⇄ duplex pipe ⇄ (reader thread, writer task) ⇄ socket.
pub(crate) fn stream(socket: Arc<dyn BluetoothSocket>) -> BoxDuplex {
    let (core_end, bridge) = tokio::io::duplex(64 * 1024);
    let (mut from_core, mut to_core) = tokio::io::split(bridge);

    // Socket → core.
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(16);
    let reader = socket.clone();
    std::thread::Builder::new()
        .name("pairly-bt-read".into())
        .spawn(move || {
            while let Ok(data) = reader.read(READ_SIZE) {
                if data.is_empty() || tx.blocking_send(data).is_err() {
                    break;
                }
            }
        })
        .ok();
    tokio::spawn(async move {
        while let Some(data) = rx.recv().await {
            if to_core.write_all(&data).await.is_err() {
                break;
            }
        }
        let _ = to_core.shutdown().await;
    });

    // Core → socket. When either side ends, closing the socket also stops the reader.
    tokio::spawn(async move {
        let mut buf = vec![0u8; READ_SIZE as usize];
        loop {
            let n = match from_core.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            let (s, data) = (socket.clone(), buf[..n].to_vec());
            match tokio::task::spawn_blocking(move || s.write(data)).await {
                Ok(Ok(())) => {}
                _ => break,
            }
        }
        let s = socket.clone();
        let _ = tokio::task::spawn_blocking(move || s.disconnect()).await;
    });
    Box::new(core_end)
}

struct State {
    events: Option<mpsc::Sender<TransportEvent>>,
    known: KnownAddresses,
}

pub(crate) struct ForeignBluetooth {
    handler: Arc<dyn BluetoothHandler>,
    state: Mutex<State>,
}

impl ForeignBluetooth {
    pub fn new(handler: Arc<dyn BluetoothHandler>) -> Arc<Self> {
        Arc::new(Self {
            handler,
            state: Mutex::new(State {
                events: None,
                known: KnownAddresses::new(TransportKind::Bluetooth),
            }),
        })
    }

    /// A device connected to Kotlin's server socket. Called on a Kotlin thread.
    pub fn incoming(&self, socket: Arc<dyn BluetoothSocket>, address: String) {
        let events = self.lock().events.clone();
        let Some(events) = events else {
            socket.disconnect();
            return;
        };
        crate::runtime().spawn(async move {
            let event = TransportEvent::Incoming {
                stream: stream(socket),
                transport: TransportKind::Bluetooth,
                remote: Some(address),
            };
            let _ = events.send(event).await;
        });
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[async_trait]
impl Transport for ForeignBluetooth {
    fn kind(&self) -> TransportKind {
        TransportKind::Bluetooth
    }

    async fn start(
        &self,
        _advert: Advertisement,
        events: mpsc::Sender<TransportEvent>,
    ) -> Result<()> {
        self.lock().events = Some(events);
        Ok(())
    }

    fn peers_changed(&self, peers: &[PairedPeer]) {
        let (events, tx) = {
            let mut st = self.lock();
            let events = st.known.update(
                peers
                    .iter()
                    .filter_map(|p| Some((p.bluetooth.clone()?, p.id))),
            );
            (events, st.events.clone())
        };
        if let Some(tx) = tx.filter(|_| !events.is_empty()) {
            tokio::spawn(async move {
                for e in events {
                    let _ = tx.send(e).await;
                }
            });
        }
    }

    async fn connect(&self, candidate: &PeerCandidate) -> Result<BoxDuplex> {
        let (handler, address) = (self.handler.clone(), candidate.address.clone());
        let dial = tokio::task::spawn_blocking(move || handler.connect(address));
        let socket = tokio::time::timeout(DIAL_TIMEOUT, dial)
            .await
            .map_err(|_| CoreError::Timeout)?
            .map_err(|e| CoreError::Transport(e.to_string()))?
            .map_err(|e| CoreError::Transport(e.to_string()))?;
        debug!(address = %candidate.address, "Bluetooth connected");
        Ok(stream(socket))
    }

    async fn stop(&self) {
        let mut st = self.lock();
        st.known.clear();
        st.events = None;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::sync::mpsc as std_mpsc;

    use super::*;

    /// One end of an in-memory "RFCOMM" link, standing in for Kotlin's socket.
    struct FakeSocket {
        rx: Mutex<std_mpsc::Receiver<Vec<u8>>>,
        tx: Mutex<Option<std_mpsc::Sender<Vec<u8>>>>,
    }

    impl BluetoothSocket for FakeSocket {
        fn read(&self, max: u32) -> Result<Vec<u8>, PairlyError> {
            match self.rx.lock().unwrap().recv() {
                Ok(mut data) => {
                    data.truncate(max as usize);
                    Ok(data)
                }
                Err(_) => Ok(Vec::new()),
            }
        }
        fn write(&self, data: Vec<u8>) -> Result<(), PairlyError> {
            let tx = self.tx.lock().unwrap();
            let tx = tx.as_ref().ok_or_else(|| crate::failed("closed"))?;
            tx.send(data).map_err(crate::failed)
        }
        fn disconnect(&self) {
            self.tx.lock().unwrap().take();
        }
    }

    fn link() -> (Arc<dyn BluetoothSocket>, Arc<dyn BluetoothSocket>) {
        let (a_tx, b_rx) = std_mpsc::channel();
        let (b_tx, a_rx) = std_mpsc::channel();
        let end = |rx, tx| -> Arc<dyn BluetoothSocket> {
            Arc::new(FakeSocket {
                rx: Mutex::new(rx),
                tx: Mutex::new(Some(tx)),
            })
        };
        (end(a_rx, a_tx), end(b_rx, b_tx))
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn sockets_carry_a_noise_session() {
        use pairly_crypto::{IdentityKeypair, Initiate};
        let (a, b) = link();
        let (sa, sb) = (stream(a), stream(b));
        let (ka, kb) = (IdentityKeypair::generate(), IdentityKeypair::generate());
        // A full Noise XX handshake and a packet each way through the bridge.
        let client =
            pairly_core::channel::connect(sa, TransportKind::Bluetooth, Initiate::Pair, &ka);
        let server = pairly_core::channel::accept(sb, TransportKind::Bluetooth, &kb, &AcceptAll);
        let (ca, cb) = tokio::join!(client, server);
        let (mut ca, mut cb) = (ca.unwrap(), cb.unwrap());
        let big = pairly_proto::Envelope::new(1, false, &Blob(vec![7; 200_000])).unwrap();
        ca.writer.send(&big).await.unwrap();
        let got = cb.reader.recv().await.unwrap().unwrap();
        assert_eq!(got.body::<Blob>().unwrap().0.len(), 200_000);
        cb.writer.send(&big).await.unwrap();
        assert!(ca.reader.recv().await.unwrap().is_some());
        // Closing one side ends the other's stream.
        ca.writer.shutdown().await;
        drop(ca);
        assert!(cb.reader.recv().await.unwrap_or(None).is_none());
    }

    #[derive(serde::Serialize, serde::Deserialize)]
    struct Blob(#[serde(with = "serde_bytes")] Vec<u8>);
    impl pairly_proto::PacketBody for Blob {
        const TYPE: &'static str = "test.blob";
    }

    struct AcceptAll;
    impl pairly_core::channel::AcceptPolicy for AcceptAll {
        fn is_paired(&self, _: &pairly_crypto::PublicKey) -> bool {
            true
        }
        fn allow_pairing(&self) -> bool {
            true
        }
        fn pairing_psk(&self) -> Option<[u8; pairly_crypto::PSK_LEN]> {
            None
        }
    }
}
