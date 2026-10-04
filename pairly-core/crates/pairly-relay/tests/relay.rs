//! A real relay in-process, two nodes that pair on a (simulated) LAN, then lose it, meet on the
//! relay, and move back to the LAN when it returns: nothing lost or delivered twice.
#![allow(clippy::unwrap_used)] // test helpers

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pairly_core::memory::MemoryNetwork;
use pairly_core::{
    DeviceId, DeviceType, NodeConfig, NodeEvent, PairlyNode, PeerInfo, Platform, TransportKind,
};
use pairly_relay::{Config, Relay};
use pairly_transport_relay::proto::{self, Join, RelayAddr, Role};
use pairly_transport_relay::{RelayConfig, RelayTransport, tls};
use tokio::sync::{broadcast, mpsc};

const TIMEOUT: Duration = Duration::from_secs(20);
const TOKEN: &str = "test-token";

struct Pings(Mutex<mpsc::UnboundedSender<String>>);

impl Platform for Pings {
    fn ping_received(&self, _: &PeerInfo, message: Option<&str>) {
        let _ = self
            .0
            .lock()
            .unwrap()
            .send(message.unwrap_or_default().to_owned());
    }
}

struct Side {
    node: PairlyNode,
    events: broadcast::Receiver<NodeEvent>,
    pings: mpsc::UnboundedReceiver<String>,
}

async fn start_relay() -> (Relay, String) {
    let (cert, key) = tls::generate_cert().unwrap();
    let mut config = Config::new("127.0.0.1:0".parse().unwrap());
    config.tokens = vec![TOKEN.into()];
    let relay = Relay::start(config, cert, key).unwrap();
    let addr = RelayAddr {
        host: "127.0.0.1".into(),
        port: relay.local_addr().unwrap().port(),
        pin: relay.pin(),
        token: Some(TOKEN.into()),
    };
    (relay, addr.to_string())
}

async fn start(net: &MemoryNetwork, name: &str, relay: Option<String>, phone: bool) -> Side {
    let (tx, pings) = mpsc::unbounded_channel();
    let mut config = NodeConfig::new(
        name,
        if phone {
            DeviceType::Phone
        } else {
            DeviceType::Laptop
        },
    );
    config.relay = relay;
    config.reconnect_max_backoff = Duration::from_secs(2);
    let node = PairlyNode::builder(config)
        .transport(net.transport(name))
        .transport(RelayTransport::new(RelayConfig {
            only_when_needed: phone,
        }))
        .platform(Arc::new(Pings(Mutex::new(tx))))
        .plugin(Arc::new(pairly_plugins::ping::PingPlugin))
        .start()
        .await
        .unwrap();
    let events = node.subscribe();
    Side {
        node,
        events,
        pings,
    }
}

impl Side {
    async fn connected(&mut self, peer: DeviceId) -> TransportKind {
        tokio::time::timeout(TIMEOUT, async {
            loop {
                if let Ok(NodeEvent::Connected { id, transport }) = self.events.recv().await
                    && id == peer
                {
                    return transport;
                }
            }
        })
        .await
        .expect("connected in time")
    }

    fn link(&self, peer: DeviceId) -> Option<TransportKind> {
        self.node
            .devices()
            .unwrap()
            .into_iter()
            .find(|d| d.id == peer)
            .and_then(|d| d.link)
    }

    async fn wait_link(&self, peer: DeviceId, want: TransportKind) {
        tokio::time::timeout(TIMEOUT, async {
            while self.link(peer) != Some(want) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("link {want:?} in time, have {:?}", self.link(peer)));
    }

    fn ping(&self, peer: DeviceId, msg: &str) {
        self.node
            .send(
                peer,
                pairly_plugins::ping::packet(Some(msg.into())).unwrap(),
            )
            .unwrap();
    }

    async fn expect_pings(&mut self, want: &[String]) {
        let mut got = Vec::new();
        while got.len() < want.len() {
            let p = tokio::time::timeout(TIMEOUT, self.pings.recv())
                .await
                .expect("ping in time")
                .unwrap();
            got.push(p);
        }
        got.sort();
        let mut want = want.to_vec();
        want.sort();
        assert_eq!(got, want);
        // ...and nothing twice.
        let extra = tokio::time::timeout(Duration::from_millis(500), self.pings.recv()).await;
        assert!(extra.is_err(), "duplicate delivery: {extra:?}");
    }
}

async fn pair(a: &mut Side, b: &mut Side) {
    let b_id = b.node.device_id();
    let a_id = a.node.device_id();
    while !a.node.devices().unwrap().iter().any(|d| d.id == b_id) {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    a.node.request_pair(b_id).await.unwrap();
    for (side, peer) in [(&mut *a, b_id), (&mut *b, a_id)] {
        loop {
            if let NodeEvent::PairingRequested { .. } = side.events.recv().await.unwrap() {
                side.node.confirm_pair(peer, true).unwrap();
                break;
            }
        }
    }
    assert_eq!(a.connected(b_id).await, TransportKind::Memory);
}

/// `TEST_LOG=debug cargo test -p pairly-relay` shows what the nodes do.
fn init_logging() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_env("TEST_LOG"))
        .with_test_writer()
        .try_init();
}

