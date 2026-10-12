//! Bluetooth transport, for when the phone and this PC share no network: RFCOMM sockets with
//! the Pairly service record (the same UUID as on Linux and Android), so a bonded phone finds
//! this PC by SDP and this PC dials a phone by its address.
//!
//! As on Linux there is no discovery: Bluetooth only reaches devices we know. A paired phone's
//! address comes from its first Bluetooth connection to us (phones can't read their own
//! address), and each known address is offered as a candidate. Both sides must be bonded in
//! Windows' Bluetooth settings first; Pairly's own pairing still runs on top.
//!
//! Winsock's Bluetooth sockets are blocking, so each connection gets two small threads that copy
//! between the socket and an in-memory pipe the core reads and writes.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use async_trait::async_trait;
use pairly_core::transport::{Advertisement, BoxDuplex, PairedPeer};
use pairly_core::{
    CoreError, KnownAddresses, PeerCandidate, Result, Transport, TransportEvent, TransportKind,
};
use tokio::sync::mpsc;

/// Paging a device that is out of range takes a while to fail.
const DIAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

fn err(e: impl std::fmt::Display) -> CoreError {
    CoreError::Transport(e.to_string())
}

struct Running {
    events: mpsc::Sender<TransportEvent>,
    known: KnownAddresses,
    listener: imp::Listener,
}

pub struct BluetoothTransport {
    address: String,
    running: Mutex<Option<Running>>,
}

impl BluetoothTransport {
    /// Fails if this PC has no Bluetooth radio (or it's switched off).
    pub fn new() -> anyhow::Result<Arc<Self>> {
        let address = imp::local_address()?;
        Ok(Arc::new(Self {
            address,
            running: Mutex::new(None),
        }))
    }

