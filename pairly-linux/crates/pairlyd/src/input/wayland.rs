//! Wayland backend: the virtual pointer and virtual keyboard protocols, offered by wlroots-based
//! compositors (Hyprland, Sway, river, …). No root, no portal prompt.
//!
//! The keyboard has no fixed layout: each character the phone types gets its own key in a
//! keymap built on the fly, so any text (accents, emoji, other scripts) types correctly
//! whatever the PC's layout is.

use std::collections::HashMap;
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Instant;

use pairly_plugins::input::{
    ButtonAction, KeyInput, Modifiers, MouseButton, PointerButton, PointerMotion, SpecialKey,
};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_keyboard, wl_pointer, wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, QueueHandle, delegate_noop};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};

struct State;

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(State: ignore wl_seat::WlSeat);
delegate_noop!(State: ZwlrVirtualPointerManagerV1);
delegate_noop!(State: ZwlrVirtualPointerV1);
delegate_noop!(State: ZwpVirtualKeyboardManagerV1);
delegate_noop!(State: ZwpVirtualKeyboardV1);

fn connect() -> Option<Connection> {
    let display = crate::clipboard::wayland_display()?;
    let path = if display.starts_with('/') {
        PathBuf::from(display)
    } else {
        PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR")?).join(display)
    };
    Connection::from_socket(UnixStream::connect(path).ok()?).ok()
}

pub struct Devices {
    conn: Connection,
    pointer: ZwlrVirtualPointerV1,
    keyboard: ZwpVirtualKeyboardV1,
    keymap: Keymap,
    start: Instant,
}

pub fn open() -> Result<Devices, String> {
    let conn = connect().ok_or("no Wayland display")?;
    let (globals, mut queue) =
        registry_queue_init::<State>(&conn).map_err(|e| format!("Wayland registry: {e}"))?;
    let qh = queue.handle();
    let seat: wl_seat::WlSeat = globals
        .bind(&qh, 1..=1, ())
        .map_err(|_| "the compositor has no seat")?;
    let pointers: ZwlrVirtualPointerManagerV1 = globals.bind(&qh, 1..=2, ()).map_err(|_| {
        "the compositor doesn't offer virtual pointers (wlroots-based ones like Hyprland and Sway do)"
    })?;
    let keyboards: ZwpVirtualKeyboardManagerV1 = globals
        .bind(&qh, 1..=1, ())
        .map_err(|_| "the compositor doesn't offer virtual keyboards")?;
    let pointer = pointers.create_virtual_pointer(Some(&seat), &qh, ());
    let keyboard = keyboards.create_virtual_keyboard(&seat, &qh, ());
    let mut devices = Devices {
        conn,
        pointer,
        keyboard,
        keymap: Keymap::default(),
        start: Instant::now(),
    };
    devices.upload_keymap()?;
    queue
        .roundtrip(&mut State)
        .map_err(|e| format!("Wayland: {e}"))?;
    Ok(devices)
}

// Linux evdev button codes.
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;

// Real modifier bits in the "complete" compatibility map.
const MOD_SHIFT: u32 = 1;
const MOD_CONTROL: u32 = 1 << 2;
const MOD_ALT: u32 = 1 << 3;
const MOD_SUPER: u32 = 1 << 6;

impl Devices {
    fn time(&self) -> u32 {
        u32::try_from(self.start.elapsed().as_millis() % u128::from(u32::MAX)).unwrap_or(0)
    }

    fn motion_inner(&self, m: PointerMotion) {
        let t = self.time();
        if m.dx != 0.0 || m.dy != 0.0 {
            self.pointer.motion(t, f64::from(m.dx), f64::from(m.dy));
        }
        if m.scroll_x != 0.0 || m.scroll_y != 0.0 {
            self.pointer.axis_source(wl_pointer::AxisSource::Finger);
            if m.scroll_y != 0.0 {
                self.pointer
                    .axis(t, wl_pointer::Axis::VerticalScroll, f64::from(m.scroll_y));
            }
            if m.scroll_x != 0.0 {
                self.pointer
                    .axis(t, wl_pointer::Axis::HorizontalScroll, f64::from(m.scroll_x));
            }
        }
        self.pointer.frame();
    }

