//! Screen mirroring between a "phone" (shares) and a "PC" (views and controls) over the
//! in-memory network.
#![allow(clippy::unwrap_used)] // test helpers

use std::sync::{Arc, Mutex};
use std::time::Duration;

use pairly_core::memory::MemoryNetwork;
use pairly_core::{DeviceType, NodeConfig, NodeEvent, PairlyNode, PeerInfo};
use pairly_plugins::screen::{
    ScreenAction, ScreenFrame, ScreenHost, ScreenInput, ScreenKey, ScreenPlugin,
};
use tokio::sync::mpsc;

#[derive(Debug, PartialEq)]
enum Call {
    StartSharing,
    StopSharing,
    Input(ScreenInput),
    Started(u32, u32),
    Frame(usize, bool),
    Stopped(String),
}

struct Host {
    calls: Mutex<mpsc::UnboundedSender<Call>>,
    refuse: bool,
}

impl Host {
    fn call(&self, c: Call) {
        let _ = self.calls.lock().unwrap().send(c);
    }
}

impl ScreenHost for Host {
    fn start_sharing(&self, _: &PeerInfo) -> Result<(), String> {
        if self.refuse {
            return Err("the user said no".into());
        }
        self.call(Call::StartSharing);
        Ok(())
    }
    fn stop_sharing(&self, _: &PeerInfo) {
        self.call(Call::StopSharing);
    }
    fn input(&self, _: &PeerInfo, input: ScreenInput) {
        self.call(Call::Input(input));
    }
    fn started(&self, _: &PeerInfo, w: u32, h: u32) {
        self.call(Call::Started(w, h));
    }
    fn frame(&self, _: &PeerInfo, f: ScreenFrame) {
        self.call(Call::Frame(f.data.len(), f.key));
    }
    fn stopped(&self, _: &PeerInfo, reason: &str) {
        self.call(Call::Stopped(reason.to_owned()));
    }
}

struct Side {
    node: PairlyNode,
    screen: Arc<ScreenPlugin>,
    calls: mpsc::UnboundedReceiver<Call>,
}

impl Side {
    async fn next(&mut self) -> Call {
        tokio::time::timeout(Duration::from_secs(5), self.calls.recv())
            .await
            .expect("call in time")
            .unwrap()
    }
}

async fn start(net: &MemoryNetwork, addr: &str, refuse: bool) -> Side {
    let (tx, calls) = mpsc::unbounded_channel();
    let screen = ScreenPlugin::new(Arc::new(Host {
        calls: Mutex::new(tx),
        refuse,
    }));
    let node = PairlyNode::builder(NodeConfig::new(addr, DeviceType::Phone))
        .transport(net.transport(addr))
        .plugin(screen.clone())
        .start()
        .await
        .unwrap();
    Side {
        node,
        screen,
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

#[tokio::test(flavor = "multi_thread")]
async fn share_watch_control_and_stop() {
    let net = MemoryNetwork::new();
    let mut phone = start(&net, "phone", false).await;
    let mut pc = start(&net, "pc", false).await;
    pair(&phone, &pc).await;
    let (phone_id, pc_id) = (phone.node.device_id(), pc.node.device_id());

    pc.screen.request(phone_id, ScreenAction::Start).unwrap();
    assert_eq!(phone.next().await, Call::StartSharing);

    phone.screen.started(pc_id, 720, 1600).unwrap();
    assert_eq!(pc.next().await, Call::Started(720, 1600));
    let frame = |len, key, config| ScreenFrame {
        data: vec![7; len],
        key,
        config,
    };
    phone.screen.frame(pc_id, &frame(40, true, true)).unwrap();
    phone
        .screen
        .frame(pc_id, &frame(200_000, true, false))
        .unwrap();
    phone
        .screen
        .frame(pc_id, &frame(3_000, false, false))
        .unwrap();
    assert_eq!(pc.next().await, Call::Frame(40, true));
    assert_eq!(pc.next().await, Call::Frame(200_000, true));
    assert_eq!(pc.next().await, Call::Frame(3_000, false));
    assert!(
        phone
            .screen
            .frame(pc_id, &frame(2_000_000, true, false))
            .is_err(),
        "frame size cap"
    );

    // Control from the PC: positions are clamped to the screen; nonsense is refused.
    pc.screen
        .input(phone_id, ScreenInput::Tap { x: 0.5, y: 1.5 })
        .unwrap();
    assert_eq!(
        phone.next().await,
        Call::Input(ScreenInput::Tap { x: 0.5, y: 1.0 })
    );
    let swipe = ScreenInput::Swipe {
        points: vec![(0.5, 0.8), (0.5, 0.2)],
        duration_ms: 300,
    };
    pc.screen.input(phone_id, swipe.clone()).unwrap();
    assert_eq!(phone.next().await, Call::Input(swipe));
    pc.screen
        .input(
            phone_id,
            ScreenInput::Key {
                key: ScreenKey::Back,
            },
        )
        .unwrap();
    assert_eq!(
        phone.next().await,
        Call::Input(ScreenInput::Key {
            key: ScreenKey::Back
        })
    );
    let one_point = ScreenInput::Swipe {
        points: vec![(0.1, 0.1)],
        duration_ms: 100,
    };
    assert!(pc.screen.input(phone_id, one_point).is_err());

    pc.screen.request(phone_id, ScreenAction::Stop).unwrap();
    assert_eq!(phone.next().await, Call::StopSharing);
    phone.screen.stopped(pc_id, "stopped on the phone").unwrap();
    assert_eq!(
        pc.next().await,
        Call::Stopped("stopped on the phone".into())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refusal_is_reported() {
    let net = MemoryNetwork::new();
    let phone = start(&net, "phone", true).await;
    let mut pc = start(&net, "pc", false).await;
    pair(&phone, &pc).await;
    pc.screen
        .request(phone.node.device_id(), ScreenAction::Start)
        .unwrap();
    assert_eq!(pc.next().await, Call::Stopped("the user said no".into()));
}
