//! Two nodes on the real network stack: mDNS discovery, QUIC, pairing and a ping.
//! Needs working multicast on the host (it fails in sandboxes without a network).
#![allow(clippy::unwrap_used)] // test helpers

use std::sync::{Arc, Mutex};
use std::time::Duration;

use pairly_core::{DeviceId, DeviceType, NodeConfig, NodeEvent, PairlyNode, PeerInfo, Platform};
use pairly_transport_lan::{LanConfig, LanTransport};
use tokio::sync::{broadcast, mpsc};

const TIMEOUT: Duration = Duration::from_secs(15);

struct Recorder(Mutex<mpsc::UnboundedSender<(DeviceId, Option<String>)>>);

impl Platform for Recorder {
    fn ping_received(&self, from: &PeerInfo, message: Option<&str>) {
        let _ = self
            .0
            .lock()
            .unwrap()
            .send((from.id, message.map(str::to_owned)));
    }
}

async fn start(
    name: &str,
) -> (
    PairlyNode,
    broadcast::Receiver<NodeEvent>,
    mpsc::UnboundedReceiver<(DeviceId, Option<String>)>,
) {
    let (tx, rx) = mpsc::unbounded_channel();
    let builder = PairlyNode::builder(NodeConfig::new(name, DeviceType::Desktop))
        .transport(LanTransport::new(LanConfig {
            port: 0,
            mdns: true,
        }))
        .platform(Arc::new(Recorder(Mutex::new(tx))))
        .plugin(Arc::new(pairly_plugins::ping::PingPlugin));
    let node = builder.start().await.unwrap();
    let events = node.subscribe();
    (node, events, rx)
}

async fn wait<T>(
    rx: &mut broadcast::Receiver<NodeEvent>,
    mut f: impl FnMut(&NodeEvent) -> Option<T>,
) -> T {
    tokio::time::timeout(TIMEOUT, async {
        loop {
            if let Ok(e) = rx.recv().await
                && let Some(v) = f(&e)
            {
                return v;
            }
        }
    })
    .await
    .expect("event in time")
}

#[tokio::test(flavor = "multi_thread")]
async fn pair_and_ping_over_lan() {
    let (a, mut ae, mut a_pings) = start("lan-a").await;
    let (b, mut be, mut b_pings) = start("lan-b").await;
    let (a_id, b_id) = (a.device_id(), b.device_id());

    // mDNS discovery.
    tokio::time::timeout(TIMEOUT, async {
        while !a.devices().unwrap().iter().any(|d| d.id == b_id) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("a discovers b over mDNS");

    a.request_pair(b_id).await.unwrap();
    let ca = wait(&mut ae, |e| match e {
        NodeEvent::PairingRequested { code, .. } => Some(*code),
        _ => None,
    })
    .await;
    let cb = wait(&mut be, |e| match e {
        NodeEvent::PairingRequested { code, .. } => Some(*code),
        _ => None,
    })
    .await;
    assert_eq!(ca, cb);
    a.confirm_pair(b_id, true).unwrap();
    b.confirm_pair(a_id, true).unwrap();
    let link = wait(&mut ae, |e| match e {
        NodeEvent::Connected { transport, .. } => Some(*transport),
        _ => None,
    })
    .await;
    assert_eq!(link, pairly_core::TransportKind::Lan);

    a.send(
        b_id,
        pairly_plugins::ping::packet(Some("hello over QUIC".into())).unwrap(),
    )
    .unwrap();
    let (from, msg) = tokio::time::timeout(TIMEOUT, b_pings.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!((from, msg.as_deref()), (a_id, Some("hello over QUIC")));

    b.send(a_id, pairly_plugins::ping::packet(None).unwrap())
        .unwrap();
    let (from, msg) = tokio::time::timeout(TIMEOUT, a_pings.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!((from, msg), (b_id, None));

    // A network refresh (re-announce + re-browse) must not disturb the live connection.
    a.network_changed().await;
    b.network_changed().await;
    a.send(
        b_id,
        pairly_plugins::ping::packet(Some("after refresh".into())).unwrap(),
    )
    .unwrap();
    let (_, msg) = tokio::time::timeout(TIMEOUT, b_pings.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(msg.as_deref(), Some("after refresh"));
    assert!(
        a.devices()
            .unwrap()
            .iter()
            .any(|d| d.id == b_id && d.link.is_some())
    );

    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn qr_pairing_over_lan() {
    let (phone, _pe, _pp) = start("lan-phone").await;
    let (pc, mut pce, _pcp) = start("lan-pc").await;
    let invite = pc.start_qr_pairing().unwrap();
    assert!(
        !invite.addresses.is_empty(),
        "the QR code carries reachable addresses"
    );
    // Use only the addresses in the code, as a phone that hasn't seen mDNS yet would.
    phone.pair_from_qr(&invite.to_uri()).await.unwrap();
    let id = wait(&mut pce, |e| match e {
        NodeEvent::Connected { id, .. } => Some(*id),
        _ => None,
    })
    .await;
    assert_eq!(id, phone.device_id());
    phone.shutdown().await;
    pc.shutdown().await;
}
