//! Copying an image on one device and pasting it on the other, over the in-memory network.
#![allow(clippy::unwrap_used)] // test helpers

use std::sync::{Arc, Mutex};
use std::time::Duration;

use pairly_core::memory::MemoryNetwork;
use pairly_core::{DeviceType, NodeConfig, NodeEvent, PairlyNode, PeerInfo};
use pairly_plugins::clipboard::{ClipboardHost, ClipboardPlugin, IMAGE_PIECE, image_type};
use tokio::sync::mpsc;

#[derive(Debug, PartialEq)]
enum Got {
    Text(String),
    Image(String, Vec<u8>),
}

struct Host(Mutex<mpsc::UnboundedSender<Got>>);

impl ClipboardHost for Host {
    fn set_clipboard(&self, _: &PeerInfo, text: &str) {
        let _ = self.0.lock().unwrap().send(Got::Text(text.to_owned()));
    }
    fn set_clipboard_image(&self, _: &PeerInfo, mime: &str, data: &[u8]) {
        let _ = self
            .0
            .lock()
            .unwrap()
            .send(Got::Image(mime.to_owned(), data.to_vec()));
    }
}

async fn start(
    net: &MemoryNetwork,
    addr: &str,
) -> (
    PairlyNode,
    Arc<ClipboardPlugin>,
    mpsc::UnboundedReceiver<Got>,
) {
    let (tx, rx) = mpsc::unbounded_channel();
    let clipboard = ClipboardPlugin::new(Arc::new(Host(Mutex::new(tx))));
    let node = PairlyNode::builder(NodeConfig::new(addr, DeviceType::Laptop))
        .transport(net.transport(addr))
        .plugin(clipboard.clone())
        .start()
        .await
        .unwrap();
    (node, clipboard, rx)
}

async fn pair(a: &PairlyNode, b: &PairlyNode) {
    let (mut ae, mut be) = (a.subscribe(), b.subscribe());
    let (a_id, b_id) = (a.device_id(), b.device_id());
    while !a.devices().unwrap().iter().any(|d| d.id == b_id) {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    a.request_pair(b_id).await.unwrap();
    for (rx, node, peer) in [(&mut ae, a, b_id), (&mut be, b, a_id)] {
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

async fn next(rx: &mut mpsc::UnboundedReceiver<Got>) -> Got {
    tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("in time")
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_copied_image_pastes_on_the_other_device() {
    let net = MemoryNetwork::new();
    let (pc, pc_clip, mut pc_got) = start(&net, "pc").await;
    let (laptop, laptop_clip, mut laptop_got) = start(&net, "laptop").await;
    pair(&pc, &laptop).await;

    // Several pieces, the last one partial.
    let screenshot: Vec<u8> = (0..(3 * IMAGE_PIECE + 4321))
        .map(|i| (i % 253) as u8)
        .collect();
    pc_clip.local_image_changed("image/png", &screenshot);
    assert_eq!(
        next(&mut laptop_got).await,
        Got::Image("image/png".into(), screenshot.clone())
    );

    // The laptop's clipboard now holds it, and its watcher reports that: no bounce back.
    laptop_clip.local_image_changed("image/png", &screenshot);
    // Text still works alongside, and arrives next on the PC (nothing came before it).
    laptop_clip.local_changed("hello");
    assert_eq!(next(&mut pc_got).await, Got::Text("hello".into()));

    // Not images, or too big: refused before sending.
    assert!(
        pc_clip
            .send_image_to(laptop.device_id(), "text/plain", b"x")
            .is_err()
    );
    let huge = vec![0u8; pairly_plugins::clipboard::MAX_IMAGE_BYTES + 1];
    assert!(
        pc_clip
            .send_image_to(laptop.device_id(), "image/png", &huge)
            .is_err()
    );

    pc.shutdown().await;
    laptop.shutdown().await;
}

#[test]
fn image_files_are_recognised_by_their_bytes() {
    assert_eq!(image_type(b"\x89PNG\r\n\x1a\n...."), Some("image/png"));
    assert_eq!(
        image_type(&[0xFF, 0xD8, 0xFF, 0xE0, 0, 0x10]),
        Some("image/jpeg")
    );
    assert_eq!(image_type(b"GIF89a.."), Some("image/gif"));
    assert_eq!(image_type(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
    assert_eq!(image_type(b"RIFF\0\0\0\0WAVEfmt "), None, "a sound file");
    assert_eq!(image_type(b"%PDF-1.7"), None);
    assert_eq!(image_type(b""), None);
}
