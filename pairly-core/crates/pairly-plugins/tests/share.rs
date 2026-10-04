//! File and text sharing between two nodes over the in-memory transport, including a transfer
//! that survives its connection being cut.
#![allow(clippy::unwrap_used)] // test helpers

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use pairly_core::memory::MemoryNetwork;
use pairly_core::{DeviceType, NodeConfig, NodeEvent, PairlyNode, PeerInfo};
use pairly_plugins::share::{ShareHost, SharePlugin, Transfer, TransferState};
use tokio::sync::mpsc;

const TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug)]
enum Call {
    Offered(Transfer),
    Changed(Transfer),
    Text(String, bool),
}

struct Host {
    calls: Mutex<mpsc::UnboundedSender<Call>>,
    /// Accept offers straight away, into this directory.
    auto_accept: Option<PathBuf>,
    plugin: OnceLock<Weak<SharePlugin>>,
}

impl Host {
    fn call(&self, c: Call) {
        let _ = self.calls.lock().unwrap().send(c);
    }
}

impl ShareHost for Host {
    fn file_offered(&self, t: &Transfer) {
        if let Some(dir) = &self.auto_accept {
            let plugin = self.plugin.get().unwrap().upgrade().unwrap();
            plugin
                .accept(t.id, File::create(dir.join(&t.name)).unwrap())
                .unwrap();
        }
        self.call(Call::Offered(t.clone()));
    }
    fn transfer_changed(&self, t: &Transfer) {
        self.call(Call::Changed(t.clone()));
    }
    fn text_received(&self, _: &PeerInfo, text: &str, url: bool) {
        self.call(Call::Text(text.into(), url));
    }
}

struct Side {
    node: PairlyNode,
    share: Arc<SharePlugin>,
    calls: mpsc::UnboundedReceiver<Call>,
}

impl Side {
    async fn next(&mut self) -> Call {
        tokio::time::timeout(TIMEOUT, self.calls.recv())
            .await
            .expect("call in time")
            .unwrap()
    }

    /// Skip progress reports until the transfer `id` reaches a final state.
    async fn finished(&mut self, id: u64) -> Transfer {
        loop {
            if let Call::Changed(t) = self.next().await
                && t.id == id
                && t.state.is_finished()
            {
                return t;
            }
        }
    }

    async fn offered(&mut self) -> Transfer {
        loop {
            if let Call::Offered(t) = self.next().await {
                return t;
            }
        }
    }
}

async fn start(net: &MemoryNetwork, addr: &str, auto_accept: Option<&Path>) -> Side {
    let (tx, calls) = mpsc::unbounded_channel();
    let host = Arc::new(Host {
        calls: Mutex::new(tx),
        auto_accept: auto_accept.map(Path::to_owned),
        plugin: OnceLock::new(),
    });
    let share = SharePlugin::new(host.clone());
    host.plugin.set(Arc::downgrade(&share)).unwrap();
    let node = PairlyNode::builder(NodeConfig::new(addr, DeviceType::Phone))
        .transport(net.transport(addr))
        .plugin(share.clone())
        .start()
        .await
        .unwrap();
    Side { node, share, calls }
}

async fn pair(a: &Side, b: &Side) {
    let (mut ae, mut be) = (a.node.subscribe(), b.node.subscribe());
    let (a_id, b_id) = (a.node.device_id(), b.node.device_id());
    while !a.node.devices().unwrap().iter().any(|d| d.id == b_id) {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    a.node.request_pair(b_id).await.unwrap();
    for (rx, node, peer) in [(&mut ae, &a.node, b_id), (&mut be, &b.node, a_id)] {
        loop {
            if let NodeEvent::PairingRequested { .. } = rx.recv().await.unwrap() {
                node.confirm_pair(peer, true).unwrap();
                break;
            }
        }
    }
    for rx in [&mut ae, &mut be] {
        while !matches!(rx.recv().await.unwrap(), NodeEvent::Connected { .. }) {}
    }
}

/// Deterministic, non-repeating test content.
fn write_test_file(path: &Path, len: usize) -> Vec<u8> {
    let mut data = Vec::with_capacity(len);
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    while data.len() < len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        data.extend_from_slice(&x.to_le_bytes());
    }
    data.truncate(len);
    std::fs::write(path, &data).unwrap();
    data
}

