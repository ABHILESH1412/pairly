//! uinput backend: a virtual mouse and keyboard at the kernel level, so it works on any desktop
//! (Wayland or X11). Needs write access to `/dev/uinput` (a udev rule or the `input` group).
//!
//! Key events are key *codes*, interpreted with the desktop's layout; text is typed assuming a
//! US layout, and characters a US keyboard can't type are skipped.

use evdev::uinput::VirtualDevice;
use evdev::{AttributeSet, EventType, InputEvent, KeyCode, RelativeAxisCode};
use pairly_plugins::input::{
    ButtonAction, KeyInput, MouseButton, PointerButton, PointerMotion, SpecialKey,
};
use tracing::debug;

pub struct Uinput {
    device: VirtualDevice,
    /// Sub-pixel and sub-detent remainders.
    rest: [f32; 2],
    wheel: [i32; 2],
}

fn letter(c: char) -> Option<KeyCode> {
    use KeyCode as K;
    Some(match c.to_ascii_lowercase() {
        'a' => K::KEY_A,
        'b' => K::KEY_B,
        'c' => K::KEY_C,
        'd' => K::KEY_D,
        'e' => K::KEY_E,
        'f' => K::KEY_F,
        'g' => K::KEY_G,
        'h' => K::KEY_H,
        'i' => K::KEY_I,
        'j' => K::KEY_J,
        'k' => K::KEY_K,
        'l' => K::KEY_L,
        'm' => K::KEY_M,
        'n' => K::KEY_N,
        'o' => K::KEY_O,
        'p' => K::KEY_P,
        'q' => K::KEY_Q,
        'r' => K::KEY_R,
        's' => K::KEY_S,
        't' => K::KEY_T,
        'u' => K::KEY_U,
        'v' => K::KEY_V,
        'w' => K::KEY_W,
        'x' => K::KEY_X,
        'y' => K::KEY_Y,
        'z' => K::KEY_Z,
        _ => return None,
    })
}

/// The US-layout key (and whether Shift is needed) for a character.
fn us_key(c: char) -> Option<(KeyCode, bool)> {
    use KeyCode as K;
    if let Some(k) = letter(c) {
        return Some((k, c.is_ascii_uppercase()));
    }
    let digits = [
        K::KEY_0,
        K::KEY_1,
        K::KEY_2,
        K::KEY_3,
        K::KEY_4,
        K::KEY_5,
        K::KEY_6,
        K::KEY_7,
        K::KEY_8,
        K::KEY_9,
    ];
    if let Some(d) = c.to_digit(10) {
        return Some((digits[d as usize], false));
    }
    Some(match c {
        ' ' => (K::KEY_SPACE, false),
        '\n' | '\r' => (K::KEY_ENTER, false),
        '\t' => (K::KEY_TAB, false),
        ')' => (K::KEY_0, true),
        '!' => (K::KEY_1, true),
        '@' => (K::KEY_2, true),
        '#' => (K::KEY_3, true),
        '$' => (K::KEY_4, true),
        '%' => (K::KEY_5, true),
        '^' => (K::KEY_6, true),
        '&' => (K::KEY_7, true),
        '*' => (K::KEY_8, true),
        '(' => (K::KEY_9, true),
        '-' => (K::KEY_MINUS, false),
        '_' => (K::KEY_MINUS, true),
        '=' => (K::KEY_EQUAL, false),
        '+' => (K::KEY_EQUAL, true),
        '[' => (K::KEY_LEFTBRACE, false),
        '{' => (K::KEY_LEFTBRACE, true),
        ']' => (K::KEY_RIGHTBRACE, false),
        '}' => (K::KEY_RIGHTBRACE, true),
        '\\' => (K::KEY_BACKSLASH, false),
        '|' => (K::KEY_BACKSLASH, true),
        ';' => (K::KEY_SEMICOLON, false),
        ':' => (K::KEY_SEMICOLON, true),
        '\'' => (K::KEY_APOSTROPHE, false),
        '"' => (K::KEY_APOSTROPHE, true),
        '`' => (K::KEY_GRAVE, false),
        '~' => (K::KEY_GRAVE, true),
        ',' => (K::KEY_COMMA, false),
        '<' => (K::KEY_COMMA, true),
        '.' => (K::KEY_DOT, false),
        '>' => (K::KEY_DOT, true),
        '/' => (K::KEY_SLASH, false),
        '?' => (K::KEY_SLASH, true),
        _ => return None,
    })
}

