//! Clipboard, battery and find-my between two nodes over the in-memory transport.
#![allow(clippy::unwrap_used)] // test helpers

use std::sync::{Arc, Mutex};
use std::time::Duration;

use pairly_core::memory::MemoryNetwork;
use pairly_core::{DeviceType, NodeConfig, NodeEvent, PairlyNode, PeerInfo};
use pairly_plugins::battery::{BatteryHost, BatteryPlugin, BatteryState};
use pairly_plugins::clipboard::{ClipboardHost, ClipboardPlugin};
use pairly_plugins::findmy::{FindMyHost, FindMyPlugin};
use tokio::sync::mpsc;

#[derive(Debug, PartialEq)]
enum Call {
    Clipboard(String),
    Battery(BatteryState, Option<BatteryState>),
    Ring(bool),
}

struct Host {
    calls: Mutex<mpsc::UnboundedSender<Call>>,
    battery: Option<BatteryState>,
}

impl Host {
    fn call(&self, c: Call) {
        let _ = self.calls.lock().unwrap().send(c);
    }
}

impl ClipboardHost for Host {
    fn set_clipboard(&self, _: &PeerInfo, text: &str) {
        self.call(Call::Clipboard(text.into()));
    }
}

impl BatteryHost for Host {
    fn current(&self) -> Option<BatteryState> {
        self.battery
    }
    fn peer_changed(&self, _: &PeerInfo, state: BatteryState, previous: Option<BatteryState>) {
        self.call(Call::Battery(state, previous));
    }
}

impl FindMyHost for Host {
    fn ring(&self, _: &PeerInfo, on: bool) {
        self.call(Call::Ring(on));
    }
}

struct Side {
    node: PairlyNode,
    clipboard: Arc<ClipboardPlugin>,
    battery: Arc<BatteryPlugin>,
    findmy: Arc<FindMyPlugin>,
    calls: mpsc::UnboundedReceiver<Call>,
}

impl Side {
    async fn next(&mut self) -> Call {
        tokio::time::timeout(Duration::from_secs(5), self.calls.recv())
            .await
            .expect("call in time")
            .unwrap()
    }

    async fn quiet(&mut self) {
        let r = tokio::time::timeout(Duration::from_millis(300), self.calls.recv()).await;
        assert!(r.is_err(), "unexpected call: {r:?}");
    }
}

async fn start(net: &MemoryNetwork, addr: &str, battery: Option<BatteryState>) -> Side {
    let (tx, calls) = mpsc::unbounded_channel();
    let host = Arc::new(Host {
        calls: Mutex::new(tx),
        battery,
    });
    let clipboard = ClipboardPlugin::new(host.clone());
    let battery = BatteryPlugin::new(host.clone());
    let findmy = FindMyPlugin::new(host);
    let node = PairlyNode::builder(NodeConfig::new(addr, DeviceType::Phone))
        .transport(net.transport(addr))
        .plugin(clipboard.clone())
        .plugin(battery.clone())
        .plugin(findmy.clone())
        .start()
        .await
        .unwrap();
    Side {
        node,
        clipboard,
        battery,
        findmy,
        calls,
    }
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

fn level(percent: u8, charging: bool) -> BatteryState {
    BatteryState { percent, charging }
}

#[tokio::test]
async fn clipboard_battery_and_ring() {
    let net = MemoryNetwork::new();
    let mut phone = start(&net, "phone", Some(level(80, false))).await;
    let mut pc = start(&net, "pc", None).await; // a desktop without a battery
    pair(&phone, &pc).await;
    let phone_id = phone.node.device_id();

    // Battery on connect: only the phone has one.
    assert_eq!(pc.next().await, Call::Battery(level(80, false), None));
    assert_eq!(pc.battery.peer_state(phone_id), Some(level(80, false)));
    phone.quiet().await;

    // Changes are sent once; repeats are ignored.
    phone.battery.local_changed(level(80, false));
    phone.battery.local_changed(level(15, false));
    phone.battery.local_changed(level(15, false));
    assert_eq!(
        pc.next().await,
        Call::Battery(level(15, false), Some(level(80, false)))
    );
    pc.quiet().await;

    // Clipboard PC -> phone, and the phone's clipboard watcher seeing it doesn't echo back.
    pc.clipboard.local_changed("copied on the PC");
    assert_eq!(
        phone.next().await,
        Call::Clipboard("copied on the PC".into())
    );
    phone.clipboard.local_changed("copied on the PC");
    pc.quiet().await;
    // A genuinely new copy goes through, and an explicit send always does.
    phone.clipboard.local_changed("copied on the phone");
    assert_eq!(
        pc.next().await,
        Call::Clipboard("copied on the phone".into())
    );
    phone
        .clipboard
        .send_to(pc.node.device_id(), "copied on the phone")
        .unwrap();
    assert_eq!(
        pc.next().await,
        Call::Clipboard("copied on the phone".into())
    );
    // Empty clipboards are never sent.
    pc.clipboard.local_changed("");
    phone.quiet().await;

    // Find my phone.
    pc.findmy.ring(phone_id, true).unwrap();
    assert_eq!(phone.next().await, Call::Ring(true));
    pc.findmy.ring(phone_id, false).unwrap();
    assert_eq!(phone.next().await, Call::Ring(false));
}
