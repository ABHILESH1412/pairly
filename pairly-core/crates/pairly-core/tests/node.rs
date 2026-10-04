//! End-to-end node tests over the in-memory transport.
#![allow(clippy::unwrap_used)] // test helpers

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use pairly_core::channel;
use pairly_core::memory::MemoryNetwork;
use pairly_core::transport::{PeerCandidate, Transport, TransportKind};
use pairly_core::{
    DeviceId, DeviceType, Envelope, NodeConfig, NodeEvent, OutboundPacket, PacketBody, PairlyNode,
    Plugin, PluginCtx, Priority, Registry,
};
use pairly_crypto::{FileKeyStore, IdentityKeypair, Initiate};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc};

const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Serialize, Deserialize)]
struct Ping {
    n: u64,
}
impl PacketBody for Ping {
    const TYPE: &'static str = "test.ping";
}

#[derive(Serialize, Deserialize)]
struct Pong {
    n: u64,
}
impl PacketBody for Pong {
    const TYPE: &'static str = "test.pong";
}

/// Answers pings with pongs and reports received pongs.
struct Echo {
    pongs: mpsc::UnboundedSender<u64>,
}

#[async_trait]
impl Plugin for Echo {
    fn id(&self) -> &'static str {
        "echo"
    }
    fn incoming(&self) -> &'static [&'static str] {
        &[Ping::TYPE, Pong::TYPE]
    }
    fn outgoing(&self) -> &'static [&'static str] {
        &[Ping::TYPE, Pong::TYPE]
    }
    async fn on_packet(&self, ctx: &PluginCtx, packet: Envelope) {
        if let Ok(Ping { n }) = packet.body() {
            ctx.send(OutboundPacket::reliable(&Pong { n }, Priority::Interactive).unwrap())
                .unwrap();
        } else if let Ok(Pong { n }) = packet.body() {
            let _ = self.pongs.send(n);
        }
    }
}

struct TestNode {
    node: PairlyNode,
    events: broadcast::Receiver<NodeEvent>,
    pongs: mpsc::UnboundedReceiver<u64>,
}

impl TestNode {
    fn id(&self) -> DeviceId {
        self.node.device_id()
    }