    /// This PC's Bluetooth address, to announce to paired devices.
    pub fn address(&self) -> Option<String> {
        Some(self.address.clone())
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
        let handle = tokio::runtime::Handle::current();
        let tx = events.clone();
        let listener = imp::Listener::start(move |stream, remote| {
            tracing::debug!(%remote, "Bluetooth connection from a device");
            let incoming = TransportEvent::Incoming {
                stream: Box::new(imp::bridge(stream, &handle)),
                transport: TransportKind::Bluetooth,
                remote: Some(remote),
            };
            tx.blocking_send(incoming).is_ok()
        })
        .map_err(err)?;
        tracing::info!(address = %self.address, "Bluetooth service registered");
        *self.lock() = Some(Running {
            events,
            known: KnownAddresses::new(TransportKind::Bluetooth),
            listener,
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
        if self.lock().is_none() {
            return Err(err("Bluetooth not started"));
        }
        let address =
            imp::parse_address(&candidate.address).ok_or_else(|| err("not a Bluetooth address"))?;
        let handle = tokio::runtime::Handle::current();
        let dial = tokio::task::spawn_blocking(move || imp::dial(address));
        let stream = tokio::time::timeout(DIAL_TIMEOUT, dial)
            .await
            .map_err(|_| CoreError::Timeout)?
            .map_err(err)?
            .map_err(err)?;
        tracing::debug!(address = %candidate.address, "Bluetooth connected");
        Ok(Box::new(imp::bridge(stream, &handle)))
    }

    async fn stop(&self) {
        if let Some(running) = self.lock().take() {
            running.listener.stop();
        }
    }
}

/// "AA:BB:CC:DD:EE:FF" from the 48-bit address.
pub fn format_address(a: u64) -> String {
    (0..6)
        .rev()
        .map(|i| format!("{:02X}", (a >> (i * 8)) & 0xFF))
        .collect::<Vec<_>>()
        .join(":")
}

#[cfg(windows)]
mod imp {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
    use windows::Win32::Devices::Bluetooth::{
        AF_BTH, BLUETOOTH_FIND_RADIO_PARAMS, BLUETOOTH_RADIO_INFO, BTHPROTO_RFCOMM,
        BluetoothFindFirstRadio, BluetoothFindRadioClose, BluetoothGetRadioInfo, NS_BTH,
        SOCKADDR_BTH,
    };
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Networking::WinSock::{
        CSADDR_INFO, INVALID_SOCKET, RNRSERVICE_DELETE, RNRSERVICE_REGISTER, SD_BOTH,
        SEND_RECV_FLAGS, SOCK_STREAM, SOCKADDR, SOCKET, SOCKET_ADDRESS, WSADATA, WSAQUERYSETW,
        WSASetServiceW, WSAStartup, accept, bind, closesocket, connect, getsockname, listen, recv,
        send, shutdown, socket,
    };
    use windows::core::{GUID, PWSTR};

    use super::format_address;

    /// Any free RFCOMM channel (`BT_PORT_ANY`, `(ULONG)-1`).
    const BT_PORT_ANY: u32 = u32::MAX;

    fn service_class() -> GUID {
        GUID::from_u128(0x9666e1eb_cdfa_4e10_aab2_648e9e26ac5d)
    }

    fn startup() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            let mut data = WSADATA::default();
            // SAFETY: an out-parameter; Winsock stays initialised for the app's life.
            let _ = unsafe { WSAStartup(0x0202, &mut data) };
        });
    }

    /// An open RFCOMM socket, closed when dropped.
    pub struct Socket(SOCKET);

    // SAFETY: a socket handle may be used from any thread.
    unsafe impl Send for Socket {}
    unsafe impl Sync for Socket {}

    impl Drop for Socket {
        fn drop(&mut self) {
            // SAFETY: our own socket, closed once.
            unsafe {
                let _ = shutdown(self.0, SD_BOTH);
                let _ = closesocket(self.0);
            }
        }
    }

    fn new_socket() -> std::io::Result<Socket> {
        startup();
        // SAFETY: plain socket creation.
        let s = unsafe { socket(i32::from(AF_BTH), SOCK_STREAM, BTHPROTO_RFCOMM as i32) }
            .map_err(std::io::Error::other)?;
        if s == INVALID_SOCKET {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Socket(s))
    }

    pub fn local_address() -> anyhow::Result<String> {
        let params = BLUETOOTH_FIND_RADIO_PARAMS {
            dwSize: std::mem::size_of::<BLUETOOTH_FIND_RADIO_PARAMS>() as u32,
        };
        let mut radio = HANDLE::default();
        // SAFETY: out-parameter; the find handle and radio handle are closed below.
        let find = unsafe { BluetoothFindFirstRadio(&params, &mut radio) }
            .map_err(|_| anyhow::anyhow!("this PC has no Bluetooth (or it's switched off)"))?;
        let mut info = BLUETOOTH_RADIO_INFO {
            dwSize: std::mem::size_of::<BLUETOOTH_RADIO_INFO>() as u32,
            ..Default::default()
        };
        // SAFETY: a valid radio handle and an out-parameter of the right size.
        let r = unsafe { BluetoothGetRadioInfo(radio, &mut info) };
        // SAFETY: handles from BluetoothFindFirstRadio.
        unsafe {
            let _ = BluetoothFindRadioClose(find);
            let _ = CloseHandle(radio);
        }
        anyhow::ensure!(r == 0, "can't read the Bluetooth radio");
        // SAFETY: the union's integer view of the address.
        let address = unsafe { info.address.Anonymous.ullLong };
        Ok(format_address(address))
    }

    pub fn parse_address(s: &str) -> Option<u64> {
        let parts: Vec<&str> = s.split(':').collect();
        if parts.len() != 6 {
            return None;
        }
        parts.iter().try_fold(0u64, |acc, p| {
            Some((acc << 8) | u64::from(u8::from_str_radix(p, 16).ok()?))
        })
    }

    /// Our listening socket with its service record; accepts on a thread of its own.
    pub struct Listener {
        socket: Arc<Socket>,
        stopped: Arc<AtomicBool>,
        record: Box<Record>,
    }

    /// The service record's pieces, kept alive while it's registered.
    struct Record {
        address: SOCKADDR_BTH,
        name: Vec<u16>,
        class: GUID,
        cs: CSADDR_INFO,
    }

    // SAFETY: the pointers inside point into the record itself (boxed, so they stay put), and
    // it's only used under the transport's lock.
    unsafe impl Send for Record {}
    unsafe impl Sync for Record {}

    impl Record {
        fn set(
            &mut self,
            op: windows::Win32::Networking::WinSock::WSAESETSERVICEOP,
        ) -> windows::core::Result<()> {
            self.cs.LocalAddr = SOCKET_ADDRESS {
                lpSockaddr: (&raw mut self.address).cast(),
                iSockaddrLength: std::mem::size_of::<SOCKADDR_BTH>() as i32,
            };
            let query = WSAQUERYSETW {
                dwSize: std::mem::size_of::<WSAQUERYSETW>() as u32,
                lpszServiceInstanceName: PWSTR(self.name.as_mut_ptr()),
                lpServiceClassId: &raw mut self.class,
                dwNameSpace: NS_BTH,
                dwNumberOfCsAddrs: 1,
                lpcsaBuffer: &raw mut self.cs,
                ..Default::default()
            };
            // SAFETY: every pointer in `query` points into `self`, alive for the call.
            let r = unsafe { WSASetServiceW(&query, op, 0) };
            if r != 0 {
                return Err(windows::core::Error::from_thread());
            }
            Ok(())
        }
    }

    impl Listener {
        pub fn start(
            on_connection: impl Fn(Socket, String) -> bool + Send + 'static,
        ) -> std::io::Result<Self> {
            let s = new_socket()?;
            let mut address = SOCKADDR_BTH {
                addressFamily: AF_BTH,
                btAddr: 0,
                serviceClassId: GUID::zeroed(),
                port: BT_PORT_ANY,
            };
            let len = std::mem::size_of::<SOCKADDR_BTH>() as i32;
            // SAFETY: a valid SOCKADDR_BTH of the given length.
            unsafe {
                if bind(s.0, (&raw const address).cast::<SOCKADDR>(), len) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if listen(s.0, 4) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // Which RFCOMM channel we got, for the service record.
                let mut got = len;
                if getsockname(s.0, (&raw mut address).cast::<SOCKADDR>(), &mut got) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            let mut record = Box::new(Record {
                address,
                name: "Pairly\0".encode_utf16().collect(),
                class: service_class(),
                cs: CSADDR_INFO {
                    iSocketType: SOCK_STREAM.0,
                    iProtocol: BTHPROTO_RFCOMM as i32,
                    ..Default::default()
                },
            });
            record
                .set(RNRSERVICE_REGISTER)
                .map_err(std::io::Error::other)?;
            let channel = { address.port };
            tracing::debug!(channel, "Bluetooth RFCOMM listening");
            let socket = Arc::new(s);
            let stopped = Arc::new(AtomicBool::new(false));
            let (sock, stop) = (socket.clone(), stopped.clone());
            std::thread::Builder::new()
                .name("pairly-bt-accept".into())
                .spawn(move || {
                    loop {
                        let mut from = SOCKADDR_BTH::default();
                        let mut len = std::mem::size_of::<SOCKADDR_BTH>() as i32;
                        // SAFETY: an out-parameter of the right size.
                        let client =
                            unsafe { accept(sock.0, Some((&raw mut from).cast()), Some(&mut len)) };
                        let client = match client {
                            Ok(c) if c != INVALID_SOCKET => Socket(c),
                            _ => {
                                if !stop.load(Ordering::SeqCst) {
                                    tracing::warn!("the Bluetooth listener stopped");
                                }
                                return;
                            }
                        };
                        if !on_connection(client, format_address(from.btAddr)) {
                            return;
                        }
                    }
                })?;
            Ok(Self {
                socket,
                stopped,
                record,
            })
        }

        pub fn stop(mut self) {
            self.stopped.store(true, Ordering::SeqCst);
            let _ = self.record.set(RNRSERVICE_DELETE);
            // SAFETY: closing the listening socket wakes the accept thread, which then ends.
            unsafe {
                let _ = closesocket(self.socket.0);
            }
        }
    }

    pub fn dial(address: u64) -> std::io::Result<Socket> {
        let s = new_socket()?;
        let to = SOCKADDR_BTH {
            addressFamily: AF_BTH,
            btAddr: address,
            serviceClassId: service_class(),
            // 0: look the channel up by the service class (SDP).
            port: 0,
        };
        // SAFETY: a valid SOCKADDR_BTH of the given length.
        let r = unsafe {
            connect(
                s.0,
                (&raw const to).cast::<SOCKADDR>(),
                std::mem::size_of::<SOCKADDR_BTH>() as i32,
            )
        };
        if r != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(s)
    }

    /// Join a blocking socket to an async pipe: one thread each way.
    pub fn bridge(socket: Socket, handle: &tokio::runtime::Handle) -> DuplexStream {
        let (ours, theirs) = tokio::io::duplex(256 * 1024);
        let (mut reader, mut writer) = tokio::io::split(ours);
        let socket = Arc::new(socket);
        let (rx_socket, rx_handle) = (socket.clone(), handle.clone());
        let _ = std::thread::Builder::new()
            .name("pairly-bt-rx".into())
            .spawn(move || {
                let mut buf = vec![0u8; 64 * 1024];
                loop {
                    // SAFETY: a valid socket and buffer.
                    let n = unsafe { recv(rx_socket.0, &mut buf, SEND_RECV_FLAGS(0)) };
                    if n <= 0 {
                        break;
                    }
                    let chunk = &buf[..n as usize];
                    if rx_handle.block_on(writer.write_all(chunk)).is_err() {
                        break;
                    }
                }
                let _ = rx_handle.block_on(writer.shutdown());
            });
        let tx_handle = handle.clone();
        let _ = std::thread::Builder::new()
            .name("pairly-bt-tx".into())
            .spawn(move || {
                let mut buf = vec![0u8; 64 * 1024];
                loop {
                    let n = match tx_handle.block_on(reader.read(&mut buf)) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    let mut sent = 0;
                    while sent < n {
                        // SAFETY: a valid socket and buffer.
                        let r = unsafe { send(socket.0, &buf[sent..n], SEND_RECV_FLAGS(0)) };
                        if r <= 0 {
                            return;
                        }
                        sent += r as usize;
                    }
                }
                // The core closed the stream: so does the socket (dropping the last handle).
            });
        theirs
    }
}

#[cfg(not(windows))]
mod imp {
    pub struct Socket;
    pub struct Listener;

    impl Listener {
        pub fn start(
            _on: impl Fn(Socket, String) -> bool + Send + 'static,
        ) -> std::io::Result<Self> {
            Err(std::io::Error::other("Bluetooth is built for Windows"))
        }

        pub fn stop(self) {}
    }

    pub fn local_address() -> anyhow::Result<String> {
        anyhow::bail!("Bluetooth is built for Windows")
    }

    pub fn parse_address(_s: &str) -> Option<u64> {
        None
    }

    pub fn dial(_address: u64) -> std::io::Result<Socket> {
        Err(std::io::Error::other("Bluetooth is built for Windows"))
    }

    pub fn bridge(_s: Socket, _h: &tokio::runtime::Handle) -> tokio::io::DuplexStream {
        tokio::io::duplex(1).0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_round_trip() {
        assert_eq!(format_address(0x0011_2233_44AA), "00:11:22:33:44:AA");
        #[cfg(windows)]
        assert_eq!(
            imp::parse_address("00:11:22:33:44:AA"),
            Some(0x0011_2233_44AA)
        );
    }
}
