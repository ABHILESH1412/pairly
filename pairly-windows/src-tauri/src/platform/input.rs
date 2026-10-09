//! Remote mouse and keyboard: a paired phone moves the pointer, clicks, scrolls and types.
//! The input library isn't thread-safe everywhere, so one thread owns it and takes commands.

use std::sync::mpsc::{Sender, channel};

use enigo::{Axis, Button, Coordinate, Direction, Enigo, Key, Keyboard, Mouse, Settings};
use pairly_plugins::input::{
    ButtonAction, KeyInput, MouseButton, PointerButton, PointerMotion, SpecialKey,
};

enum Cmd {
    Motion(PointerMotion),
    Button(PointerButton),
    Key(KeyInput),
}

pub struct RemoteInput {
    tx: Sender<Cmd>,
}

impl RemoteInput {
    pub fn new() -> Self {
        let (tx, rx) = channel::<Cmd>();
        std::thread::Builder::new()
            .name("pairly-input".into())
            .spawn(move || {
                let mut enigo = match Enigo::new(&Settings::default()) {
                    Ok(e) => e,
                    Err(e) => {
                        tracing::warn!(error = %e, "remote input isn't available");
                        return;
                    }
                };
                // Scroll comes as fractional steps: keep the remainder for the next one.
                let (mut sx, mut sy) = (0.0_f32, 0.0_f32);
                for cmd in rx {
                    let result = match cmd {
                        Cmd::Motion(m) => {
                            let mut r = Ok(());
                            if m.dx != 0.0 || m.dy != 0.0 {
                                r = enigo.move_mouse(
                                    m.dx.round() as i32,
                                    m.dy.round() as i32,
                                    Coordinate::Rel,
                                );
                            }
                            sx += m.scroll_x;
                            sy += m.scroll_y;
                            let (whole_x, whole_y) = (sx.trunc(), sy.trunc());
                            if whole_y != 0.0 {
                                r = r.and(enigo.scroll(whole_y as i32, Axis::Vertical));
                                sy -= whole_y;
                            }
                            if whole_x != 0.0 {
                                r = r.and(enigo.scroll(whole_x as i32, Axis::Horizontal));
                                sx -= whole_x;
                            }
                            r
                        }
                        Cmd::Button(b) => {
                            let button = match b.button {
                                MouseButton::Left => Button::Left,
                                MouseButton::Right => Button::Right,
                                MouseButton::Middle => Button::Middle,
                            };
                            let direction = match b.action {
                                ButtonAction::Click => Direction::Click,
                                ButtonAction::Press => Direction::Press,
                                ButtonAction::Release => Direction::Release,
                            };
                            enigo.button(button, direction)
                        }
                        Cmd::Key(k) => type_key(&mut enigo, &k),
                    };
                    if let Err(e) = result {
                        tracing::debug!(error = %e, "remote input failed");
                    }
                }
            })
            .expect("can start the input thread");
        Self { tx }
    }

    pub fn pointer(&self, motion: PointerMotion) {
        let _ = self.tx.send(Cmd::Motion(motion));
    }

    pub fn button(&self, button: PointerButton) {
        let _ = self.tx.send(Cmd::Button(button));
    }

    pub fn key(&self, key: KeyInput) {
        let _ = self.tx.send(Cmd::Key(key));
    }
}

fn special(key: SpecialKey) -> Key {
    match key {
        SpecialKey::Enter => Key::Return,
        SpecialKey::Backspace => Key::Backspace,
        SpecialKey::Delete => Key::Delete,
        SpecialKey::Tab => Key::Tab,
        SpecialKey::Escape => Key::Escape,
        SpecialKey::Left => Key::LeftArrow,
        SpecialKey::Right => Key::RightArrow,
        SpecialKey::Up => Key::UpArrow,
        SpecialKey::Down => Key::DownArrow,
        SpecialKey::Home => Key::Home,
        SpecialKey::End => Key::End,
        SpecialKey::PageUp => Key::PageUp,
        SpecialKey::PageDown => Key::PageDown,
        SpecialKey::Space => Key::Space,
        SpecialKey::F(n) => match n {
            1 => Key::F1,
            2 => Key::F2,
            3 => Key::F3,
            4 => Key::F4,
            5 => Key::F5,
            6 => Key::F6,
            7 => Key::F7,
            8 => Key::F8,
            9 => Key::F9,
            10 => Key::F10,
            11 => Key::F11,
            _ => Key::F12,
        },
        SpecialKey::VolumeUp => Key::VolumeUp,
        SpecialKey::VolumeDown => Key::VolumeDown,
        SpecialKey::Mute => Key::VolumeMute,
    }
}

/// Text is typed as text (any language); a special key with its modifiers held around it.
fn type_key(enigo: &mut Enigo, k: &KeyInput) -> enigo::InputResult<()> {
    let mods: Vec<Key> = [
        (k.modifiers.ctrl, Key::Control),
        (k.modifiers.alt, Key::Alt),
        (k.modifiers.shift, Key::Shift),
        (k.modifiers.logo, Key::Meta),
    ]
    .into_iter()
    .filter_map(|(on, key)| on.then_some(key))
    .collect();
    for m in &mods {
        enigo.key(*m, Direction::Press)?;
    }
    let result = match (&k.key, &k.text) {
        (Some(key), _) => enigo.key(special(*key), Direction::Click),
        // A shortcut such as Ctrl+C: the letter as a key, not as typed text.
        (None, Some(text)) if !mods.is_empty() && text.chars().count() == 1 => {
            let c = text.chars().next().unwrap_or(' ').to_ascii_lowercase();
            enigo.key(Key::Unicode(c), Direction::Click)
        }
        (None, Some(text)) => enigo.text(text),
        (None, None) => Ok(()),
    };
    for m in mods.iter().rev() {
        let _ = enigo.key(*m, Direction::Release);
    }
    result
}
