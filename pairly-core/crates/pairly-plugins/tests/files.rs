//! Browsing another device's files over the in-memory network: a PC shares a folder read-only,
//! with hidden files; the other side lists it and downloads a multi-piece file.
#![allow(clippy::unwrap_used)] // test helpers

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use pairly_core::memory::MemoryNetwork;
use pairly_core::{DeviceType, NodeConfig, NodeEvent, PairlyNode};
use pairly_plugins::files::{CHUNK, FileOp, FilesHost, FilesPlugin};

/// A PC sharing `root`, read-only, hidden files kept out.
struct Shared(PathBuf);

impl FilesHost for Shared {
    fn root(&self) -> Result<PathBuf, String> {
        Ok(self.0.clone())
    }
    fn allow_changes(&self) -> bool {
        false
    }
    fn show_hidden(&self) -> bool {
        false
    }
}

/// Shares nothing (the default).
struct Nothing;
impl FilesHost for Nothing {}

async fn start(
    net: &MemoryNetwork,
    addr: &str,
    host: Arc<dyn FilesHost>,
) -> (PairlyNode, Arc<FilesPlugin>) {
    let files = FilesPlugin::new(host);
    let node = PairlyNode::builder(NodeConfig::new(addr, DeviceType::Laptop))
        .transport(net.transport(addr))
        .plugin(files.clone())
        .start()
        .await
        .unwrap();
    (node, files)
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

#[tokio::test(flavor = "multi_thread")]
async fn browse_and_download_a_shared_folder() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    std::fs::create_dir_all(root.join("Documents")).unwrap();
    std::fs::create_dir_all(root.join(".ssh")).unwrap();
    std::fs::write(root.join(".ssh/id"), "secret").unwrap();
    // Several pieces, the last one partial.
    let big: Vec<u8> = (0..(3 * CHUNK as usize + 12_345))
        .map(|i| (i % 251) as u8)
        .collect();
    std::fs::write(root.join("Documents/report.bin"), &big).unwrap();

    let net = MemoryNetwork::new();
    let (pc, pc_files) = start(&net, "pc", Arc::new(Shared(root.clone()))).await;
    let (phone, files) = start(&net, "phone", Arc::new(Nothing)).await;
    pair(&phone, &pc).await;
    let pc_id = pc.device_id();

    let top = files.list(pc_id, "").await.unwrap();
    let names: Vec<&str> = top.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["Documents"], "hidden folders aren't listed");
    let docs = files.list(pc_id, "Documents").await.unwrap();
    assert_eq!(
        (docs[0].name.as_str(), docs[0].size),
        ("report.bin", big.len() as u64)
    );

    let mut got = Vec::new();
    let mut last = 0;
    let size = files
        .download(
            pc_id,
            "Documents/report.bin",
            docs[0].size,
            |data| {
                got.extend_from_slice(data);
                Ok(())
            },
            |done| {
                assert!(done > last, "progress only grows");
                last = done;
            },
        )
        .await
        .unwrap();
    assert_eq!(size, big.len() as u64);
    assert!(got == big, "the download is byte-for-byte the file");

    // Read-only and no hidden files, whatever the asker tries.
    assert!(files.list(pc_id, ".ssh").await.is_err());
    assert!(files.read(pc_id, ".ssh/id", 0, 10).await.is_err());
    let delete = FileOp::Delete {
        path: "Documents/report.bin".into(),
    };
    assert!(files.ask(pc_id, delete).await.is_err());
    assert!(root.join("Documents/report.bin").exists());

    // A device that shares nothing says so.
    let err = pc_files
        .list(phone.device_id(), "")
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("doesn't share"), "{err}");

    pc.shutdown().await;
    phone.shutdown().await;
}