fn special(k: SpecialKey) -> KeyCode {
    use KeyCode as K;
    match k {
        SpecialKey::Enter => K::KEY_ENTER,
        SpecialKey::Backspace => K::KEY_BACKSPACE,
        SpecialKey::Delete => K::KEY_DELETE,
        SpecialKey::Tab => K::KEY_TAB,
        SpecialKey::Escape => K::KEY_ESC,
        SpecialKey::Left => K::KEY_LEFT,
        SpecialKey::Right => K::KEY_RIGHT,
        SpecialKey::Up => K::KEY_UP,
        SpecialKey::Down => K::KEY_DOWN,
        SpecialKey::Home => K::KEY_HOME,
        SpecialKey::End => K::KEY_END,
        SpecialKey::PageUp => K::KEY_PAGEUP,
        SpecialKey::PageDown => K::KEY_PAGEDOWN,
        SpecialKey::Space => K::KEY_SPACE,
        SpecialKey::F(n) => [
            K::KEY_F1,
            K::KEY_F2,
            K::KEY_F3,
            K::KEY_F4,
            K::KEY_F5,
            K::KEY_F6,
            K::KEY_F7,
            K::KEY_F8,
            K::KEY_F9,
            K::KEY_F10,
            K::KEY_F11,
            K::KEY_F12,
        ][usize::from(n.clamp(1, 12) - 1)],
        SpecialKey::VolumeUp => K::KEY_VOLUMEUP,
        SpecialKey::VolumeDown => K::KEY_VOLUMEDOWN,
        SpecialKey::Mute => K::KEY_MUTE,
    }
}

const MODIFIERS: [KeyCode; 4] = [
    KeyCode::KEY_LEFTCTRL,
    KeyCode::KEY_LEFTALT,
    KeyCode::KEY_LEFTSHIFT,
    KeyCode::KEY_LEFTMETA,
];

/// Every key the device may send.
fn all_keys() -> AttributeSet<KeyCode> {
    let mut keys = AttributeSet::<KeyCode>::new();
    for k in [KeyCode::BTN_LEFT, KeyCode::BTN_RIGHT, KeyCode::BTN_MIDDLE] {
        keys.insert(k);
    }
    for c in (' '..='~').chain(['\n', '\t']) {
        if let Some((k, _)) = us_key(c) {
            keys.insert(k);
        }
    }
    for k in [
        SpecialKey::Backspace,
        SpecialKey::Delete,
        SpecialKey::Escape,
        SpecialKey::Left,
        SpecialKey::Right,
        SpecialKey::Up,
        SpecialKey::Down,
        SpecialKey::Home,
        SpecialKey::End,
        SpecialKey::PageUp,
        SpecialKey::PageDown,
        SpecialKey::VolumeUp,
        SpecialKey::VolumeDown,
        SpecialKey::Mute,
    ] {
        keys.insert(special(k));
    }
    for n in 1..=12 {
        keys.insert(special(SpecialKey::F(n)));
    }
    for m in MODIFIERS {
        keys.insert(m);
    }
    keys
}

pub fn open() -> Result<Uinput, String> {
    let mut axes = AttributeSet::<RelativeAxisCode>::new();
    for a in [
        RelativeAxisCode::REL_X,
        RelativeAxisCode::REL_Y,
        RelativeAxisCode::REL_WHEEL,
        RelativeAxisCode::REL_HWHEEL,
        RelativeAxisCode::REL_WHEEL_HI_RES,
        RelativeAxisCode::REL_HWHEEL_HI_RES,
    ] {
        axes.insert(a);
    }
    let device = VirtualDevice::builder()
        .map_err(|e| format!("can't open /dev/uinput ({e}); see the README for access"))?
        .name("Pairly remote input")
        .with_keys(&all_keys())
        .map_err(|e| e.to_string())?
        .with_relative_axes(&axes)
        .map_err(|e| e.to_string())?
        .build()
        .map_err(|e| format!("can't create a uinput device: {e}"))?;
    Ok(Uinput {
        device,
        rest: [0.0; 2],
        wheel: [0; 2],
    })
}

fn key_event(code: KeyCode, down: bool) -> InputEvent {
    InputEvent::new(EventType::KEY.0, code.0, i32::from(down))
}

fn rel(axis: RelativeAxisCode, value: i32) -> InputEvent {
    InputEvent::new(EventType::RELATIVE.0, axis.0, value)
}

/// High-resolution wheel units per detent.
const DETENT: i32 = 120;
/// Phone scroll pixels to high-resolution units.
const SCROLL_SCALE: f32 = 6.0;

