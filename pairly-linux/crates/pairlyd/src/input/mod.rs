//! Remote input from a paired phone (touchpad, keyboard, presenter), through whichever backend
//! this desktop supports, tried in order:
//!
//! 1. [`wayland`]: virtual pointer/keyboard protocols (Hyprland, Sway, river). Silent.
//! 2. [`portal`]: the XDG RemoteDesktop portal (GNOME, KDE). Asks once, then remembers.
//! 3. [`uinput`]: a kernel virtual device (any desktop, X11 too). Needs write access to
//!    `/dev/uinput`; types with a US layout.
//!
//! `[input] backend` in the config forces one.

mod portal;
mod uinput;
mod wayland;

use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Mutex, PoisonError};

use pairly_core::PeerInfo;
use pairly_plugins::input::{InputHost, KeyInput, PointerButton, PointerMotion};
use serde::Deserialize;
use tracing::{debug, info, warn};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackendChoice {
    #[default]
    Auto,
    Wayland,
    Portal,
    Uinput,
}

trait Backend {
    fn name(&self) -> &'static str;
    fn motion(&mut self, m: PointerMotion) -> Result<(), String>;
    fn button(&mut self, b: PointerButton) -> Result<(), String>;
    fn key(&mut self, k: &KeyInput) -> Result<(), String>;
    /// Put the pointer at `fx`, `fy` (fractions) of the monitor at `monitor` in the desktop.
    fn place(&mut self, _monitor: (i32, i32), _fx: f32, _fy: f32) -> Result<(), String> {
        Err("this input method can't place the pointer at a spot".into())
    }
}

enum Cmd {
    Pointer(PointerMotion),
    Button(PointerButton),
    Key(KeyInput),
    Place((i32, i32), f32, f32),
}

/// The plugin's handle: commands go to a thread that owns the backend.
pub struct LinuxInput {
    choice: BackendChoice,
    /// Where the portal backend keeps its "remember this permission" token.
    data_dir: PathBuf,
    tx: Mutex<Option<mpsc::Sender<Cmd>>>,
    laser: std::sync::Arc<crate::laser::Laser>,
}

impl LinuxInput {
    pub fn new(choice: BackendChoice, data_dir: PathBuf) -> Self {
        Self {
            choice,
            data_dir,
            tx: Mutex::new(None),
            laser: crate::laser::Laser::new(),
        }
    }

    /// Put the pointer at a spot on a monitor (screen sharing: a tap on the phone).
    pub fn place(&self, monitor: (i32, i32), fx: f32, fy: f32) {
        self.send(Cmd::Place(monitor, fx, fy));
    }

    pub fn press(&self, button: PointerButton) {
        self.send(Cmd::Button(button));
    }

    pub fn scroll(&self, scroll_x: f32, scroll_y: f32) {
        self.send(Cmd::Pointer(PointerMotion {
            dx: 0.0,
            dy: 0.0,
            scroll_x,
            scroll_y,
        }));
    }

    fn send(&self, cmd: Cmd) {
        let mut tx = self.tx.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(sender) = tx.as_ref()
            && sender.send(cmd).is_ok()
        {
            return;
        }
        // Not running yet, or the backend died: (re)start it. The command that triggered this
        // is dropped; input is continuous, so the next one goes through.
        let (sender, rx) = mpsc::channel();
        let (choice, dir) = (self.choice, self.data_dir.clone());
        if std::thread::Builder::new()
            .name("pairly-input".into())
            .spawn(move || run(choice, &dir, &rx))
            .is_ok()
        {
            *tx = Some(sender);
        }
    }
}

impl InputHost for LinuxInput {
    fn pointer(&self, _from: &PeerInfo, motion: PointerMotion) {
        self.send(Cmd::Pointer(motion));
    }
    fn button(&self, _from: &PeerInfo, button: PointerButton) {
        self.send(Cmd::Button(button));
    }
    fn key(&self, _from: &PeerInfo, key: &KeyInput) {
        self.send(Cmd::Key(key.clone()));
    }
    fn laser(&self, _from: &PeerInfo, laser: pairly_plugins::input::LaserPointer) {
        self.laser.handle(laser);
    }
}

fn open(choice: BackendChoice, data_dir: &std::path::Path) -> Result<Box<dyn Backend>, String> {
    let try_wayland = || wayland::open().map(|b| Box::new(b) as Box<dyn Backend>);
    let try_portal = || portal::open(data_dir).map(|b| Box::new(b) as Box<dyn Backend>);
    let try_uinput = || uinput::open().map(|b| Box::new(b) as Box<dyn Backend>);
    match choice {
        BackendChoice::Wayland => try_wayland(),
        BackendChoice::Portal => try_portal(),
        BackendChoice::Uinput => try_uinput(),
        BackendChoice::Auto => {
            let mut why = Vec::new();
            for attempt in [&try_wayland as &dyn Fn() -> _, &try_portal, &try_uinput] {
                match attempt() {
                    Ok(b) => return Ok(b),
                    Err(e) => why.push(e),
                }
            }
            Err(why.join("; "))
        }
    }
}

fn run(choice: BackendChoice, data_dir: &std::path::Path, rx: &mpsc::Receiver<Cmd>) {
    let mut backend = match open(choice, data_dir) {
        Ok(b) => {
            info!(backend = b.name(), "remote input ready");
            b
        }
        Err(e) => {
            warn!(error = %e, "remote input unavailable");
            return;
        }
    };
    while let Ok(cmd) = rx.recv() {
        let result = match cmd {
            Cmd::Pointer(m) => backend.motion(m),
            Cmd::Button(b) => backend.button(b),
            Cmd::Key(k) => backend.key(&k),
            // A backend that can't place the pointer just ignores it.
            Cmd::Place(monitor, fx, fy) => backend.place(monitor, fx, fy).or_else(|e| {
                debug!(error = %e, "pointer placement unavailable");
                Ok(())
            }),
        };
        if let Err(e) = result {
            warn!(error = %e, "remote input failed; reconnecting on the next event");
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Touches real devices: `cargo test -p pairlyd -- --ignored input_backends_open`.
    #[test]
    #[ignore = "needs /dev/uinput and a session bus"]
    fn input_backends_open() {
        let dir = std::env::temp_dir();
        match uinput::open() {
            Ok(mut u) => {
                assert_eq!(u.name(), "uinput");
                u.motion(PointerMotion {
                    dx: 0.0,
                    dy: 0.0,
                    scroll_x: 0.0,
                    scroll_y: 0.0,
                })
                .unwrap_or_default();
                println!("uinput: ok");
            }
            Err(e) => println!("uinput: {e}"),
        }
        match portal::open(&dir) {
            Ok(p) => println!("portal: {}", p.name()),
            Err(e) => println!("portal: {e}"),
        }
    }
}
