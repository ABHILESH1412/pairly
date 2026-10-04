//! XDG RemoteDesktop portal backend (GNOME, KDE): the desktop asks once whether Pairly may
//! control the keyboard and pointer, and remembers the answer (a restore token kept in the data
//! directory). Input then goes through the portal's `Notify*` calls; text is sent as keysyms, so
//! any character types regardless of layout.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::StreamExt;
use pairly_plugins::input::{
    ButtonAction, KeyInput, MouseButton, PointerButton, PointerMotion, SpecialKey,
};
use tracing::info;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::{Connection, MatchRule, MessageStream, Proxy};

const DEST: &str = "org.freedesktop.portal.Desktop";
const PATH: &str = "/org/freedesktop/portal/desktop";
const IFACE: &str = "org.freedesktop.portal.RemoteDesktop";
const KEYBOARD: u32 = 1;
const POINTER: u32 = 2;
/// Keep the permission until revoked.
const PERSIST_PERMANENT: u32 = 2;
/// The user may take a while to answer the permission dialog.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(300);

pub struct Portal {
    rt: tokio::runtime::Runtime,
    proxy: Proxy<'static>,
    session: OwnedObjectPath,
}

type Options<'a> = HashMap<&'a str, Value<'a>>;

fn token() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(std::process::id().into());
    format!("pairly{:x}", h.finish())
}

/// Call a portal method that answers through a `Request` object, and wait for the answer.
async fn request<B>(
    conn: &Connection,
    proxy: &Proxy<'_>,
    method: &str,
    handle_token: &str,
    body: &B,
) -> Result<HashMap<String, OwnedValue>, String>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
{
    let sender = conn
        .unique_name()
        .ok_or("no bus name")?
        .trim_start_matches(':')
        .replace('.', "_");
    let path = format!("{PATH}/request/{sender}/{handle_token}");
    // Subscribe before calling, so the answer can't be missed.
    let rule = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface("org.freedesktop.portal.Request")
        .map_err(|e| e.to_string())?
        .member("Response")
        .map_err(|e| e.to_string())?
        .path(path.as_str())
        .map_err(|e| e.to_string())?
        .build();
    let mut answers = MessageStream::for_match_rule(rule, conn, None)
        .await
        .map_err(|e| e.to_string())?;
    proxy
        .call_method(method, body)
        .await
        .map_err(|e| format!("{method}: {e}"))?;
    let answer = tokio::time::timeout(ANSWER_TIMEOUT, answers.next())
        .await
        .map_err(|_| format!("{method}: no answer"))?
        .ok_or_else(|| format!("{method}: no answer"))?
        .map_err(|e| e.to_string())?;
    let (code, results): (u32, HashMap<String, OwnedValue>) =
        answer.body().deserialize().map_err(|e| e.to_string())?;
    match code {
        0 => Ok(results),
        1 => Err("remote control was not allowed on this PC".into()),
        _ => Err(format!("{method} failed")),
    }
}

fn token_file(data_dir: &Path) -> PathBuf {
    data_dir.join("remote-desktop-token")
}

pub fn open(data_dir: &Path) -> Result<Portal, String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let (proxy, session) = rt.block_on(async {
        let conn = Connection::session().await.map_err(|e| e.to_string())?;
        let proxy = Proxy::new(&conn, DEST, PATH, IFACE)
            .await
            .map_err(|e| e.to_string())?;
        let version: u32 = proxy
            .get_property("version")
            .await
            .map_err(|_| "this desktop has no RemoteDesktop portal".to_owned())?;

        let t = token();
        let mut opts: Options = HashMap::new();
        opts.insert("handle_token", Value::from(format!("{t}a")));
        opts.insert("session_handle_token", Value::from(format!("{t}s")));
        let created = request(&conn, &proxy, "CreateSession", &format!("{t}a"), &(opts,)).await?;
        let session = created
            .get("session_handle")
            .and_then(|v| String::try_from(v.try_clone().ok()?).ok())
            .ok_or("no session handle")?;
        let session = OwnedObjectPath::try_from(session).map_err(|e| e.to_string())?;

        let saved = std::fs::read_to_string(token_file(data_dir)).ok();
        let mut opts: Options = HashMap::new();
        opts.insert("handle_token", Value::from(format!("{t}b")));
        opts.insert("types", Value::from(KEYBOARD | POINTER));
        if version >= 2 {
            opts.insert("persist_mode", Value::from(PERSIST_PERMANENT));
            if let Some(saved) = saved.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                opts.insert("restore_token", Value::from(saved.to_owned()));
            }
        }
        request(
            &conn,
            &proxy,
            "SelectDevices",
            &format!("{t}b"),
            &(&session, opts),
        )
        .await?;

        info!("asking the desktop for remote control (once)");
        let mut opts: Options = HashMap::new();
        opts.insert("handle_token", Value::from(format!("{t}c")));
        let started = request(
            &conn,
            &proxy,
            "Start",
            &format!("{t}c"),
            &(&session, "", opts),
        )
        .await?;
        if let Some(fresh) = started
            .get("restore_token")
            .and_then(|v| String::try_from(v.try_clone().ok()?).ok())
        {
            let _ = std::fs::write(token_file(data_dir), fresh);
        }
        Ok::<_, String>((proxy, session))
    })?;
    Ok(Portal { rt, proxy, session })
}