#[tokio::test(flavor = "multi_thread")]
async fn fails_over_to_the_relay_and_back() {
    init_logging();
    let (relay, relay_addr) = start_relay().await;
    let net = MemoryNetwork::new();
    // Only the PC is configured with the relay; the phone learns it while paired on the LAN.
    let mut pc = start(&net, "pc", Some(relay_addr), false).await;
    let mut phone = start(&net, "phone", None, true).await;
    pair(&mut pc, &mut phone).await;
    let (pc_id, phone_id) = (pc.node.device_id(), phone.node.device_id());

    // Leave home: the LAN disappears and both meet on the relay.
    net.set_reachable("phone", false).await;
    pc.wait_link(phone_id, TransportKind::Relay).await;
    phone.wait_link(pc_id, TransportKind::Relay).await;
    let over_relay: Vec<String> = (0..5).map(|i| format!("relay {i}")).collect();
    for m in &over_relay {
        pc.ping(phone_id, m);
    }
    phone.expect_pings(&over_relay).await;
    phone.ping(pc_id, "hello from mobile data");
    pc.expect_pings(&["hello from mobile data".into()]).await;
    assert!(
        relay
            .stats()
            .bytes
            .load(std::sync::atomic::Ordering::Relaxed)
            > 0
    );

    // Back home: the LAN reappears and the session moves to it while messages are in flight.
    let switching: Vec<String> = (0..20).map(|i| format!("switch {i}")).collect();
    net.set_reachable("phone", true).await;
    for m in &switching {
        pc.ping(phone_id, m);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    pc.wait_link(phone_id, TransportKind::Memory).await;
    phone.wait_link(pc_id, TransportKind::Memory).await;
    phone.expect_pings(&switching).await;

    pc.node.shutdown().await;
    phone.node.shutdown().await;
    relay.shutdown().await;
}

/// Raw protocol: a stream for a room nobody is in, with and without the right token.
async fn raw_join(relay: &Relay, join: &Join) -> u8 {
    let mut endpoint =
        quinn::Endpoint::client("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
    endpoint.set_default_client_config(tls::client_config(relay.pin()).unwrap());
    let conn = endpoint
        .connect(relay.local_addr().unwrap(), proto::SERVER_NAME)
        .unwrap()
        .await
        .unwrap();
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    send.write_all(&join.encode()).await.unwrap();
    let mut reply = [0u8; 1];
    recv.read_exact(&mut reply).await.unwrap();
    reply[0]
}

#[tokio::test(flavor = "multi_thread")]
async fn tokens_pins_and_empty_rooms() {
    let (relay, _) = start_relay().await;
    let mut join = Join {
        role: Role::Dial,
        room: [9; proto::ROOM_LEN],
        token: Some("wrong".into()),
    };
    assert_eq!(raw_join(&relay, &join).await, proto::DENIED);
    join.token = Some(TOKEN.into());
    assert_eq!(raw_join(&relay, &join).await, proto::NO_PEER);
    join.role = Role::Listen;
    assert_eq!(raw_join(&relay, &join).await, proto::ABSENT);

    // A client with the wrong pin refuses the relay's certificate.
    let mut endpoint =
        quinn::Endpoint::client("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
    endpoint.set_default_client_config(tls::client_config([0; 32]).unwrap());
    let result = endpoint
        .connect(relay.local_addr().unwrap(), proto::SERVER_NAME)
        .unwrap()
        .await;
    assert!(result.is_err(), "connected despite a wrong pin");
    relay.shutdown().await;
}
