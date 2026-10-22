//! A phone's camera as this PC's webcam. The phone's H.264 goes to `pairly-gtk --webcam
//! <device>`, which decodes it into a v4l2loopback device: a camera every app sees (browsers,
//! Zoom, OBS…). The kernel module is the user's to install; without it, Pairly says how.
//!
//! The helper reads the same messages as the screen viewer: `[kind u8][length u32 BE][payload]`
//! with 1 started (`[width u32][height u32]`), 2 frame (`[flags u8][H.264]`), 3 stopped.

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Stdio};
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, Weak};

use pairly_core::{DeviceId, PeerInfo};
use pairly_plugins::camera::{CameraHost, CameraPlugin, CameraRequest, Facing};
use pairly_plugins::screen::ScreenFrame;
use tracing::{info, warn};
use zbus::Connection;

const STARTED: u8 = 1;
const FRAME: u8 = 2;
/// About a second of video waiting for the helper.
const QUEUE_FRAMES: usize = 30;
/// The name Pairly's setup gives the virtual camera (`card_label`).
pub const CAMERA_NAME: &str = "Pairly Camera";

/// How to set up the virtual camera, for when it's missing.
pub const SETUP_HINT: &str = "Set up the virtual camera once (see Pairly's README): \
    install v4l2loopback-dkms, then sudo modprobe v4l2loopback exclusive_caps=1 \
    card_label=\"Pairly Camera\"";

struct Feed {
    tx: SyncSender<Vec<u8>>,
    child: Child,
}

#[derive(Default)]
pub struct Webcam {
    feeds: Mutex<HashMap<DeviceId, Feed>>,
    plugin: OnceLock<Weak<CameraPlugin>>,
    conn: OnceLock<Connection>,
}

fn message(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + payload.len());
    out.push(kind);
    out.extend_from_slice(&u32::try_from(payload.len()).unwrap_or(0).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// The virtual camera's device: the one named [`CAMERA_NAME`], else any v4l2loopback one.
pub fn device() -> Option<PathBuf> {
    let mut found = None;
    for entry in std::fs::read_dir("/sys/class/video4linux").ok()?.flatten() {
        let name = std::fs::read_to_string(entry.path().join("name")).unwrap_or_default();
        let name = name.trim();
        let dev = PathBuf::from("/dev").join(entry.file_name());
        if name == CAMERA_NAME {
            return Some(dev);
        }
        // v4l2loopback's default names.
        if name.contains("Dummy video device") || name.to_ascii_lowercase().contains("loopback") {
            found = Some(dev);
        }
    }
    found
}

impl Webcam {
    pub fn new() -> Arc<Self> {
        Arc::default()
    }

    pub fn set_plugin(&self, plugin: &Arc<CameraPlugin>) {
        let _ = self.plugin.set(Arc::downgrade(plugin));
    }

    pub fn set_conn(&self, conn: Connection) {
        let _ = self.conn.set(conn);
    }

    fn plugin(&self) -> Option<Arc<CameraPlugin>> {
        self.plugin.get().and_then(Weak::upgrade)
    }

    fn feeds(&self) -> MutexGuard<'_, HashMap<DeviceId, Feed>> {
        self.feeds.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn notify(&self, summary: String, body: &str) {
        if let Some(conn) = self.conn.get() {
            crate::notifications::show_simple(conn, summary, body);
        }
    }

    /// Ask `peer` (a phone) to film for this PC (`back`: its back camera), or stop.
    pub fn request(&self, peer: DeviceId, on: bool, back: bool) -> Result<(), String> {
        let plugin = self.plugin().ok_or("the webcam isn't running")?;
        if on && device().is_none() {
            return Err(format!("there's no virtual camera yet. {SETUP_HINT}"));
        }
        let request = if on {
            CameraRequest::Start {
                facing: if back { Facing::Back } else { Facing::Front },
            }
        } else {
            self.end(peer);
            CameraRequest::Stop
        };
        plugin.request(peer, request).map_err(|e| e.to_string())
    }

    /// Whether a phone is filming for this PC now.
    pub fn is_on(&self, peer: DeviceId) -> bool {
        self.feeds().contains_key(&peer)
    }

    fn end(&self, peer: DeviceId) {
        if let Some(mut feed) = self.feeds().remove(&peer) {
            drop(feed.tx);
            let _ = feed.child.wait();
        }
    }

    fn start_feed(&self, from: &PeerInfo, width: u32, height: u32) -> Result<(), String> {
        let device = device().ok_or_else(|| format!("there's no virtual camera. {SETUP_HINT}"))?;
        let mut child = crate::desktop_env::command(crate::laser::gtk_binary())
            .arg("--webcam")
            .arg(&device)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .map_err(|e| format!("can't start the webcam: {e}"))?;
        let stdin = child.stdin.take().ok_or("can't talk to the webcam")?;
        let (tx, rx) = sync_channel::<Vec<u8>>(QUEUE_FRAMES);
        std::thread::Builder::new()
            .name("pairly-webcam".into())
            .spawn(move || write_loop(stdin, &rx))
            .map_err(|e| e.to_string())?;
        let mut size = width.to_be_bytes().to_vec();
        size.extend_from_slice(&height.to_be_bytes());
        let _ = tx.send(message(STARTED, &size));
        if let Some(mut old) = self.feeds().insert(from.id, Feed { tx, child }) {
            let _ = old.child.kill();
        }
        info!(device = %from.id, camera = %device.display(), "the phone's camera is this PC's webcam");
        Ok(())
    }
}

fn write_loop(mut stdin: ChildStdin, rx: &std::sync::mpsc::Receiver<Vec<u8>>) {
    while let Ok(msg) = rx.recv() {
        if stdin.write_all(&msg).and_then(|()| stdin.flush()).is_err() {
            break;
        }
    }
}

impl CameraHost for Webcam {
    fn camera_started(&self, from: &PeerInfo, width: u32, height: u32) {
        match self.start_feed(from, width, height) {
            Ok(()) => self.notify(
                format!("{}'s camera is your webcam", from.name),
                &format!("Pick “{CAMERA_NAME}” as the camera in your video app."),
            ),
            Err(e) => {
                warn!(error = %e, "can't use the phone's camera");
                if let Some(plugin) = self.plugin() {
                    let _ = plugin.request(from.id, CameraRequest::Stop);
                }
                self.notify("Can't use the phone's camera".into(), &e);
            }
        }
    }

    fn camera_frame(&self, from: &PeerInfo, frame: ScreenFrame) {
        let tx = match self.feeds().get(&from.id) {
            Some(feed) => feed.tx.clone(),
            None => return,
        };
        let mut payload = Vec::with_capacity(1 + frame.data.len());
        payload.push(u8::from(frame.key || frame.config));
        payload.extend_from_slice(&frame.data);
        let msg = message(FRAME, &payload);
        // Codec setup and key frames must get through; others may be dropped.
        if let Err(TrySendError::Full(msg)) = tx.try_send(msg)
            && (frame.key || frame.config)
        {
            let _ = tx.send(msg);
        }
    }

    fn camera_stopped(&self, from: &PeerInfo, reason: &str) {
        if self.is_on(from.id) {
            self.end(from.id);
            self.notify(format!("{}'s camera stopped", from.name), reason);
        }
    }

    fn disconnected(&self, peer: DeviceId) {
        self.end(peer);
    }
}