    async fn event<T>(&mut self, mut f: impl FnMut(&NodeEvent) -> Option<T>) -> T {
        let wait = async {
            loop {
                match self.events.recv().await {
                    Ok(e) => {
                        if let Some(v) = f(&e) {
                            return v;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(e) => panic!("event stream closed: {e}"),
                }
            }
        };
        tokio::time::timeout(TIMEOUT, wait)
            .await
            .expect("timed out waiting for event")
    }

    async fn connected_to(&mut self, peer: DeviceId) {
        self.event(|e| matches!(e, NodeEvent::Connected { id, .. } if *id == peer).then_some(()))
            .await;
    }

    async fn ping(&mut self, peer: DeviceId, n: u64) {
        self.node
            .send(
                peer,
                OutboundPacket::reliable(&Ping { n }, Priority::Interactive).unwrap(),
            )
            .unwrap();
        let got = tokio::time::timeout(TIMEOUT, self.pongs.recv())
            .await
            .expect("pong in time");
        assert_eq!(got, Some(n));
    }

    async fn sees(&self, peer: DeviceId) {
        let wait = async {
            while !self.node.devices().unwrap().iter().any(|d| d.id == peer) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        tokio::time::timeout(TIMEOUT, wait)
            .await
            .expect("device discovered");
    }

    fn paired_with(&self, peer: DeviceId) -> bool {
        self.node
            .devices()
            .unwrap()
            .iter()
            .any(|d| d.id == peer && d.paired)
    }
}

async fn start(net: &MemoryNetwork, address: &str, name: &str, storage: Option<&Path>) -> TestNode {
    let (tx, pongs) = mpsc::unbounded_channel();
    let mut builder = PairlyNode::builder(NodeConfig::new(name, DeviceType::Desktop))
        .transport(net.transport(address))
        .plugin(Arc::new(Echo { pongs: tx }));
    if let Some(dir) = storage {
        builder = builder
            .keystore(Arc::new(FileKeyStore::new(dir.join("identity.key"))))
            .registry(Registry::open(&dir.join("registry.db")).unwrap());
    }
    let node = builder.start().await.unwrap();
    let events = node.subscribe();
    TestNode {
        node,
        events,
        pongs,
    }
}

/// Pair `a` with `b`, with both users accepting matching codes.
async fn pair(a: &mut TestNode, b: &mut TestNode) {
    let (a_id, b_id) = (a.id(), b.id());
    a.sees(b_id).await;
    a.node.request_pair(b_id).await.unwrap();
    let code_a = a
        .event(|e| match e {
            NodeEvent::PairingRequested {
                id,
                code,
                incoming: false,
                ..
            } if *id == b_id => Some(*code),
            _ => None,
        })
        .await;
    let (code_b, name) = b
        .event(|e| match e {
            NodeEvent::PairingRequested {
                id,
                code,
                name,
                incoming: true,
            } if *id == a_id => Some((*code, name.clone())),
            _ => None,
        })
        .await;
    assert_eq!(code_a, code_b, "both screens show the same code");
    assert_eq!(name, a.node.name());
    a.node.confirm_pair(b_id, true).unwrap();
    b.node.confirm_pair(a_id, true).unwrap();
    a.connected_to(b_id).await;
    b.connected_to(a_id).await;
}

#[tokio::test]
async fn pair_then_ping_both_ways() {
    let net = MemoryNetwork::new();
    let mut a = start(&net, "a", "Laptop", None).await;
    let mut b = start(&net, "b", "Phone", None).await;
    pair(&mut a, &mut b).await;

    a.ping(b.id(), 1).await;
    b.ping(a.id(), 2).await;

    for (node, peer) in [(&a, b.id()), (&b, a.id())] {
        let dev = node
            .node
            .devices()
            .unwrap()
            .into_iter()
            .find(|d| d.id == peer)
            .unwrap();
        assert!(dev.paired);
        assert_eq!(dev.link, Some(TransportKind::Memory));
    }
    a.node.shutdown().await;
    b.node.shutdown().await;
}

#[tokio::test]
async fn rejected_pairing_stores_nothing() {
    let net = MemoryNetwork::new();
    let mut a = start(&net, "a", "Laptop", None).await;
    let mut b = start(&net, "b", "Phone", None).await;
    let (a_id, b_id) = (a.id(), b.id());
    a.sees(b_id).await;
    a.node.request_pair(b_id).await.unwrap();
    b.event(|e| matches!(e, NodeEvent::PairingRequested { .. }).then_some(()))
        .await;
    b.node.confirm_pair(a_id, false).unwrap();

    a.event(|e| matches!(e, NodeEvent::PairingFailed { id, .. } if *id == b_id).then_some(()))
        .await;
    b.event(|e| matches!(e, NodeEvent::PairingFailed { id, .. } if *id == a_id).then_some(()))
        .await;
    assert!(!a.paired_with(b_id) && !b.paired_with(a_id));
    // A declined request starts a cooldown: an immediate retry is refused without a prompt.
    assert!(a.node.request_pair(b_id).await.is_err());
}

#[tokio::test]
async fn reconnects_after_restart() {
    let net = MemoryNetwork::new();
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut a = start(&net, "a", "Laptop", Some(dir_a.path())).await;
    let mut b = start(&net, "b", "Phone", Some(dir_b.path())).await;
    pair(&mut a, &mut b).await;
    a.ping(b.id(), 1).await;

    let b_id = b.id();
    b.node.shutdown().await;
    a.event(|e| matches!(e, NodeEvent::Disconnected { id, .. } if *id == b_id).then_some(()))
        .await;

    // Same identity and registry on disk: they find each other and reconnect via IK.
    let mut b = start(&net, "b", "Phone", Some(dir_b.path())).await;
    assert_eq!(b.id(), b_id);
    a.connected_to(b_id).await;
    a.ping(b_id, 2).await;
    b.ping(a.id(), 3).await;
}

#[tokio::test]
async fn stranger_cannot_reconnect() {
    let net = MemoryNetwork::new();
    let mut a = start(&net, "a", "Laptop", None).await;
    let mut b = start(&net, "b", "Phone", None).await;
    pair(&mut a, &mut b).await;

    // A stranger who knows B's public key still can't open a session: B only accepts IK
    // from pinned keys.
    let stranger = IdentityKeypair::generate();
    let target = PeerCandidate {
        transport: TransportKind::Memory,
        address: "b".into(),
        device_id: Some(b.id()),
        name: None,
    };
    let stream = net.transport("stranger").connect(&target).await.unwrap();
    let b_key = b.node.public_key();
    let res = channel::connect(
        stream,
        TransportKind::Memory,
        Initiate::Reconnect(&b_key),
        &stranger,
    )
    .await;
    assert!(res.is_err());
}

#[tokio::test]
async fn unpaired_device_is_refused() {
    let net = MemoryNetwork::new();
    let mut a = start(&net, "a", "Laptop", None).await;
    let mut b = start(&net, "b", "Phone", None).await;
    pair(&mut a, &mut b).await;
    let (a_id, b_id) = (a.id(), b.id());

    a.node.unpair(b_id).unwrap();
    a.event(|e| matches!(e, NodeEvent::Unpaired { id } if *id == b_id).then_some(()))
        .await;
    // B still thinks it is paired and keeps retrying, but A refuses.
    b.event(|e| matches!(e, NodeEvent::Disconnected { id, .. } if *id == a_id).then_some(()))
        .await;
    let reconnected = tokio::time::timeout(Duration::from_secs(2), async {
        a.connected_to(b_id).await;
    })
    .await;
    assert!(reconnected.is_err(), "A must not accept B after unpairing");
    assert!(!a.paired_with(b_id));
    assert!(
        a.node
            .send(
                b_id,
                OutboundPacket::reliable(&Ping { n: 1 }, Priority::Interactive).unwrap()
            )
            .is_err()
    );
}

#[tokio::test]
async fn qr_pairing_skips_the_code_and_is_one_time() {
    let net = MemoryNetwork::new();
    let mut phone = start(&net, "phone", "Phone", None).await;
    let mut pc = start(&net, "pc", "PC", None).await;
    let uri = pc.node.start_qr_pairing().unwrap().to_uri();

    phone.node.pair_from_qr(&uri).await.unwrap();
    // Straight to connected: no code dialog on either side.
    let (phone_id, pc_id) = (phone.id(), pc.id());
    for (node, peer) in [(&mut phone, pc_id), (&mut pc, phone_id)] {
        node.event(|e| match e {
            NodeEvent::PairingRequested { .. } => panic!("QR pairing must not ask for a code"),
            NodeEvent::Connected { id, .. } if *id == peer => Some(()),
            _ => None,
        })
        .await;
    }
    phone.ping(pc.id(), 1).await;

    // The code is spent: another device can't use it.
    let intruder = start(&net, "intruder", "Intruder", None).await;
    assert!(intruder.node.pair_from_qr(&uri).await.is_err());
    assert!(!pc.paired_with(intruder.id()));
}

#[tokio::test]
async fn qr_code_can_be_cancelled_or_expire() {
    let net = MemoryNetwork::new();
    let phone = start(&net, "phone", "Phone", None).await;
    let pc = start(&net, "pc", "PC", None).await;
    let uri = pc.node.start_qr_pairing().unwrap().to_uri();
    pc.node.cancel_qr_pairing();
    assert!(phone.node.pair_from_qr(&uri).await.is_err());

    let mut config = NodeConfig::new("Short", DeviceType::Desktop);
    config.qr_timeout = Duration::from_millis(100);
    let short = PairlyNode::builder(config)
        .transport(net.transport("short"))
        .start()
        .await
        .unwrap();
    let uri = short.start_qr_pairing().unwrap().to_uri();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(phone.node.pair_from_qr(&uri).await.is_err());
    assert!(!phone.paired_with(short.device_id()));
}

#[tokio::test]
async fn tampered_qr_codes_fail() {
    let net = MemoryNetwork::new();
    let phone = start(&net, "phone", "Phone", None).await;
    let pc = start(&net, "pc", "PC", None).await;

    // Wrong secret: the PSK handshake fails.
    let mut invite = pc.node.start_qr_pairing().unwrap();
    invite.secret[0] ^= 1;
    assert!(phone.node.pair_from_qr(&invite.to_uri()).await.is_err());

    // Right secret but a different key: whoever answers isn't the device in the code.
    let mut invite = pc.node.start_qr_pairing().unwrap();
    invite.public_key = IdentityKeypair::generate().public();
    assert!(phone.node.pair_from_qr(&invite.to_uri()).await.is_err());
    assert!(!phone.paired_with(pc.id()) && !pc.paired_with(phone.id()));
}
