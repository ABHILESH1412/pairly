//! Control mode in a screen viewer. Windows keeps some keys for itself before any app sees
//! them: the Windows key and its shortcuts, Alt+Tab, Alt+Esc, Alt+F4, Ctrl+Esc. While Control
//! mode is on and the viewer is the window in front, a low-level keyboard hook takes those and
//! sends them to the other PC instead; every other key still reaches the viewer as usual.
//! Ctrl+Alt+Delete and Windows+L stay with Windows: no app can take them.

use std::sync::{Mutex, PoisonError};

use pairly_plugins::input::{Modifiers, SpecialKey};
use pairly_plugins::screen::ScreenInput;

/// Where Windows' own shortcuts go while Control mode is on.
pub type Sink = Box<dyn Fn(ScreenInput) + Send + Sync>;

struct Target {
    /// The viewer's window (`HWND` as a number): only while it's in front.
    window: isize,
    send: Sink,
}

static TARGET: Mutex<Option<Target>> = Mutex::new(None);

/// Send Windows' own shortcuts to `send` while `window` is in front; `None` turns that off.
pub fn control(target: Option<(isize, Sink)>) {
    let on = target.is_some();
    *TARGET.lock().unwrap_or_else(PoisonError::into_inner) =
        target.map(|(window, send)| Target { window, send });
    #[cfg(windows)]
    if on {
        imp::ensure_hook();
    }
    #[cfg(not(windows))]
    let _ = on;
}

/// Control mode is on for `window`.
pub fn is_on_for(window: isize) -> bool {
    TARGET
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .is_some_and(|t| t.window == window)
}

/// A key (by Windows virtual-key code) as a special key or the text it stands for.
fn key_of(vk: u32) -> Option<(Option<SpecialKey>, Option<String>)> {
    let special = |k| Some((Some(k), None));
    let text = |t: &str| Some((None, Some(t.to_owned())));
    match vk {
        0x41..=0x5A => text(&char::from(u8::try_from(vk).ok()? + 32).to_string()),
        0x30..=0x39 => text(&char::from(u8::try_from(vk).ok()?).to_string()),
        0x08 => special(SpecialKey::Backspace),
        0x09 => special(SpecialKey::Tab),
        0x0D => special(SpecialKey::Enter),
        0x1B => special(SpecialKey::Escape),
        0x20 => special(SpecialKey::Space),
        0x21 => special(SpecialKey::PageUp),
        0x22 => special(SpecialKey::PageDown),
        0x23 => special(SpecialKey::End),
        0x24 => special(SpecialKey::Home),
        0x25 => special(SpecialKey::Left),
        0x26 => special(SpecialKey::Up),
        0x27 => special(SpecialKey::Right),
        0x28 => special(SpecialKey::Down),
        0x2E => special(SpecialKey::Delete),
        0x70..=0x87 => special(SpecialKey::F(u8::try_from(vk - 0x6F).ok()?)),
        0xBA => text(";"),
        0xBB => text("="),
        0xBC => text(","),
        0xBD => text("-"),
        0xBE => text("."),
        0xBF => text("/"),
        0xC0 => text("`"),
        0xDB => text("["),
        0xDC => text("\\"),
        0xDD => text("]"),
        0xDE => text("'"),
        _ => None,
    }
}

/// Whether Windows would keep this key for itself (with these modifiers held).
fn windows_keeps(vk: u32, m: Modifiers) -> bool {
    const TAB: u32 = 0x09;
    const ESCAPE: u32 = 0x1B;
    const SPACE: u32 = 0x20;
    const F4: u32 = 0x73;
    m.logo || (m.alt && matches!(vk, TAB | ESCAPE | SPACE | F4)) || (m.ctrl && vk == ESCAPE)
}

fn combo(vk: u32, modifiers: Modifiers) -> Option<ScreenInput> {
    let (key, text) = key_of(vk)?;
    Some(ScreenInput::Combo {
        key,
        text,
        modifiers,
    })
}

#[cfg(windows)]
mod imp {
    use std::sync::Once;
    use std::sync::atomic::{AtomicBool, Ordering};

