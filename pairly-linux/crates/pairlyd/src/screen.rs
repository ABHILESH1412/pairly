//! Screen sharing, both ways.
//!
//! **A phone's screen on this PC:** asking for it starts `pairly-gtk --screen` (the viewer),
//! the phone's video goes to it, and what you do in its window goes back to the phone. Frames
//! pass through a short queue: if the viewer falls behind, new frames are dropped rather than
//! piling up. Closing the window stops the phone's sharing; the phone stopping (or leaving)
//! tells the window why.
//!
//! **This PC's screen on a phone:** a phone asking starts `pairly-gtk --cast` (the desktop's
//! screen-sharing portal decides which screen, asking the first time), its H.264 frames go to
//! the phone, and the phone's touches become clicks, drags, scrolling and typing here. A
//! notification shows while it lasts. `[screen] share_with_phones = false` turns it off.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{ChildStdin, Command, Stdio};
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, Weak};

use pairly_core::{DeviceId, PeerInfo};
use pairly_plugins::input::{
    ButtonAction, InputHost, KeyInput, Modifiers, MouseButton, PointerButton, SpecialKey,
};
use pairly_plugins::screen::{
    ScreenAction, ScreenFrame, ScreenHost, ScreenInput, ScreenKey, ScreenPlugin,
};
use tracing::{debug, info, warn};

/// About half a second of video.
const QUEUE_FRAMES: usize = 16;

const STARTED: u8 = 1;
const FRAME: u8 = 2;
const STOPPED: u8 = 3;

struct Viewer {
    tx: SyncSender<Vec<u8>>,
    /// Tells this viewer apart from a newer one for the same phone.
    id: u64,
}

/// This PC's screen being sent to a phone.
struct Cast {
    stdin: ChildStdin,
    /// The shared monitor: its place in the desktop and its height (for scrolling).
    monitor: (i32, i32),
    height: u32,
    notification: Option<u32>,
}

#[derive(Default)]
pub struct LinuxScreen {
    viewers: Mutex<HashMap<DeviceId, Viewer>>,
    casts: Mutex<HashMap<DeviceId, Cast>>,
    next_id: Mutex<u64>,
    plugin: OnceLock<Weak<ScreenPlugin>>,
    /// Set at startup (see [`LinuxScreen::setup`]).
    sharing: OnceLock<Sharing>,
    /// Itself, for work finished on other tasks.
    this: OnceLock<Weak<Self>>,
}

/// What sharing this PC's screen needs.
struct Sharing {
    allowed: bool,
    input: Arc<crate::input::LinuxInput>,
    token_file: std::path::PathBuf,
    conn: zbus::Connection,
    runtime: tokio::runtime::Handle,
}

