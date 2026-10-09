//! The PC itself: battery, power, ringing, the clipboard and remote input. Windows is the
//! target; the other branches keep the app building and testable on a Linux machine.

pub mod clipboard;
pub mod input;

use pairly_plugins::battery::BatteryState;

/// This PC's battery, if it has one.
pub fn battery() -> Option<BatteryState> {
    #[cfg(windows)]
    {
        use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
        let mut status = SYSTEM_POWER_STATUS::default();
        // SAFETY: a plain out-parameter call.
        unsafe { GetSystemPowerStatus(&mut status) }.ok()?;
        // 128: no battery; 255: unknown.
        if status.BatteryFlag & 128 != 0 || status.BatteryLifePercent > 100 {
            return None;
        }
        Some(BatteryState {
            percent: status.BatteryLifePercent,
            charging: status.ACLineStatus == 1,
        })
    }
    #[cfg(not(windows))]
    {
        let dir = std::fs::read_dir("/sys/class/power_supply").ok()?;
        for entry in dir.flatten() {
            let path = entry.path();
            if std::fs::read_to_string(path.join("type")).ok()?.trim() != "Battery" {
                continue;
            }
            let percent = std::fs::read_to_string(path.join("capacity"))
                .ok()?
                .trim()
                .parse()
                .ok()?;
            let status = std::fs::read_to_string(path.join("status")).unwrap_or_default();
            return Some(BatteryState {
                percent,
                charging: matches!(status.trim(), "Charging" | "Full"),
            });
        }
        None
    }
}

/// Lock the screen, power off or restart (asked by a paired phone).
pub fn power(action: pairly_plugins::power::PowerAction) -> Result<(), String> {
    #[cfg(windows)]
    {
        use pairly_plugins::power::PowerAction;
        match action {
            PowerAction::Lock => {
                // SAFETY: no arguments; fails only without an interactive desktop.
                unsafe { windows::Win32::System::Shutdown::LockWorkStation() }
                    .map_err(|e| format!("couldn't lock: {e}"))
            }
            PowerAction::PowerOff | PowerAction::Restart => {
                let flag = if action == PowerAction::PowerOff { "/s" } else { "/r" };
                std::process::Command::new("shutdown")
                    .args([flag, "/t", "0"])
                    .status()
                    .map_err(|e| e.to_string())
                    .and_then(|s| {
                        s.success()
                            .then_some(())
                            .ok_or_else(|| "Windows refused".to_owned())
                    })
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = action;
        Err("power control is only built for Windows".into())
    }
}

/// One short alert sound (the phone is looking for this PC).
pub fn beep() {
    #[cfg(windows)]
    {
        use windows::Win32::System::Diagnostics::Debug::MessageBeep;
        use windows::Win32::UI::WindowsAndMessaging::MB_ICONEXCLAMATION;
        // SAFETY: plays a system sound; nothing to clean up.
        let _ = unsafe { MessageBeep(MB_ICONEXCLAMATION) };
    }
}

/// Open a link or a folder with the default program.
pub fn open(target: &str) {
    #[cfg(windows)]
    let result = std::process::Command::new("explorer").arg(target).spawn();
    #[cfg(not(windows))]
    let result = std::process::Command::new("xdg-open").arg(target).spawn();
    if let Err(e) = result {
        tracing::warn!(error = %e, "couldn't open {target}");
    }
}