/// X keysym for a special key.
fn special_keysym(k: SpecialKey) -> u32 {
    match k {
        SpecialKey::Enter => 0xff0d,
        SpecialKey::Backspace => 0xff08,
        SpecialKey::Delete => 0xffff,
        SpecialKey::Tab => 0xff09,
        SpecialKey::Escape => 0xff1b,
        SpecialKey::Left => 0xff51,
        SpecialKey::Up => 0xff52,
        SpecialKey::Right => 0xff53,
        SpecialKey::Down => 0xff54,
        SpecialKey::Home => 0xff50,
        SpecialKey::End => 0xff57,
        SpecialKey::PageUp => 0xff55,
        SpecialKey::PageDown => 0xff56,
        SpecialKey::Space => 0x20,
        SpecialKey::F(n) => 0xffbe + u32::from(n.clamp(1, 24) - 1),
        SpecialKey::VolumeUp => 0x1008_ff13,
        SpecialKey::VolumeDown => 0x1008_ff11,
        SpecialKey::Mute => 0x1008_ff12,
    }
}

/// X keysym for a character: Latin-1 directly, everything else as a Unicode keysym.
fn char_keysym(c: char) -> u32 {
    match c {
        '\n' | '\r' => 0xff0d,
        '\t' => 0xff09,
        c => {
            let cp = u32::from(c);
            if (0x20..=0x7e).contains(&cp) || (0xa0..=0xff).contains(&cp) {
                cp
            } else {
                0x0100_0000 | cp
            }
        }
    }
}

impl Portal {
    fn call<B>(&self, method: &str, body: &B) -> Result<(), String>
    where
        B: serde::Serialize + zbus::zvariant::DynamicType,
    {
        self.rt
            .block_on(self.proxy.call_method(method, body))
            .map(drop)
            .map_err(|e| format!("{method}: {e}"))
    }

    fn keysym(&self, sym: u32, down: bool) -> Result<(), String> {
        let opts: Options = HashMap::new();
        self.call(
            "NotifyKeyboardKeysym",
            &(
                &self.session,
                opts,
                i32::try_from(sym).unwrap_or(0),
                u32::from(down),
            ),
        )
    }
}

impl super::Backend for Portal {
    fn name(&self) -> &'static str {
        "RemoteDesktop portal"
    }

    fn motion(&mut self, m: PointerMotion) -> Result<(), String> {
        if m.dx != 0.0 || m.dy != 0.0 {
            let opts: Options = HashMap::new();
            self.call(
                "NotifyPointerMotion",
                &(&self.session, opts, f64::from(m.dx), f64::from(m.dy)),
            )?;
        }
        if m.scroll_x != 0.0 || m.scroll_y != 0.0 {
            let opts: Options = HashMap::new();
            self.call(
                "NotifyPointerAxis",
                &(
                    &self.session,
                    opts,
                    f64::from(m.scroll_x),
                    f64::from(m.scroll_y),
                ),
            )?;
        }
        Ok(())
    }

    fn button(&mut self, b: PointerButton) -> Result<(), String> {
        let code: i32 = match b.button {
            MouseButton::Left => 0x110,
            MouseButton::Right => 0x111,
            MouseButton::Middle => 0x112,
        };
        let press = |state: u32| {
            let opts: Options = HashMap::new();
            self.call("NotifyPointerButton", &(&self.session, opts, code, state))
        };
        match b.action {
            ButtonAction::Press => press(1),
            ButtonAction::Release => press(0),
            ButtonAction::Click => press(1).and_then(|()| press(0)),
        }
    }

    fn key(&mut self, k: &KeyInput) -> Result<(), String> {
        let m = k.modifiers;
        let held: Vec<u32> = [
            (m.ctrl, 0xffe3),
            (m.alt, 0xffe9),
            (m.shift, 0xffe1),
            (m.logo, 0xffeb),
        ]
        .into_iter()
        .filter_map(|(on, sym)| on.then_some(sym))
        .collect();
        for sym in &held {
            self.keysym(*sym, true)?;
        }
        let mut syms: Vec<u32> = k.key.map(special_keysym).into_iter().collect();
        syms.extend(
            k.text
                .as_deref()
                .unwrap_or("")
                .chars()
                .filter(|c| !c.is_control() || matches!(c, '\n' | '\r' | '\t'))
                .map(char_keysym),
        );
        for sym in syms {
            self.keysym(sym, true)?;
            self.keysym(sym, false)?;
        }
        for sym in held.iter().rev() {
            self.keysym(*sym, false)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keysyms() {
        assert_eq!(char_keysym('a'), 0x61);
        assert_eq!(char_keysym('é'), 0xe9);
        assert_eq!(char_keysym('€'), 0x0100_20ac);
        assert_eq!(char_keysym('\n'), 0xff0d);
        assert_eq!(special_keysym(SpecialKey::F(5)), 0xffc2);
    }
}