fn message(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + payload.len());
    out.push(kind);
    out.extend_from_slice(&u32::try_from(payload.len()).unwrap_or(0).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

impl LinuxScreen {
    pub fn new() -> Arc<Self> {
        let screen = Arc::new(Self::default());
        let _ = screen.this.set(Arc::downgrade(&screen));
        screen
    }

    pub fn set_plugin(&self, plugin: &Arc<ScreenPlugin>) {
        let _ = self.plugin.set(Arc::downgrade(plugin));
    }

    /// Enable sharing this PC's screen with phones (call once at startup).
    pub fn setup(
        &self,
        allowed: bool,
        input: Arc<crate::input::LinuxInput>,
        data_dir: &std::path::Path,
        conn: zbus::Connection,
    ) {
        let _ = self.sharing.set(Sharing {
            allowed,
            input,
            token_file: data_dir.join("screencast-token"),
            conn,
            runtime: tokio::runtime::Handle::current(),
        });
    }

    fn casts(&self) -> MutexGuard<'_, HashMap<DeviceId, Cast>> {
        self.casts.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A phone asks to see this screen: start the portal recording.
    fn start_cast(self: &Arc<Self>, from: &PeerInfo) -> Result<(), String> {
        let sharing = self.sharing.get().ok_or("screen sharing isn't ready")?;
        if !sharing.allowed {
            return Err(
                "this PC doesn't share its screen ([screen] share_with_phones = false)".into(),
            );
        }
        if self.casts().contains_key(&from.id) {
            return Ok(());
        }
        let mut child = Command::new(crate::laser::gtk_binary())
            .arg("--cast")
            .arg(&sharing.token_file)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|e| format!("can't start screen recording: {e}"))?;
        let (stdin, stdout) = match (child.stdin.take(), child.stdout.take()) {
            (Some(i), Some(o)) => (i, o),
            _ => return Err("can't talk to the screen recorder".into()),
        };
        self.casts().insert(
            from.id,
            Cast {
                stdin,
                monitor: (0, 0),
                height: 1080,
                notification: None,
            },
        );
        info!(device = %from.id, "sharing this PC's screen");
        let this = Arc::downgrade(self);
        let peer = from.clone();
        std::thread::Builder::new()
            .name("pairly-cast".into())
            .spawn(move || {
                let mut out = BufReader::new(stdout);
                let mut said_stop = false;
                while let Some((kind, body)) = read_message(&mut out) {
                    let Some(this) = this.upgrade() else { break };
                    said_stop |= this.handle_cast_message(&peer, kind, body);
                }
                let _ = child.wait();
                if let Some(this) = this.upgrade() {
                    this.end_cast(
                        &peer,
                        (!said_stop).then_some("the screen recording stopped"),
                    );
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// A message from the recorder; true if it said it stopped.
    fn handle_cast_message(&self, peer: &PeerInfo, kind: u8, body: Vec<u8>) -> bool {
        let Some(plugin) = self.plugin() else {
            return false;
        };
        match kind {
            STARTED if body.len() >= 24 => {
                let num = |i: usize| [body[i], body[i + 1], body[i + 2], body[i + 3]];
                let (w, h) = (u32::from_be_bytes(num(0)), u32::from_be_bytes(num(4)));
                let at = (i32::from_be_bytes(num(8)), i32::from_be_bytes(num(12)));
                let height = u32::from_be_bytes(num(20));
                if let Some(cast) = self.casts().get_mut(&peer.id) {
                    cast.monitor = at;
                    cast.height = height.max(1);
                }
                self.notify_cast(peer);
                if let Err(e) = plugin.started(peer.id, w, h) {
                    warn!(device = %peer.id, error = %e, "can't start sharing the screen");
                }
                false
            }
            FRAME if !body.is_empty() => {
                let frame = ScreenFrame {
                    key: body[0] & 1 != 0,
                    config: false,
                    data: body[1..].to_vec(),
                };
                // The link fell behind: ask for a key frame to resume from.
                if let Ok(true) = plugin.frame(peer.id, &frame)
                    && let Some(cast) = self.casts().get_mut(&peer.id)
                {
                    let _ = writeln!(cast.stdin, "key");
                }
                false
            }
            STOPPED => {
                self.end_cast(peer, Some(&String::from_utf8_lossy(&body)));
                true
            }
            _ => false,
        }
    }

    /// The sharing ended: tell the phone (with `reason`) and take the notification down.
    fn end_cast(&self, peer: &PeerInfo, reason: Option<&str>) {
        let Some(cast) = self.casts().remove(&peer.id) else {
            return;
        };
        info!(device = %peer.id, reason, "stopped sharing this PC's screen");
        if let (Some(reason), Some(plugin)) = (reason, self.plugin()) {
            let _ = plugin.stopped(peer.id, reason);
        }
        if let (Some(id), Some(sharing)) = (cast.notification, self.sharing.get()) {
            let conn = sharing.conn.clone();
            sharing
                .runtime
                .spawn(async move { crate::notifications::close(&conn, id).await });
        }
    }

    /// "Your screen is being shared": shown for as long as it is.
    fn notify_cast(&self, peer: &PeerInfo) {
        let Some(sharing) = self.sharing.get() else {
            return;
        };
        let (conn, name, me) = (sharing.conn.clone(), peer.name.clone(), peer.id);
        let screen = self.weak_self();
        sharing.runtime.spawn(async move {
            let id = crate::notifications::notify(
                &conn,
                &format!("Sharing your screen with {name}"),
                "It can see your screen and control the mouse and keyboard",
                &[],
                true,
            )
            .await;
            if let Some(screen) = screen.and_then(|w| w.upgrade())
                && let Some(cast) = screen.casts().get_mut(&me)
            {
                cast.notification = id;
            }
        });
    }

    fn weak_self(&self) -> Option<Weak<Self>> {
        self.this.get().cloned()
    }

    /// The phone's touches and keys, as this PC's pointer and keyboard.
    fn control(&self, from: &PeerInfo, input: ScreenInput) {
        let Some(sharing) = self.sharing.get() else {
            return;
        };
        let Some((monitor, height)) = self.casts().get(&from.id).map(|c| (c.monitor, c.height))
        else {
            return;
        };
        let pc = &sharing.input;
        let click = |button| {
            pc.press(PointerButton {
                button,
                action: ButtonAction::Click,
            });
        };
        match input {
            ScreenInput::Tap { x, y } => {
                pc.place(monitor, x, y);
                click(MouseButton::Left);
            }
            ScreenInput::LongPress { x, y } => {
                pc.place(monitor, x, y);
                click(MouseButton::Right);
            }
            ScreenInput::Swipe {
                points,
                duration_ms,
            } => {
                // Hold the button while moving through the points, at the swipe's pace.
                let pc = pc.clone();
                std::thread::spawn(move || {
                    let step = std::time::Duration::from_millis(
                        u64::from(duration_ms) / points.len().max(1) as u64,
                    );
                    let mut points = points.into_iter();
                    let Some((x, y)) = points.next() else { return };
                    pc.place(monitor, x, y);
                    let button = |action| PointerButton {
                        button: MouseButton::Left,
                        action,
                    };
                    pc.press(button(ButtonAction::Press));
                    for (x, y) in points {
                        std::thread::sleep(step.min(std::time::Duration::from_millis(40)));
                        pc.place(monitor, x, y);
                    }
                    pc.press(button(ButtonAction::Release));
                });
            }
            // Two fingers: the content follows them (as on a touch screen).
            ScreenInput::Scroll { dx, dy } => {
                let px = height as f32;
                pc.scroll(-dx * px, -dy * px);
            }
            ScreenInput::Key { key } => {
                let special = match key {
                    ScreenKey::Back => SpecialKey::Escape,
                    ScreenKey::Enter => SpecialKey::Enter,
                    ScreenKey::Backspace => SpecialKey::Backspace,
                    ScreenKey::Delete => SpecialKey::Delete,
                    ScreenKey::Left => SpecialKey::Left,
                    ScreenKey::Right => SpecialKey::Right,
                    ScreenKey::Up => SpecialKey::Up,
                    ScreenKey::Down => SpecialKey::Down,
                    ScreenKey::Home | ScreenKey::Recents => return,
                };
                pc.key(
                    from,
                    &KeyInput {
                        text: None,
                        key: Some(special),
                        modifiers: Modifiers::default(),
                    },
                );
            }
            ScreenInput::Text { text } => pc.key(
                from,
                &KeyInput {
                    text: Some(text),
                    key: None,
                    modifiers: Modifiers::default(),
                },
            ),
        }
    }

    fn plugin(&self) -> Option<Arc<ScreenPlugin>> {
        self.plugin.get().and_then(Weak::upgrade)
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<DeviceId, Viewer>> {
        self.viewers.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Open the viewer for `peer` (named `name`) and ask the phone to share.
    pub fn open(self: &Arc<Self>, peer: DeviceId, name: &str) -> Result<(), String> {
        let plugin = self.plugin().ok_or("screen sharing isn't running")?;
        // Ask first: it fails right away if the phone isn't on the same Wi-Fi.
        plugin
            .request(peer, ScreenAction::Start)
            .map_err(|e| e.to_string())?;
        if self.lock().contains_key(&peer) {
            return Ok(()); // already watching: the phone just asks again
        }
        let mut child = Command::new(crate::laser::gtk_binary())
            .args(["--screen", name])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|e| format!("can't open the viewer: {e}"))?;
        let (stdin, stdout) = match (child.stdin.take(), child.stdout.take()) {
            (Some(i), Some(o)) => (i, o),
            _ => return Err("can't talk to the viewer".into()),
        };
        let (tx, rx) = sync_channel::<Vec<u8>>(QUEUE_FRAMES);
        let id = {
            let mut n = self.next_id.lock().unwrap_or_else(PoisonError::into_inner);
            *n += 1;
            *n
        };
        self.lock().insert(peer, Viewer { tx, id });
        info!(device = %peer, "showing the phone's screen");

        std::thread::Builder::new()
            .name("pairly-screen-out".into())
            .spawn(move || write_loop(stdin, &rx))
            .map_err(|e| e.to_string())?;
        let this = Arc::downgrade(self);
        std::thread::Builder::new()
            .name("pairly-screen-in".into())
            .spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    let Ok(line) = line else { break };
                    let Some(this) = this.upgrade() else { break };
                    this.handle_viewer_line(peer, &line);
                }
                let _ = child.wait();
                // The window closed: stop the phone's sharing, unless a newer viewer took over.
                if let Some(this) = this.upgrade() {
                    let mut viewers = this.lock();
                    if viewers.get(&peer).is_some_and(|v| v.id == id) {
                        viewers.remove(&peer);
                        drop(viewers);
                        if let Some(plugin) = this.plugin() {
                            let _ = plugin.request(peer, ScreenAction::Stop);
                        }
                        info!(device = %peer, "phone screen closed");
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// A line from the viewer: something to do on the phone.
    fn handle_viewer_line(&self, peer: DeviceId, line: &str) {
        let Some(plugin) = self.plugin() else { return };
        let input = match parse_input(line) {
            Some(i) => i,
            None => {
                if line.trim() == "stop" {
                    let _ = plugin.request(peer, ScreenAction::Stop);
                }
                return;
            }
        };
        if let Err(e) = plugin.input(peer, input) {
            debug!(device = %peer, error = %e, "screen input not sent");
        }
    }

    /// Queue a message for `peer`'s viewer; frames are dropped if it's behind, others wait.
    fn to_viewer(&self, peer: DeviceId, msg: Vec<u8>, droppable: bool) {
        let tx = match self.lock().get(&peer) {
            Some(v) => v.tx.clone(),
            None => return,
        };
        match tx.try_send(msg) {
            Ok(()) | Err(TrySendError::Disconnected(_)) => {}
            Err(TrySendError::Full(msg)) => {
                if !droppable {
                    let _ = tx.send(msg);
                }
            }
        }
    }
}

fn write_loop(mut stdin: ChildStdin, rx: &std::sync::mpsc::Receiver<Vec<u8>>) {
    while let Ok(msg) = rx.recv() {
        if stdin.write_all(&msg).and_then(|()| stdin.flush()).is_err() {
            break;
        }
    }
}

/// `tap x y`, `long x y`, `swipe ms x y x y…`, `key back|home|recents|enter|backspace`,
/// `text …`.
fn parse_input(line: &str) -> Option<ScreenInput> {
    let (word, rest) = line.split_once(' ').unwrap_or((line, ""));
    let nums = || -> Vec<f32> {
        rest.split_whitespace()
            .filter_map(|w| w.parse().ok())
            .collect()
    };
    Some(match word {
        "tap" | "long" => {
            let n = nums();
            let (x, y) = (*n.first()?, *n.get(1)?);
            if word == "tap" {
                ScreenInput::Tap { x, y }
            } else {
                ScreenInput::LongPress { x, y }
            }
        }
        "swipe" => {
            let n = nums();
            let (ms, coords) = n.split_first()?;
            ScreenInput::Swipe {
                points: coords
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|[x, y]| (*x, *y))
                    .collect(),
                duration_ms: ms.max(0.0) as u32,
            }
        }
        "key" => ScreenInput::Key {
            key: match rest.trim() {
                "back" => ScreenKey::Back,
                "home" => ScreenKey::Home,
                "recents" => ScreenKey::Recents,
                "enter" => ScreenKey::Enter,
                "backspace" => ScreenKey::Backspace,
                "delete" => ScreenKey::Delete,
                "left" => ScreenKey::Left,
                "right" => ScreenKey::Right,
                "up" => ScreenKey::Up,
                "down" => ScreenKey::Down,
                _ => return None,
            },
        },
        "text" if !rest.is_empty() => ScreenInput::Text {
            text: rest.to_owned(),
        },
        _ => return None,
    })
}

/// One `[kind][length][payload]` message from the recorder.
fn read_message(r: &mut impl std::io::Read) -> Option<(u8, Vec<u8>)> {
    let mut head = [0_u8; 5];
    r.read_exact(&mut head).ok()?;
    let len = u32::from_be_bytes([head[1], head[2], head[3], head[4]]) as usize;
    if len > 4 * 1024 * 1024 {
        return None;
    }
    let mut body = vec![0_u8; len];
    r.read_exact(&mut body).ok()?;
    Some((head[0], body))
}

impl ScreenHost for LinuxScreen {
    fn start_sharing(&self, from: &PeerInfo) -> Result<(), String> {
        let this = self
            .this
            .get()
            .and_then(Weak::upgrade)
            .ok_or("screen sharing isn't ready")?;
        this.start_cast(from)
    }

    fn stop_sharing(&self, from: &PeerInfo) {
        if let Some(cast) = self.casts().get_mut(&from.id) {
            let _ = writeln!(cast.stdin, "stop");
        }
    }

    fn input(&self, from: &PeerInfo, input: ScreenInput) {
        self.control(from, input);
    }

    fn started(&self, from: &PeerInfo, width: u32, height: u32) {
        info!(device = %from.id, width, height, "the phone started sharing its screen");
        let mut size = width.to_be_bytes().to_vec();
        size.extend_from_slice(&height.to_be_bytes());
        self.to_viewer(from.id, message(STARTED, &size), false);
    }

    fn frame(&self, from: &PeerInfo, frame: ScreenFrame) {
        let mut payload = Vec::with_capacity(1 + frame.data.len());
        payload.push(u8::from(frame.key || frame.config));
        payload.extend_from_slice(&frame.data);
        // Codec setup and key frames must get through; ordinary frames may be dropped.
        let droppable = !(frame.key || frame.config);
        self.to_viewer(from.id, message(FRAME, &payload), droppable);
    }

    fn stopped(&self, from: &PeerInfo, reason: &str) {
        if self.lock().contains_key(&from.id) {
            warn!(device = %from.id, reason, "the phone stopped sharing its screen");
            self.to_viewer(from.id, message(STOPPED, reason.as_bytes()), false);
        }
    }

    fn disconnected(&self, peer: DeviceId) {
        self.to_viewer(peer, message(STOPPED, b"the phone disconnected"), false);
        if let Some(cast) = self.casts().get_mut(&peer) {
            let _ = writeln!(cast.stdin, "stop");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viewer_lines_become_input() {
        assert_eq!(
            parse_input("tap 0.25 0.5"),
            Some(ScreenInput::Tap { x: 0.25, y: 0.5 })
        );
        assert_eq!(
            parse_input("swipe 300 0.5 0.8 0.5 0.6 0.5 0.2"),
            Some(ScreenInput::Swipe {
                points: vec![(0.5, 0.8), (0.5, 0.6), (0.5, 0.2)],
                duration_ms: 300
            })
        );
        assert_eq!(
            parse_input("key recents"),
            Some(ScreenInput::Key {
                key: ScreenKey::Recents
            })
        );
        assert_eq!(
            parse_input("text héllo wörld"),
            Some(ScreenInput::Text {
                text: "héllo wörld".into()
            })
        );
        // A typed space is the text " ".
        assert_eq!(
            parse_input("text  "),
            Some(ScreenInput::Text { text: " ".into() })
        );
        assert_eq!(parse_input("tap 0.5"), None);
        assert_eq!(parse_input("stop"), None);
    }
}