    fn button_inner(&self, b: PointerButton) {
        let code = match b.button {
            MouseButton::Left => BTN_LEFT,
            MouseButton::Right => BTN_RIGHT,
            MouseButton::Middle => BTN_MIDDLE,
        };
        let press = |state| {
            self.pointer.button(self.time(), code, state);
            self.pointer.frame();
        };
        match b.action {
            ButtonAction::Press => press(wl_pointer::ButtonState::Pressed),
            ButtonAction::Release => press(wl_pointer::ButtonState::Released),
            ButtonAction::Click => {
                press(wl_pointer::ButtonState::Pressed);
                press(wl_pointer::ButtonState::Released);
            }
        }
    }

    fn upload_keymap(&mut self) -> Result<(), String> {
        let text = self.keymap.render();
        let dir = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").ok_or("no XDG_RUNTIME_DIR")?);
        let path = dir.join(format!("pairly-keymap-{}", std::process::id()));
        let file = std::fs::File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .map_err(|e| e.to_string())?;
        // The compositor maps the file through the fd; the name isn't needed.
        let _ = std::fs::remove_file(&path);
        use std::io::Write;
        (&file)
            .write_all(text.as_bytes())
            .map_err(|e| e.to_string())?;
        (&file).write_all(&[0]).map_err(|e| e.to_string())?;
        let size = u32::try_from(text.len() + 1).map_err(|e| e.to_string())?;
        self.keyboard
            .keymap(wl_keyboard::KeymapFormat::XkbV1.into(), file.as_fd(), size);
        Ok(())
    }

    fn tap(&self, code: u32) {
        let t = self.time();
        self.keyboard.key(t, code, 1);
        self.keyboard.key(t, code, 0);
    }

    fn key_inner(&mut self, k: &KeyInput) -> Result<(), String> {
        // Make sure every key we need exists, then send them.
        let mut codes = Vec::new();
        if let Some(special) = k.key {
            codes.push(self.code(&special_keysym(special))?);
        }
        for c in k.text.as_deref().unwrap_or("").chars() {
            let name = match c {
                '\n' | '\r' => "Return".to_owned(),
                '\t' => "Tab".to_owned(),
                c if c.is_control() => continue,
                c => format!("U{:04X}", u32::from(c)),
            };
            codes.push(self.code(&name)?);
        }
        let held = self.hold(k.modifiers)?;
        for code in codes {
            self.tap(code);
        }
        self.release(&held);
        Ok(())
    }

    /// The key for a keysym, adding it to the keymap (and re-uploading) if new.
    fn code(&mut self, keysym: &str) -> Result<u32, String> {
        if let Some(code) = self.keymap.find(keysym) {
            return Ok(code);
        }
        let code = self.keymap.add(keysym);
        self.upload_keymap()?;
        Ok(code)
    }

    fn hold(&mut self, m: Modifiers) -> Result<Vec<u32>, String> {
        let mut mask = 0;
        let mut held = Vec::new();
        for (on, sym, bit) in [
            (m.shift, "Shift_L", MOD_SHIFT),
            (m.ctrl, "Control_L", MOD_CONTROL),
            (m.alt, "Alt_L", MOD_ALT),
            (m.logo, "Super_L", MOD_SUPER),
        ] {
            if on {
                let code = self.code(sym)?;
                self.keyboard.key(self.time(), code, 1);
                held.push(code);
                mask |= bit;
            }
        }
        if mask != 0 {
            self.keyboard.modifiers(mask, 0, 0, 0);
        }
        Ok(held)
    }

    fn release(&self, held: &[u32]) {
        for code in held.iter().rev() {
            self.keyboard.key(self.time(), *code, 0);
        }
        if !held.is_empty() {
            self.keyboard.modifiers(0, 0, 0, 0);
        }
    }
}

impl super::Backend for Devices {
    fn name(&self) -> &'static str {
        "Wayland virtual input"
    }

    fn motion(&mut self, m: PointerMotion) -> Result<(), String> {
        self.motion_inner(m);
        self.conn.flush().map_err(|e| e.to_string())
    }

    fn button(&mut self, b: PointerButton) -> Result<(), String> {
        self.button_inner(b);
        self.conn.flush().map_err(|e| e.to_string())
    }

    fn key(&mut self, k: &KeyInput) -> Result<(), String> {
        self.key_inner(k)?;
        self.conn.flush().map_err(|e| e.to_string())
    }
}

