//! Notification sync between two nodes over the in-memory transport.
#![allow(clippy::unwrap_used)] // test helpers

use std::sync::{Arc, Mutex};
use std::time::Duration;

use pairly_core::memory::MemoryNetwork;
use pairly_core::{DeviceType, NodeConfig, NodeEvent, PairlyNode, PeerInfo};
use pairly_plugins::notification::{Notification, NotificationHost, NotificationPlugin};
use tokio::sync::mpsc;

#[derive(Debug, PartialEq)]
enum Call {
    Show(String, String),
    Remove(String),
    Sync(Vec<String>),
    DismissLocal(String),
    ActionLocal(String, String),
    ReplyLocal(String, String),
}

struct Recorder(Mutex<mpsc::UnboundedSender<Call>>);

impl Recorder {
    fn call(&self, c: Call) {
        let _ = self.0.lock().unwrap().send(c);
    }
}

impl NotificationHost for Recorder {
    fn show(&self, _: &PeerInfo, n: &Notification) {
        self.call(Call::Show(n.id.clone(), n.text.clone()));
    }
    fn remove(&self, _: &PeerInfo, id: &str) {
        self.call(Call::Remove(id.into()));
    }
    fn sync(&self, _: &PeerInfo, active: &[String]) {
        self.call(Call::Sync(active.to_vec()));
    }
    fn dismiss_local(&self, id: &str) {
        self.call(Call::DismissLocal(id.into()));
    }
    fn action_local(&self, id: &str, action: &str) {
        self.call(Call::ActionLocal(id.into(), action.into()));
    }
    fn reply_local(&self, id: &str, text: &str) {
        self.call(Call::ReplyLocal(id.into(), text.into()));
    }
}

struct Side {
    node: PairlyNode,
    plugin: Arc<NotificationPlugin>,
    calls: mpsc::UnboundedReceiver<Call>,
}

impl Side {
    async fn next(&mut self) -> Call {
        tokio::time::timeout(Duration::from_secs(5), self.calls.recv())
            .await
            .unwrap()
            .unwrap()
    }
}

async fn start(net: &MemoryNetwork, addr: &str) -> Side {
    let (tx, calls) = mpsc::unbounded_channel();
    let plugin = NotificationPlugin::new(Arc::new(Recorder(Mutex::new(tx))));
    let node = PairlyNode::builder(NodeConfig::new(addr, DeviceType::Phone))
        .transport(net.transport(addr))
        .plugin(plugin.clone())
        .start()
        .await
        .unwrap();
    Side {
        node,
        plugin,
        calls,
    }
}

fn note(id: &str, text: &str) -> Notification {
    Notification {
        id: id.into(),
        app: "Chat".into(),
        title: "Ana".into(),
        text: text.into(),
        time: 0,
        actions: vec![],
        can_reply: true,
        icon: None,
        silent: false,
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

#[tokio::test]
async fn notifications_flow_both_ways() {
    let net = MemoryNetwork::new();
    let mut phone = start(&net, "phone").await;
    let mut pc = start(&net, "pc").await;

    // Already showing before the devices connect: sent on connect, then the full active list.
    phone.plugin.posted(note("old", "missed this"));
    pair(&phone, &pc).await;
    assert_eq!(
        pc.next().await,
        Call::Show("old".into(), "missed this".into())
    );
    assert_eq!(pc.next().await, Call::Sync(vec!["old".into()]));
    // The PC had nothing showing.
    assert_eq!(phone.next().await, Call::Sync(vec![]));

    // Live: posted, updated, replied to, acted on, dismissed from the mirror, removed at source.
    phone.plugin.posted(note("m1", "hello"));
    assert_eq!(pc.next().await, Call::Show("m1".into(), "hello".into()));
    phone.plugin.posted(note("m1", "hello again"));
    assert_eq!(
        pc.next().await,
        Call::Show("m1".into(), "hello again".into())
    );

    let phone_id = phone.node.device_id();
    pc.plugin
        .request_reply(phone_id, "m1", "on my way")
        .unwrap();
    assert_eq!(
        phone.next().await,
        Call::ReplyLocal("m1".into(), "on my way".into())
    );
    pc.plugin.request_action(phone_id, "m1", "0").unwrap();
    assert_eq!(
        phone.next().await,
        Call::ActionLocal("m1".into(), "0".into())
    );
    pc.plugin.request_dismiss(phone_id, "m1").unwrap();
    assert_eq!(phone.next().await, Call::DismissLocal("m1".into()));

    phone.plugin.removed("m1");
    assert_eq!(pc.next().await, Call::Remove("m1".into()));
    // Removing something we never posted sends nothing.
    phone.plugin.removed("never-posted");

    // And the other direction.
    pc.plugin.posted(note("pc-1", "build finished"));
    assert_eq!(
        phone.next().await,
        Call::Show("pc-1".into(), "build finished".into())
    );
}
