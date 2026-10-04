//! Media players, artwork and commands between two nodes over the in-memory transport.
#![allow(clippy::unwrap_used)] // test helpers

use std::sync::{Arc, Mutex};
use std::time::Duration;

use pairly_core::memory::MemoryNetwork;
use pairly_core::{DeviceType, NodeConfig, NodeEvent, PairlyNode, PeerInfo};
use pairly_plugins::media::{MediaAction, MediaHost, MediaPlugin, PlayerState};
use tokio::sync::mpsc;

#[derive(Debug, PartialEq)]
enum Call {
    Players(Vec<String>),
    Art(String, usize),
    Command(String, MediaAction),
}

struct Host {
    players: Mutex<Vec<PlayerState>>,
    calls: Mutex<mpsc::UnboundedSender<Call>>,
}

impl MediaHost for Host {
    fn players(&self) -> Vec<PlayerState> {
        self.players.lock().unwrap().clone()
    }
    fn artwork(&self, key: &str) -> Option<Vec<u8>> {
        (key == "file:///art.png").then(|| vec![1; 1000])
    }
    fn peer_players(&self, _: &PeerInfo, players: &[PlayerState]) {
        let titles = players.iter().map(|p| p.title.clone()).collect();
        let _ = self.calls.lock().unwrap().send(Call::Players(titles));
    }
    fn peer_artwork(&self, _: &PeerInfo, key: &str, data: &[u8]) {
        let _ = self
            .calls
            .lock()
            .unwrap()
            .send(Call::Art(key.into(), data.len()));
    }
    fn command(&self, _: &PeerInfo, player: &str, action: MediaAction) {
        let _ = self
            .calls
            .lock()
            .unwrap()
            .send(Call::Command(player.into(), action));
    }
}

struct Side {
    node: PairlyNode,
    media: Arc<MediaPlugin>,
    host: Arc<Host>,
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

async fn start(net: &MemoryNetwork, addr: &str, players: Vec<PlayerState>) -> Side {
    let (tx, calls) = mpsc::unbounded_channel();
    let host = Arc::new(Host {
        players: Mutex::new(players),
        calls: Mutex::new(tx),
    });
    let media = MediaPlugin::new(host.clone());
    let node = PairlyNode::builder(NodeConfig::new(addr, DeviceType::Phone))
        .transport(net.transport(addr))
        .plugin(media.clone())
        .start()
        .await
        .unwrap();
    Side {
        node,
        media,
        host,
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

fn player(title: &str, art: Option<&str>) -> PlayerState {
    PlayerState {
        id: "org.mpris.MediaPlayer2.vlc".into(),
        name: "VLC".into(),
        title: title.into(),
        art: art.map(Into::into),
        playing: true,
        ..Default::default()
    }
}

#[tokio::test]
async fn players_artwork_and_commands() {
    let net = MemoryNetwork::new();
    let mut pc = start(&net, "pc", vec![player("Song A", Some("file:///art.png"))]).await;
    let mut phone = start(&net, "phone", vec![]).await;
    pair(&phone, &pc).await;

    // On connect: the PC's player, then its artwork. The phone has nothing playing.
    let mut got = vec![phone.next().await, phone.next().await];
    got.sort_by_key(|c| format!("{c:?}"));
    assert_eq!(
        got,
        vec![
            Call::Art("file:///art.png".into(), 1000),
            Call::Players(vec!["Song A".into()]),
        ]
    );
    assert_eq!(pc.next().await, Call::Players(vec![]));

    // A new track with the same artwork: the art isn't sent again.
    *pc.host.players.lock().unwrap() = vec![player("Song B", Some("file:///art.png"))];
    pc.media.local_changed();
    assert_eq!(phone.next().await, Call::Players(vec!["Song B".into()]));
    let quiet = tokio::time::timeout(Duration::from_millis(300), phone.calls.recv()).await;
    assert!(quiet.is_err(), "artwork resent: {quiet:?}");
    assert_eq!(
        phone.media.peer_players(pc.node.device_id())[0].title,
        "Song B"
    );

    // The phone controls the PC's player.
    phone
        .media
        .command(
            pc.node.device_id(),
            "org.mpris.MediaPlayer2.vlc",
            MediaAction::Seek(-5000),
        )
        .unwrap();
    assert_eq!(
        pc.next().await,
        Call::Command(
            "org.mpris.MediaPlayer2.vlc".into(),
            MediaAction::Seek(-5000)
        )
    );

    // Disconnecting clears the PC's players on the phone.
    pc.node.shutdown().await;
    assert_eq!(phone.next().await, Call::Players(vec![]));
}