    use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, VK_CONTROL, VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_MENU,
        VK_RCONTROL, VK_RMENU, VK_RSHIFT, VK_RWIN, VK_SHIFT,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, GetForegroundWindow, GetMessageW, HC_ACTION, KBDLLHOOKSTRUCT, MSG,
        SetWindowsHookExW, WH_KEYBOARD_LL, WM_KEYDOWN, WM_SYSKEYDOWN,
    };

    use super::{PoisonError, TARGET, combo, windows_keeps};
    use pairly_plugins::input::Modifiers;
    use pairly_plugins::screen::ScreenInput;

    static HOOK: Once = Once::new();
    /// The Windows key is down (we take it, so Windows' own state never says so).
    static WIN: AtomicBool = AtomicBool::new(false);
    /// ...and something was pressed with it.
    static WIN_USED: AtomicBool = AtomicBool::new(false);

    /// The hook lives on its own thread, which must keep reading messages for it to run.
    pub fn ensure_hook() {
        HOOK.call_once(|| {
            let started = std::thread::Builder::new()
                .name("pairly-keyhook".into())
                .spawn(|| {
                    // SAFETY: a hook procedure with the right signature, on this thread.
                    let hook = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook), None, 0) };
                    if let Err(e) = hook {
                        tracing::warn!(error = %e, "can't take Windows' shortcuts for Control mode");
                        return;
                    }
                    let mut msg = MSG::default();
                    // SAFETY: a plain message loop.
                    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {}
                });
            if let Err(e) = started {
                tracing::warn!(error = %e, "can't start the Control mode thread");
            }
        });
    }

    fn held(vk: u16) -> bool {
        // SAFETY: a plain query.
        (unsafe { GetAsyncKeyState(i32::from(vk)) }) < 0
    }

    unsafe extern "system" fn hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code == HC_ACTION as i32 {
            // SAFETY: for keyboard hooks, lparam points at this.
            let info = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
            let down = matches!(wparam.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
            if take(info.vkCode, down) {
                return LRESULT(1);
            }
        }
        // SAFETY: passing the event on, as every hook must.
        unsafe { CallNextHookEx(None, code, wparam, lparam) }
    }

    /// Whether this key event goes to the other PC (and not to Windows).
    fn take(vk: u32, down: bool) -> bool {
        let target = TARGET.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(target) = target.as_ref() else {
            return false;
        };
        // SAFETY: a plain query.
        let front = unsafe { GetForegroundWindow() }.0 as isize;
        if front != target.window {
            return false;
        }
        let is = |k: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY| vk == u32::from(k.0);
        if is(VK_LWIN) || is(VK_RWIN) {
            if down {
                if !WIN.swap(true, Ordering::Relaxed) {
                    WIN_USED.store(false, Ordering::Relaxed);
                }
            } else {
                WIN.store(false, Ordering::Relaxed);
                // Pressed and let go alone: the Start menu (or overview) there.
                if !WIN_USED.swap(false, Ordering::Relaxed) {
                    (target.send)(ScreenInput::Combo {
                        key: None,
                        text: None,
                        modifiers: Modifiers {
                            logo: true,
                            ..Modifiers::default()
                        },
                    });
                }
            }
            return true;
        }
        // Ctrl, Alt and Shift themselves pass: the viewer reads them as usual.
        let modifier = [
            VK_CONTROL,
            VK_LCONTROL,
            VK_RCONTROL,
            VK_MENU,
            VK_LMENU,
            VK_RMENU,
            VK_SHIFT,
            VK_LSHIFT,
            VK_RSHIFT,
        ]
        .into_iter()
        .any(is);
        if modifier {
            return false;
        }
        let modifiers = Modifiers {
            ctrl: held(VK_CONTROL.0),
            alt: held(VK_MENU.0),
            shift: held(VK_SHIFT.0),
            logo: WIN.load(Ordering::Relaxed),
        };
        if !windows_keeps(vk, modifiers) {
            return false;
        }
        if down && let Some(input) = combo(vk, modifiers) {
            WIN_USED.store(true, Ordering::Relaxed);
            (target.send)(input);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_windows_keeps() {
        let none = Modifiers::default();
        let with = |f: fn(&mut Modifiers)| {
            let mut m = Modifiers::default();
            f(&mut m);
            m
        };
        // Alt+Tab, Ctrl+Esc, anything with the Windows key: taken.
        assert!(windows_keeps(0x09, with(|m| m.alt = true)));
        assert!(windows_keeps(0x1B, with(|m| m.ctrl = true)));
        assert!(windows_keeps(0x45, with(|m| m.logo = true)));
        // Plain typing and ordinary shortcuts: left to the viewer.
        assert!(!windows_keeps(0x45, none));
        assert!(!windows_keeps(0x43, with(|m| m.ctrl = true)));
        // Windows+E as a shortcut for the other PC.
        assert_eq!(
            combo(0x45, with(|m| m.logo = true)),
            Some(ScreenInput::Combo {
                key: None,
                text: Some("e".into()),
                modifiers: with(|m| m.logo = true),
            })
        );
        assert_eq!(key_of(0x74), Some((Some(SpecialKey::F(5)), None)));
    }
}
