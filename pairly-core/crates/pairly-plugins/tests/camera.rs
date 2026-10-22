//! The phone's camera as the PC's webcam, over the in-memory network.
#![allow(clippy::unwrap_used)] // test helpers

use std::sync::{Arc, Mutex};
use std::time::Duration;

use pairly_core::memory::MemoryNetwork;
use pairly_core::{DeviceType, NodeConfig, NodeEvent, PairlyNode, PeerInfo};
use pairly_plugins::camera::{CameraHost, CameraPlugin, CameraRequest, Facing};
use pairly_plugins::screen::ScreenFrame;
use tokio::sync::mpsc;

#[derive(Debug, PartialEq)]
enum Call {
    Start(Facing),
    Stop,
    Started(u32, u32),
    Frame(usize, bool),
    Stopped(String),
}

struct Host(Mutex<mpsc::UnboundedSender<Call>>);

impl Host {
    fn call(&self, c: Call) {
        let _ = self.0.lock().unwrap().send(c);
    }
}

impl CameraHost for Host {
    fn start_camera(&self, _: &PeerInfo, facing: Facing) -> Result<(), String> {
        self.call(Call::Start(facing));
        Ok(())
    }
    fn stop_camera(&self, _: &PeerInfo) {
        self.call(Call::Stop);
    }
    fn camera_started(&self, _: &PeerInfo, w: u32, h: u32) {
        self.call(Call::Started(w, h));
    }
    fn camera_frame(&self, _: &PeerInfo, f: ScreenFrame) {
        self.call(Call::Frame(f.data.len(), f.key));
    }
    fn camera_stopped(&self, _: &PeerInfo, reason: &str) {
        self.call(Call::Stopped(reason.to_owned()));
    }
}

struct Side {
    node: PairlyNode,
    camera: Arc<CameraPlugin>,
    calls: mpsc::UnboundedReceiver<Call>,
}

impl Side {
    async fn next(&mut self) -> Call {
        tokio::time::timeout(Duration::from_secs(5), self.calls.recv())
            .await
            .expect("in time")
            .unwrap()
    }
}

async fn start(net: &MemoryNetwork, addr: &str, kind: DeviceType) -> Side {
    let (tx, calls) = mpsc::unbounded_channel();
    let camera = CameraPlugin::new(Arc::new(Host(Mutex::new(tx))));
    let node = PairlyNode::builder(NodeConfig::new(addr, kind))
        .transport(net.transport(addr))
        .plugin(camera.clone())
        .start()
        .await
        .unwrap();
    Side {
        node,
        camera,
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
async fn the_phone_films_for_the_pc() {
    let net = MemoryNetwork::new();
    let mut pc = start(&net, "pc", DeviceType::Laptop).await;
    let mut phone = start(&net, "phone", DeviceType::Phone).await;
    pair(&pc, &phone).await;
    let (pc_id, phone_id) = (pc.node.device_id(), phone.node.device_id());

    pc.camera
        .request(
            phone_id,
            CameraRequest::Start {
                facing: Facing::Back,
            },
        )
        .unwrap();
    assert_eq!(phone.next().await, Call::Start(Facing::Back));

    phone.camera.started(pc_id, 1280, 720).unwrap();
    assert_eq!(pc.next().await, Call::Started(1280, 720));
    let frame = |len, key| ScreenFrame {
        data: vec![3; len],
        key,
        config: false,
    };
    phone.camera.frame(pc_id, &frame(50_000, true)).unwrap();
    phone.camera.frame(pc_id, &frame(4_000, false)).unwrap();
    assert_eq!(pc.next().await, Call::Frame(50_000, true));
    assert_eq!(pc.next().await, Call::Frame(4_000, false));
    assert!(
        phone.camera.frame(pc_id, &frame(2_000_000, true)).is_err(),
        "size cap"
    );

    pc.camera.request(phone_id, CameraRequest::Stop).unwrap();
    assert_eq!(phone.next().await, Call::Stop);
    phone.camera.stopped(pc_id, "stopped on the PC").unwrap();
    assert_eq!(pc.next().await, Call::Stopped("stopped on the PC".into()));

    pc.node.shutdown().await;
    phone.node.shutdown().await;
}