pub(super) fn special_keysym(k: SpecialKey) -> String {
    match k {
        SpecialKey::Enter => "Return",
        SpecialKey::Backspace => "BackSpace",
        SpecialKey::Delete => "Delete",
        SpecialKey::Tab => "Tab",
        SpecialKey::Escape => "Escape",
        SpecialKey::Left => "Left",
        SpecialKey::Right => "Right",
        SpecialKey::Up => "Up",
        SpecialKey::Down => "Down",
        SpecialKey::Home => "Home",
        SpecialKey::End => "End",
        SpecialKey::PageUp => "Prior",
        SpecialKey::PageDown => "Next",
        SpecialKey::Space => "space",
        SpecialKey::F(n) => return format!("F{}", n.clamp(1, 24)),
        SpecialKey::VolumeUp => "XF86AudioRaiseVolume",
        SpecialKey::VolumeDown => "XF86AudioLowerVolume",
        SpecialKey::Mute => "XF86AudioMute",
    }
    .to_owned()
}

/// Keysyms by key: evdev code `i + 1` carries `keysyms[i]` (xkb keycode `i + 9`).
#[derive(Default)]
struct Keymap {
    keysyms: Vec<String>,
    index: HashMap<String, u32>,
}

/// xkb keycodes stop at 255.
const MAX_KEYS: usize = 247;

impl Keymap {
    fn find(&self, keysym: &str) -> Option<u32> {
        self.index.get(keysym).copied()
    }

    fn add(&mut self, keysym: &str) -> u32 {
        if self.keysyms.len() >= MAX_KEYS {
            // Full (lots of distinct characters): start over.
            self.keysyms.clear();
            self.index.clear();
        }
        self.keysyms.push(keysym.to_owned());
        let code = u32::try_from(self.keysyms.len()).unwrap_or(1);
        self.index.insert(keysym.to_owned(), code);
        code
    }

    fn render(&self) -> String {
        let mut keycodes = String::new();
        let mut symbols = String::new();
        let mut mods: Vec<(&str, usize)> = Vec::new();
        for (i, sym) in self.keysyms.iter().enumerate() {
            keycodes.push_str(&format!("    <K{i}> = {};\n", i + 9));
            symbols.push_str(&format!("    key <K{i}> {{ [ {sym} ] }};\n"));
            let modifier = match sym.as_str() {
                "Shift_L" => Some("Shift"),
                "Control_L" => Some("Control"),
                "Alt_L" => Some("Mod1"),
                "Super_L" => Some("Mod4"),
                _ => None,
            };
            if let Some(m) = modifier {
                mods.push((m, i));
            }
        }
        for (m, i) in mods {
            symbols.push_str(&format!("    modifier_map {m} {{ <K{i}> }};\n"));
        }
        if self.keysyms.is_empty() {
            // An empty keymap is invalid; give it one harmless key.
            keycodes.push_str("    <K0> = 9;\n");
            symbols.push_str("    key <K0> { [ VoidSymbol ] };\n");
        }
        format!(
            "xkb_keymap {{\n\
             xkb_keycodes \"pairly\" {{\n    minimum = 8;\n    maximum = 255;\n{keycodes}}};\n\
             xkb_types \"pairly\" {{ include \"complete\" }};\n\
             xkb_compatibility \"pairly\" {{ include \"complete\" }};\n\
             xkb_symbols \"pairly\" {{\n{symbols}}};\n\
             }};\n"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keymap_grows_and_maps_modifiers() {
        let mut k = Keymap::default();
        assert!(k.render().contains("VoidSymbol"));
        assert_eq!(k.add("U00E9"), 1);
        assert_eq!(k.add("Control_L"), 2);
        assert_eq!(k.find("U00E9"), Some(1));
        let text = k.render();
        assert!(text.contains("<K0> = 9;"));
        assert!(text.contains("key <K0> { [ U00E9 ] };"));
        assert!(text.contains("modifier_map Control { <K1> };"));
        assert_eq!(special_keysym(SpecialKey::F(5)), "F5");
        assert_eq!(special_keysym(SpecialKey::PageDown), "Next");
    }
}
