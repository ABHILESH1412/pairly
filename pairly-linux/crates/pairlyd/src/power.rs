//! A paired phone asks to lock this PC's screen, or to power it off or restart it.
//!
//! Locking goes through logind (`loginctl lock-session`), which the desktop's locker answers
//! (hypridle, GNOME, KDE). Power off and restart use `systemctl`, allowed by polkit for the
//! user of the active session. `[power] from_phone = false` refuses all three.

use std::process::Command;
use std::time::Duration;

use pairly_core::PeerInfo;
use pairly_plugins::power::{PowerAction, PowerHost};
use tracing::{info, warn};

/// Time for the answer to reach the phone before the network goes down.
const SHUTDOWN_DELAY: Duration = Duration::from_secs(1);

pub struct LinuxPower {
    pub allowed: bool,
}

impl PowerHost for LinuxPower {
    fn act(&self, from: &PeerInfo, action: PowerAction) -> Result<(), String> {
        if !self.allowed {
            return Err("this PC doesn't allow it ([power] from_phone = false)".into());
        }
        info!(device = %from.id, ?action, "power action from a phone");
        match action {
            PowerAction::Lock => lock(),
            PowerAction::PowerOff => later(&["poweroff"]),
            PowerAction::Restart => later(&["reboot"]),
        }
    }
}

/// Lock the user's graphical session. Our service runs outside any session, so name it.
fn lock() -> Result<(), String> {
    let uid = own_uid();
    let display = Command::new("loginctl")
        .args(["show-user", &uid, "-p", "Display", "--value"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .filter(|s| !s.is_empty());
    let mut cmd = Command::new("loginctl");
    cmd.arg("lock-session");
    if let Some(session) = &display {
        cmd.arg(session);
    }
    run(&mut cmd)
}

/// `systemctl <args>` after a moment, so the phone hears that it's happening.
fn later(args: &'static [&'static str]) -> Result<(), String> {
    std::thread::spawn(move || {
        std::thread::sleep(SHUTDOWN_DELAY);
        if let Err(e) = run(Command::new("systemctl").args(args)) {
            warn!(error = %e, ?args, "power action failed");
        }
    });
    Ok(())
}

fn run(cmd: &mut Command) -> Result<(), String> {
    let out = cmd.output().map_err(|e| format!("couldn't run it: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_owned())
    }
}

/// Our uid, from /proc (no libc needed for one call).
fn own_uid() -> String {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("Uid:"))
                .and_then(|v| v.split_whitespace().next().map(str::to_owned))
        })
        .unwrap_or_default()
}