impl super::Backend for Uinput {
    fn name(&self) -> &'static str {
        "uinput"
    }

    fn motion(&mut self, m: PointerMotion) -> Result<(), String> {
        let mut events = Vec::new();
        for (i, (delta, axis)) in [
            (m.dx, RelativeAxisCode::REL_X),
            (m.dy, RelativeAxisCode::REL_Y),
        ]
        .into_iter()
        .enumerate()
        {
            let total = self.rest[i] + delta;
            let whole = total.trunc();
            self.rest[i] = total - whole;
            if whole != 0.0 {
                events.push(rel(axis, whole as i32));
            }
        }
        // Wayland-style scroll (positive = down) to evdev wheels (positive = up / right).
        for (i, (value, hi, lo)) in [
            (
                -m.scroll_y,
                RelativeAxisCode::REL_WHEEL_HI_RES,
                RelativeAxisCode::REL_WHEEL,
            ),
            (
                m.scroll_x,
                RelativeAxisCode::REL_HWHEEL_HI_RES,
                RelativeAxisCode::REL_HWHEEL,
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let units = (value * SCROLL_SCALE) as i32;
            if units == 0 {
                continue;
            }
            events.push(rel(hi, units));
            self.wheel[i] += units;
            let detents = self.wheel[i] / DETENT;
            if detents != 0 {
                self.wheel[i] -= detents * DETENT;
                events.push(rel(lo, detents));
            }
        }
        if events.is_empty() {
            return Ok(());
        }
        self.device.emit(&events).map_err(|e| e.to_string())
    }

    fn button(&mut self, b: PointerButton) -> Result<(), String> {
        let code = match b.button {
            MouseButton::Left => KeyCode::BTN_LEFT,
            MouseButton::Right => KeyCode::BTN_RIGHT,
            MouseButton::Middle => KeyCode::BTN_MIDDLE,
        };
        let emit = |d: &mut VirtualDevice, down| {
            d.emit(&[key_event(code, down)]).map_err(|e| e.to_string())
        };
        match b.action {
            ButtonAction::Press => emit(&mut self.device, true),
            ButtonAction::Release => emit(&mut self.device, false),
            ButtonAction::Click => {
                emit(&mut self.device, true)?;
                emit(&mut self.device, false)
            }
        }
    }

    fn key(&mut self, k: &KeyInput) -> Result<(), String> {
        let m = k.modifiers;
        let held: Vec<KeyCode> = [
            (m.ctrl, KeyCode::KEY_LEFTCTRL),
            (m.alt, KeyCode::KEY_LEFTALT),
            (m.shift, KeyCode::KEY_LEFTSHIFT),
            (m.logo, KeyCode::KEY_LEFTMETA),
        ]
        .into_iter()
        .filter_map(|(on, code)| on.then_some(code))
        .collect();
        let mut taps: Vec<(KeyCode, bool)> = Vec::new();
        if let Some(s) = k.key {
            taps.push((special(s), false));
        }
        for c in k.text.as_deref().unwrap_or("").chars() {
            match us_key(c) {
                Some(t) => taps.push(t),
                None => debug!(?c, "a US keyboard can't type this character; skipped"),
            }
        }
        let mut events: Vec<InputEvent> = held.iter().map(|c| key_event(*c, true)).collect();
        self.device.emit(&events).map_err(|e| e.to_string())?;
        for (code, shift) in taps {
            events.clear();
            if shift && !m.shift {
                events.push(key_event(KeyCode::KEY_LEFTSHIFT, true));
            }
            events.push(key_event(code, true));
            events.push(key_event(code, false));
            if shift && !m.shift {
                events.push(key_event(KeyCode::KEY_LEFTSHIFT, false));
            }
            self.device.emit(&events).map_err(|e| e.to_string())?;
        }
        let release: Vec<InputEvent> = held.iter().rev().map(|c| key_event(*c, false)).collect();
        self.device.emit(&release).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn us_layout() {
        assert_eq!(us_key('a'), Some((KeyCode::KEY_A, false)));
        assert_eq!(us_key('A'), Some((KeyCode::KEY_A, true)));
        assert_eq!(us_key('?'), Some((KeyCode::KEY_SLASH, true)));
        assert_eq!(us_key('7'), Some((KeyCode::KEY_7, false)));
        assert_eq!(us_key('é'), None);
        assert!(all_keys().contains(KeyCode::KEY_F12));
    }
}
