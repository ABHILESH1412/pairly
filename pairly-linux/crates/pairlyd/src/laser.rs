//! The presentation laser pointer: a phone holds its "Pointer" button and a red dot follows its
//! movements on this PC's screen. The dot is drawn by `pairly-gtk --laser` (a click-through
//! overlay); this starts it on the first `show`, feeds it one command per line, and lets it
//! quit after a minute without pointing, so it holds no memory while unused.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant};

use pairly_plugins::input::{LaserAction, LaserPointer};
use tracing::{info, warn};

/// Quit the overlay this long after the pointer was last hidden.
const IDLE_QUIT: Duration = Duration::from_secs(60);
/// The overlay's exit code when the desktop has no overlay layer (see `pairly-gtk`).
const UNSUPPORTED: i32 = 2;

#[derive(Default)]
struct State {
    overlay: Option<(Child, ChildStdin)>,
    hidden_since: Option<Instant>,
    /// This desktop can't show it (e.g. GNOME): don't keep trying.
    unsupported: bool,
}

pub struct Laser {
    state: Mutex<State>,
}

impl Laser {
    pub fn new() -> Arc<Self> {
        let laser = Arc::new(Self {
            state: Mutex::default(),
        });
        let weak = Arc::downgrade(&laser);
        std::thread::Builder::new()
            .name("pairly-laser".into())
            .spawn(move || idle_quit(&weak))
            .ok();
        laser
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn handle(&self, laser: LaserPointer) {
        let mut st = self.lock();
        if st.unsupported {
            return;
        }
        let line = match laser.action {
            LaserAction::Show => {
                st.hidden_since = None;
                if !running(&mut st) {
                    match spawn() {
                        Ok(overlay) => {
                            info!("laser pointer started");
                            st.overlay = Some(overlay);
                        }
                        Err(e) => {
                            warn!(error = %e, "can't start the laser pointer overlay");
                            return;
                        }
                    }
                }
                "show".to_owned()
            }
            LaserAction::Move => format!("move {:.5} {:.5}", laser.dx, laser.dy),
            LaserAction::Hide => {
                st.hidden_since = Some(Instant::now());
                "hide".to_owned()
            }
        };
        if let Some((_, stdin)) = st.overlay.as_mut()
            && writeln!(stdin, "{line}")
                .and_then(|()| stdin.flush())
                .is_err()
        {
            running(&mut st);
        }
    }
}

/// Whether the overlay is still alive; forgets it (and notes an unsupported desktop) if not.
fn running(st: &mut State) -> bool {
    let Some((child, _)) = st.overlay.as_mut() else {
        return false;
    };
    match child.try_wait() {
        Ok(None) => true,
        Ok(Some(status)) => {
            if status.code() == Some(UNSUPPORTED) {
                warn!("this desktop can't draw the laser pointer (it needs wlr-layer-shell)");
                st.unsupported = true;
            }
            st.overlay = None;
            false
        }
        Err(_) => {
            st.overlay = None;
            false
        }
    }
}

fn spawn() -> std::io::Result<(Child, ChildStdin)> {
    let mut child = Command::new(gtk_binary())
        .arg("--laser")
        .stdin(Stdio::piped())
        .spawn()?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| std::io::Error::other("no stdin"))?;
    Ok((child, stdin))
}

/// `pairly-gtk` next to this binary (an install keeps them together), else from PATH.
pub fn gtk_binary() -> PathBuf {
    std::env::current_exe()
        .ok()
        .map(|exe| exe.with_file_name("pairly-gtk"))
        .filter(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from("pairly-gtk"))
}

fn idle_quit(laser: &Weak<Laser>) {
    loop {
        std::thread::sleep(Duration::from_secs(15));
        let Some(laser) = laser.upgrade() else { return };
        let mut st = laser.lock();
        let idle = st.hidden_since.is_some_and(|t| t.elapsed() >= IDLE_QUIT);
        if idle && let Some((mut child, mut stdin)) = st.overlay.take() {
            let _ = writeln!(stdin, "quit");
            drop(stdin);
            let _ = child.wait();
            st.hidden_since = None;
            info!("laser pointer closed (unused)");
        }
    }
}
