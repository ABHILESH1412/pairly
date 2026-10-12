//! Screens, both ways, and the presentation laser pointer.
//!
//! **A phone's screen on this PC:** a window of its own (`screen.html`) asks the phone to share,
//! receives the phone's H.264 video over a channel (messages `[kind u8][payload]`: 1 started
//! `[width u32 BE][height u32 BE]`, 2 frame `[flags u8][access unit]` with flag 1 key frame and
//! 2 codec setup, 3 stopped `[reason]`) and decodes it in the page (WebCodecs, on the GPU).
//! What you do in the window goes back to the phone as taps, swipes, keys and text. Closing the
//! window stops the phone's sharing.
//!
//! **This PC's screen on a phone:** a phone asking starts a recording of the main screen
//! ([`crate::platform::capture`]), encoded to H.264 and sent to the phone; the phone's touches
//! become clicks, drags, scrolling and typing here. A notification shows while it lasts, with a
//! button to stop it.
//!
//! **The laser pointer:** a phone holding its "Pointer" button moves a red dot over everything
//! on screen, drawn by a click-through, always-on-top window (`laser.html`) that closes after
//! a minute without pointing.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use pairly_core::{DeviceId, PeerInfo};
use pairly_plugins::input::{
    ButtonAction, KeyInput, LaserAction, LaserPointer, Modifiers, MouseButton, PointerButton,
    SpecialKey,
};
use pairly_plugins::screen::{ScreenAction, ScreenFrame, ScreenInput, ScreenKey};
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::{Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

use crate::backend::{Backend, lock};
use crate::platform::capture::{self, Recording};
use crate::platform::toast;

const STARTED: u8 = 1;
const FRAME: u8 = 2;
const STOPPED: u8 = 3;

/// Close the laser overlay this long after the pointer was last hidden.
const LASER_IDLE: Duration = Duration::from_secs(60);

struct Cast {
    recording: Recording,
    /// Where the shared monitor is on the desktop: (x, y, width, height) in pixels.
    monitor: (i32, i32, u32, u32),
}

#[derive(Default)]
struct Laser {
    open: bool,
    hidden_since: Option<Instant>,
}

pub struct Screens {
    backend: Weak<Backend>,
    viewers: Mutex<HashMap<DeviceId, Channel<InvokeResponseBody>>>,
    casts: Mutex<HashMap<DeviceId, Cast>>,
    laser: Mutex<Laser>,
}

pub fn label(peer: DeviceId) -> String {
    format!("screen-{peer}")
}

fn message(kind: u8, payload: &[u8]) -> InvokeResponseBody {
    let mut out = Vec::with_capacity(payload.len() + 1);
    out.push(kind);
    out.extend_from_slice(payload);
    InvokeResponseBody::Raw(out)
}

impl Screens {
    pub fn new(backend: Weak<Backend>) -> Arc<Self> {
        let this = Arc::new(Self {
            backend,
            viewers: Mutex::default(),
            casts: Mutex::default(),
            laser: Mutex::default(),
        });
        // Close the laser overlay once it's been idle a while.
        let weak = Arc::downgrade(&this);
        std::thread::Builder::new()
            .name("pairly-laser".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(Duration::from_secs(5));
                    let Some(this) = weak.upgrade() else { return };
                    let idle = {
                        let l = lock(&this.laser);
                        l.open && l.hidden_since.is_some_and(|t| t.elapsed() > LASER_IDLE)
                    };
                    if idle {
                        this.close_laser();
                    }
                }
            })
            .ok();
        this
    }

    pub fn backend(&self) -> Weak<Backend> {
        self.backend.clone()
    }

    fn plugin(&self) -> Option<Arc<pairly_plugins::screen::ScreenPlugin>> {
        Some(self.backend.upgrade()?.features().ok()?.screen)
    }

    /// Devices this PC's screen is shown on.
    pub fn casting(&self) -> Vec<DeviceId> {
        lock(&self.casts).keys().copied().collect()
    }

    /// Everything stops (Pairly is switched off).
    pub fn stop_all(&self) {
        let viewers: Vec<DeviceId> = lock(&self.viewers).keys().copied().collect();
        for peer in viewers {
            self.viewer_stopped(peer, "Pairly was switched off");
        }
        let casts: Vec<DeviceId> = self.casting();
        for peer in casts {
            self.stop_cast(peer, None);
        }
    }

    // ----- a phone's screen here -----------------------------------------------------------

    /// Open (or bring up) the window showing a phone's screen.
    pub fn open_viewer(&self, peer: DeviceId, name: &str) -> anyhow::Result<()> {
        let b = self
            .backend
            .upgrade()
            .ok_or_else(|| anyhow::anyhow!("Pairly is stopping"))?;
        if let Some(w) = b.app.get_webview_window(&label(peer)) {
            let _ = w.unminimize();
            let _ = w.set_focus();
            return Ok(());
        }
        let query = format!("screen.html?id={peer}&name={}", url_escape(name));
        WebviewWindowBuilder::new(&b.app, label(peer), WebviewUrl::App(query.into()))
            .title(format!("{name} · Pairly"))
            .inner_size(420.0, 860.0)
            .min_inner_size(240.0, 320.0)
            .build()?;
        Ok(())
    }

    /// The viewer window is ready for video: ask the phone to share.
    pub fn watch(
        &self,
        peer: DeviceId,
        channel: Channel<InvokeResponseBody>,
    ) -> anyhow::Result<()> {
        lock(&self.viewers).insert(peer, channel);
        let plugin = self
            .plugin()
            .ok_or_else(|| anyhow::anyhow!("Pairly is off"))?;
        if let Err(e) = plugin.request(peer, ScreenAction::Start) {
            lock(&self.viewers).remove(&peer);
            return Err(e.into());
        }
        Ok(())
    }

    /// The viewer window closed: the phone can stop sharing.
    pub fn viewer_closed(&self, peer: DeviceId) {
        if lock(&self.viewers).remove(&peer).is_some()
            && let Some(plugin) = self.plugin()
        {
            let _ = plugin.request(peer, ScreenAction::Stop);
        }
    }

    pub fn viewer_input(&self, peer: DeviceId, input: ScreenInput) -> anyhow::Result<()> {
        let plugin = self
            .plugin()
            .ok_or_else(|| anyhow::anyhow!("Pairly is off"))?;
        Ok(plugin.input(peer, input)?)
    }

    pub fn viewer_started(&self, peer: DeviceId, width: u32, height: u32) {
        let mut payload = width.to_be_bytes().to_vec();
        payload.extend_from_slice(&height.to_be_bytes());
        self.to_viewer(peer, message(STARTED, &payload));
    }

    pub fn viewer_frame(&self, peer: DeviceId, frame: &ScreenFrame) {
        let flags = u8::from(frame.key) | (u8::from(frame.config) << 1);
        let mut out = Vec::with_capacity(frame.data.len() + 2);
        out.push(FRAME);
        out.push(flags);
        out.extend_from_slice(&frame.data);
        self.to_viewer(peer, InvokeResponseBody::Raw(out));
    }

    pub fn viewer_stopped(&self, peer: DeviceId, reason: &str) {
        if let Some(channel) = lock(&self.viewers).remove(&peer) {
            let _ = channel.send(message(STOPPED, reason.as_bytes()));
        }
    }

    fn to_viewer(&self, peer: DeviceId, msg: InvokeResponseBody) {
        let channel = lock(&self.viewers).get(&peer).cloned();
        if let Some(channel) = channel
            && channel.send(msg).is_err()
        {
            // The window is gone.
            self.viewer_closed(peer);
        }
    }

    // ----- this PC's screen on a phone -----------------------------------------------------

    pub fn start_cast(&self, from: &PeerInfo) -> Result<(), String> {
        if lock(&self.casts).contains_key(&from.id) {
            return Ok(());
        }
        let weak = self.backend.clone();
        let peer = from.clone();
        let on_stop = Arc::new(AtomicBool::new(false));
        let recording = capture::start(
            Box::new({
                let weak = weak.clone();
                let peer = peer.clone();
                move |event| {
                    let Some(b) = weak.upgrade() else {
                        return false;
                    };
                    let Some(plugin) = b.screens.plugin() else {
                        return false;
                    };
                    match event {
                        capture::Event::Started { width, height } => {
                            if let Err(e) = plugin.started(peer.id, width, height) {
                                tracing::warn!(error = %e, "can't start sharing the screen");
                                return false;
                            }
                            true
                        }
                        // True asks the recorder for a key frame (the link fell behind).
                        capture::Event::Frame { data, key } => {
                            let frame = ScreenFrame {
                                data,
                                key,
                                config: false,
                            };
                            plugin.frame(peer.id, &frame).unwrap_or(false)
                        }
                        capture::Event::Stopped(reason) => {
                            if !on_stop.swap(true, Ordering::SeqCst) {
                                b.screens.stop_cast(peer.id, Some(&reason));
                            }
                            false
                        }
                    }
                }
            }),
        )
        .map_err(|e| format!("can't record the screen: {e}"))?;
        let monitor = recording.monitor();
        lock(&self.casts).insert(from.id, Cast { recording, monitor });
        tracing::info!(device = %from.id, "sharing this PC's screen");
        // "Your screen is being shared", with a way to stop it.
        if let Some(b) = weak.upgrade() {
            let (weak, id) = (self.backend.clone(), from.id);
            toast::show(
                &toast::Toast {
                    title: format!("Sharing your screen with {}", from.name),
                    body: "It can see your screen and control the mouse and keyboard".into(),
                    buttons: vec![toast::Button {
                        id: "stop".into(),
                        label: "Stop Sharing".into(),
                    }],
                    silent: true,
                    tag: "cast".into(),
                    group: from.id.to_string(),
                    ..toast::Toast::default()
                },
                Some(Box::new(move |event| {
                    if let (toast::ToastEvent::Clicked(_), Some(b)) = (event, weak.upgrade()) {
                        b.screens.stop_cast(id, Some("stopped on the PC"));
                    }
                })),
            );
            b.emit();
        }
        Ok(())
    }

    /// Stop sharing this screen with a phone (telling it why, if `reason`).
    pub fn stop_cast(&self, peer: DeviceId, reason: Option<&str>) {
        let Some(cast) = lock(&self.casts).remove(&peer) else {
            return;
        };
        cast.recording.stop();
        tracing::info!(device = %peer, reason, "stopped sharing this PC's screen");
        if let (Some(reason), Some(plugin)) = (reason, self.plugin()) {
            let _ = plugin.stopped(peer, reason);
        }
        toast::remove(&peer.to_string(), "cast");
        if let Some(b) = self.backend.upgrade() {
            b.emit();
        }
    }

    /// The phone's touches and keys, as this PC's pointer and keyboard.
    pub fn cast_input(&self, from: &PeerInfo, input: ScreenInput) {
        let Some(b) = self.backend.upgrade() else {
            return;
        };
        let Some(monitor) = lock(&self.casts).get(&from.id).map(|c| c.monitor) else {
            return;
        };
        let pc = &b.input;
        let at = |x: f32, y: f32| {
            (
                monitor.0 + (x * monitor.2 as f32) as i32,
                monitor.1 + (y * monitor.3 as f32) as i32,
            )
        };
        let click = |button| {
            pc.button(PointerButton {
                button,
                action: ButtonAction::Click,
            });
        };
        match input {
            ScreenInput::Tap { x, y } => {
                pc.move_to(at(x, y));
                click(MouseButton::Left);
            }
            ScreenInput::LongPress { x, y } => {
                pc.move_to(at(x, y));
                click(MouseButton::Right);
            }
            ScreenInput::Swipe {
                points,
                duration_ms,
            } => {
                // Hold the button while moving through the points, at the swipe's pace.
                let step =
                    Duration::from_millis(u64::from(duration_ms) / points.len().max(1) as u64)
                        .min(Duration::from_millis(40));
                let points: Vec<(i32, i32)> = points.into_iter().map(|(x, y)| at(x, y)).collect();
                pc.drag(points, step);
            }
            // Two fingers: the content follows them (as on a touch screen).
            ScreenInput::Scroll { dx, dy } => {
                // About 40 pixels a wheel step.
                let px = monitor.3 as f32 / 40.0;
                pc.pointer(pairly_plugins::input::PointerMotion {
                    dx: 0.0,
                    dy: 0.0,
                    scroll_x: -dx * px,
                    scroll_y: -dy * px,
                });
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
                pc.key(KeyInput {
                    text: None,
                    key: Some(special),
                    modifiers: Modifiers::default(),
                });
            }
            ScreenInput::Text { text } => pc.key(KeyInput {
                text: Some(text),
                key: None,
                modifiers: Modifiers::default(),
            }),
        }
    }

    // ----- the laser pointer ---------------------------------------------------------------

    pub fn laser(&self, laser: LaserPointer) {
        let Some(b) = self.backend.upgrade() else {
            return;
        };
        if laser.action == LaserAction::Show {
            let mut l = lock(&self.laser);
            l.hidden_since = None;
            let exists = b.app.get_webview_window("laser").is_some();
            if !exists {
                drop(l);
                if let Err(e) = open_laser(&b.app) {
                    tracing::warn!(error = %e, "can't show the laser pointer");
                    return;
                }
                l = lock(&self.laser);
            }
            l.open = true;
        } else if laser.action == LaserAction::Hide {
            lock(&self.laser).hidden_since = Some(Instant::now());
        }
        let action = match laser.action {
            LaserAction::Show => "show",
            LaserAction::Move => "move",
            LaserAction::Hide => "hide",
        };
        let _ = b
            .app
            .emit_to("laser", "laser", (action, laser.dx, laser.dy));
    }

    fn close_laser(&self) {
        let mut l = lock(&self.laser);
        l.open = false;
        l.hidden_since = None;
        if let Some(w) = self
            .backend
            .upgrade()
            .and_then(|b| b.app.get_webview_window("laser"))
        {
            let _ = w.close();
        }
    }
}

/// The laser overlay: transparent, on top of everything (full-screen slides too), covering the
/// main screen, and letting clicks through.
fn open_laser(app: &tauri::AppHandle) -> anyhow::Result<()> {
    let monitor = app
        .primary_monitor()?
        .ok_or_else(|| anyhow::anyhow!("no screen"))?;
    let window = WebviewWindowBuilder::new(app, "laser", WebviewUrl::App("laser.html?on".into()))
        .title("Pairly pointer")
        .transparent(true)
        .decorations(false)
        .shadow(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .resizable(false)
        .focused(false)
        .visible(false)
        .build()?;
    window.set_position(*monitor.position())?;
    window.set_size(*monitor.size())?;
    window.set_ignore_cursor_events(true)?;
    window.show()?;
    Ok(())
}

fn url_escape(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}