#[tokio::test(flavor = "multi_thread")]
async fn text_and_links() {
    let net = MemoryNetwork::new();
    let mut phone = start(&net, "phone", None).await;
    let pc = start(&net, "pc", None).await;
    pair(&phone, &pc).await;
    let phone_id = phone.node.device_id();

    pc.share
        .send_text(phone_id, "https://example.com/x", true)
        .unwrap();
    pc.share
        .send_text(phone_id, "just some text", false)
        .unwrap();
    // A link with a scheme that must not be opened arrives as plain text.
    pc.share
        .send_text(phone_id, "file:///etc/passwd", true)
        .unwrap();
    for (text, url) in [
        ("https://example.com/x", true),
        ("just some text", false),
        ("file:///etc/passwd", false),
    ] {
        match phone.next().await {
            Call::Text(t, u) => assert_eq!((t.as_str(), u), (text, url)),
            other => panic!("expected text, got {other:?}"),
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn file_transfer_survives_a_cut_connection() {
    let net = MemoryNetwork::new();
    let (src, dst) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut phone = start(&net, "phone", Some(dst.path())).await;
    let mut pc = start(&net, "pc", None).await;
    pair(&phone, &pc).await;

    let path = src.path().join("video.mp4");
    let data = write_test_file(&path, 24 * 1024 * 1024 + 123);
    let id = pc
        .share
        .send_file(
            phone.node.device_id(),
            File::open(&path).unwrap(),
            "../video.mp4",
            Some("video/mp4"),
        )
        .unwrap();

    let offer = phone.offered().await;
    assert_eq!(offer.name, "video.mp4", "path parts are stripped");
    assert_eq!(offer.size, data.len() as u64);
    assert_eq!(offer.mime.as_deref(), Some("video/mp4"));

    // Cut the connection once some of the file has arrived.
    loop {
        if let Call::Changed(t) = phone.next().await
            && t.bytes > 2 * 1024 * 1024
        {
            assert!(t.bytes < data.len() as u64, "cut before the end");
            break;
        }
    }
    net.sever();

    let sent = pc.finished(id).await;
    assert_eq!(sent.state, TransferState::Done);
    assert_eq!(sent.bytes, data.len() as u64);
    let received = phone.finished(offer.id).await;
    assert_eq!(received.state, TransferState::Done);
    assert!(
        std::fs::read(dst.path().join("video.mp4")).unwrap() == data,
        "received bytes match"
    );
    assert!(pc.share.transfers().is_empty() && phone.share.transfers().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn decline_and_cancel() {
    let net = MemoryNetwork::new();
    let (src, dst) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut phone = start(&net, "phone", None).await;
    let mut pc = start(&net, "pc", None).await;
    pair(&phone, &pc).await;
    let phone_id = phone.node.device_id();
    let path = src.path().join("a.bin");
    write_test_file(&path, 8 * 1024 * 1024);

    // Declined offer.
    let id = pc
        .share
        .send_file(phone_id, File::open(&path).unwrap(), "a.bin", None)
        .unwrap();
    let offer = phone.offered().await;
    phone.share.cancel(offer.id).unwrap();
    assert_eq!(
        phone.finished(offer.id).await.state,
        TransferState::Cancelled
    );
    assert_eq!(pc.finished(id).await.state, TransferState::Cancelled);

    // Accepted, then cancelled by the sender while running.
    let id = pc
        .share
        .send_file(phone_id, File::open(&path).unwrap(), "a.bin", None)
        .unwrap();
    let offer = phone.offered().await;
    phone
        .share
        .accept(offer.id, File::create(dst.path().join("a.bin")).unwrap())
        .unwrap();
    pc.share.cancel(id).unwrap();
    assert_eq!(pc.finished(id).await.state, TransferState::Cancelled);
    assert_eq!(
        phone.finished(offer.id).await.state,
        TransferState::Cancelled
    );
    assert!(pc.share.transfers().is_empty() && phone.share.transfers().is_empty());
}
