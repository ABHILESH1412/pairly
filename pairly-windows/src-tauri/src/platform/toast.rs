//! Windows notifications with buttons and a reply box (Tauri's notification plugin can show
//! only plain ones). Each notification has a tag (and the device it's about as its group), so
//! it can be replaced or taken down; clicks, replies and dismissals come back through the
//! callback given with it, while the app runs.
//!
//! Windows shows notifications only for a known app id: the installer's Start-menu shortcut
//! carries it, and it's also registered here (under HKCU) so a build run from anywhere works.

#[derive(Debug, Clone, Default)]
pub struct Button {
    /// Handed back in [`ToastEvent::Clicked`].
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, Default)]
pub struct Toast {
    pub title: String,
    pub body: String,
    /// Small text under the body, e.g. "WhatsApp · Pixel 8".
    pub attribution: String,
    /// A PNG file shown as the notification's picture.
    pub icon: Option<std::path::PathBuf>,
    pub buttons: Vec<Button>,
    /// Show a reply box with a Send button (its text comes back as [`ToastEvent::Reply`]).
    pub reply: bool,
    pub silent: bool,
    /// Straight to the notification centre, without popping up (an update to an earlier one).
    pub quiet: bool,
    /// Stay on screen and ring like a phone call until answered.
    pub call: bool,
    /// Replaces an earlier notification with the same tag and group.
    pub tag: String,
    pub group: String,
}

#[derive(Debug, Clone)]
pub enum ToastEvent {
    /// The notification itself was clicked.
    Opened,
    Clicked(String),
    Reply(String),
    /// The user closed it (not timed out or replaced).
    Dismissed,
}

pub type OnEvent = Box<dyn Fn(ToastEvent) + Send + Sync>;

pub const APP_ID: &str = "io.github.abhilesh1412.pairly";

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            // Control characters other than newlines aren't allowed in XML.
            c if c.is_control() && c != '\n' && c != '\t' => {}
            c => out.push(c),
        }
    }
    out
}

/// The notification's XML (ToastGeneric).
pub fn toast_xml(t: &Toast) -> String {
    let mut xml = String::new();
    let scenario = if t.call {
        r#" scenario="incomingCall""#
    } else {
        ""
    };
    xml.push_str(&format!(
        r#"<toast launch="open"{scenario}><visual><binding template="ToastGeneric">"#
    ));
    xml.push_str(&format!("<text>{}</text>", xml_escape(&t.title)));
    if !t.body.is_empty() {
        xml.push_str(&format!("<text>{}</text>", xml_escape(&t.body)));
    }
    if !t.attribution.is_empty() {
        xml.push_str(&format!(
            r#"<text placement="attribution">{}</text>"#,
            xml_escape(&t.attribution)
        ));
    }
    if let Some(icon) = &t.icon {
        let url = format!("file:///{}", icon.display().to_string().replace('\\', "/"));
        xml.push_str(&format!(
            r#"<image placement="appLogoOverride" hint-crop="circle" src="{}"/>"#,
            xml_escape(&url)
        ));
    }
    xml.push_str("</binding></visual>");
    if t.reply || !t.buttons.is_empty() {
        xml.push_str("<actions>");
        if t.reply {
            xml.push_str(r#"<input id="reply" type="text" placeHolderContent="Reply"/>"#);
            xml.push_str(r#"<action content="Send" arguments="reply" hint-inputId="reply"/>"#);
        }
        // Windows shows at most five buttons.
        for b in t.buttons.iter().take(if t.reply { 4 } else { 5 }) {
            xml.push_str(&format!(
                r#"<action content="{}" arguments="b:{}"/>"#,
                xml_escape(&b.label),
                xml_escape(&b.id)
            ));
        }
        xml.push_str("</actions>");
    }
    if t.call {
        xml.push_str(r#"<audio src="ms-winsoundevent:Notification.Looping.Call" loop="true"/>"#);
    } else if t.silent {
        xml.push_str(r#"<audio silent="true"/>"#);
    }
    xml.push_str("</toast>");
    xml
}

/// A tag Windows accepts (at most 64 characters): long ids are shortened to a hash.
pub fn short_tag(id: &str) -> String {
    if id.len() <= 60 && id.chars().all(|c| c.is_ascii_graphic()) {
        return id.to_owned();
    }
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    id.hash(&mut h);
    format!("h{:016x}", h.finish())
}

#[cfg(windows)]
mod imp {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock, PoisonError};

    use windows::Data::Xml::Dom::XmlDocument;
    use windows::Foundation::{IPropertyValue, TypedEventHandler};
    use windows::UI::Notifications::{
        ToastActivatedEventArgs, ToastDismissalReason, ToastDismissedEventArgs, ToastNotification,
        ToastNotificationManager,
    };
    use windows::core::{HSTRING, IInspectable, Interface};

    use super::{APP_ID, OnEvent, Toast, ToastEvent, toast_xml};

    /// Shown notifications, kept so their events keep arriving: (group, tag) → notification.
    fn shown() -> &'static Mutex<HashMap<(String, String), ToastNotification>> {
        static SHOWN: OnceLock<Mutex<HashMap<(String, String), ToastNotification>>> =
            OnceLock::new();
        SHOWN.get_or_init(Mutex::default)
    }

    /// Register the app id (name and icon for notifications) and use it for this process.
    pub fn register(icon: Option<&std::path::Path>) {
        use windows::Win32::System::Registry::{HKEY_CURRENT_USER, REG_SZ, RegSetKeyValueW};
        use windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;
        let key = HSTRING::from(format!(r"Software\Classes\AppUserModelId\{APP_ID}"));
        let set = |name: &str, value: &str| {
            let wide: Vec<u16> = value.encode_utf16().chain(Some(0)).collect();
            // SAFETY: a valid key path, value name and NUL-terminated UTF-16 buffer.
            let r = unsafe {
                RegSetKeyValueW(
                    HKEY_CURRENT_USER,
                    &key,
                    &HSTRING::from(name),
                    REG_SZ.0,
                    Some(wide.as_ptr().cast()),
                    (wide.len() * 2) as u32,
                )
            };
            if r.is_err() {
                tracing::debug!(?r, "can't register the app id's {name}");
            }
        };
        set("DisplayName", "Pairly");
        if let Some(icon) = icon {
            set("IconUri", &icon.display().to_string());
        }
        // SAFETY: a valid string; affects only this process's taskbar grouping and toasts.
        if let Err(e) = unsafe { SetCurrentProcessExplicitAppUserModelID(&HSTRING::from(APP_ID)) } {
            tracing::debug!(error = %e, "can't set the app id");
        }
    }

    fn user_text(args: &ToastActivatedEventArgs, id: &str) -> Option<String> {
        let inputs = args.UserInput().ok()?;
        let value = inputs.Lookup(&HSTRING::from(id)).ok()?;
        let value: IPropertyValue = value.cast().ok()?;
        Some(value.GetString().ok()?.to_string())
    }

    pub fn show(t: &Toast, on_event: Option<OnEvent>) -> windows::core::Result<()> {
        let doc = XmlDocument::new()?;
        doc.LoadXml(&HSTRING::from(toast_xml(t)))?;
        let toast = ToastNotification::CreateToastNotification(&doc)?;
        if !t.tag.is_empty() {
            toast.SetTag(&HSTRING::from(t.tag.as_str()))?;
        }
        if !t.group.is_empty() {
            toast.SetGroup(&HSTRING::from(t.group.as_str()))?;
        }
        if t.quiet {
            toast.SetSuppressPopup(true)?;
        }
        if let Some(on_event) = on_event {
            let on_event = std::sync::Arc::new(on_event);
            let activated = on_event.clone();
            toast.Activated(&TypedEventHandler::<ToastNotification, IInspectable>::new(
                move |_, args| {
                    let Some(args) = args.as_ref() else {
                        return Ok(());
                    };
                    let args: ToastActivatedEventArgs = args.cast()?;
                    let arguments = args.Arguments()?.to_string();
                    let event = if arguments == "reply" {
                        match user_text(&args, "reply").filter(|t| !t.trim().is_empty()) {
                            Some(text) => ToastEvent::Reply(text),
                            None => return Ok(()),
                        }
                    } else if let Some(id) = arguments.strip_prefix("b:") {
                        ToastEvent::Clicked(id.to_owned())
                    } else {
                        ToastEvent::Opened
                    };
                    activated(event);
                    Ok(())
                },
            ))?;
            let key = (t.group.clone(), t.tag.clone());
            toast.Dismissed(&TypedEventHandler::<
                ToastNotification,
                ToastDismissedEventArgs,
            >::new(move |_, args| {
                if let Some(args) = args.as_ref()
                    && args.Reason()? == ToastDismissalReason::UserCanceled
                {
                    on_event(ToastEvent::Dismissed);
                    shown()
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .remove(&key);
                }
                Ok(())
            }))?;
        }
        let notifier = ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(APP_ID))?;
        notifier.Show(&toast)?;
        let mut map = shown().lock().unwrap_or_else(PoisonError::into_inner);
        // Keep the latest few hundred; older ones have long left the screen.
        if map.len() > 300 {
            map.clear();
        }
        map.insert((t.group.clone(), t.tag.clone()), toast);
        Ok(())
    }

    /// Take a notification down (from the screen and the notification centre).
    pub fn remove(group: &str, tag: &str) {
        shown()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&(group.to_owned(), tag.to_owned()));
        let r = ToastNotificationManager::History().and_then(|h| {
            h.RemoveGroupedTagWithId(
                &HSTRING::from(tag),
                &HSTRING::from(group),
                &HSTRING::from(APP_ID),
            )
        });
        if let Err(e) = r {
            tracing::debug!(error = %e, "can't remove a notification");
        }
    }

    /// Take down every notification about one device.
    pub fn remove_group(group: &str) {
        shown()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|(g, _), _| g != group);
        let _ = ToastNotificationManager::History()
            .and_then(|h| h.RemoveGroupWithId(&HSTRING::from(group), &HSTRING::from(APP_ID)));
    }
}

#[cfg(windows)]
pub use imp::{register, remove, remove_group};

/// Show a notification; false if Windows wouldn't (the caller can fall back to a plain one).
pub fn show(t: &Toast, on_event: Option<OnEvent>) -> bool {
    #[cfg(windows)]
    {
        match imp::show(t, on_event) {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(error = %e, "can't show a notification");
                false
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (t, on_event);
        false
    }
}

#[cfg(not(windows))]
pub fn register(_icon: Option<&std::path::Path>) {}
#[cfg(not(windows))]
pub fn remove(_group: &str, _tag: &str) {}
#[cfg(not(windows))]
pub fn remove_group(_group: &str) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_is_escaped_and_has_the_parts() {
        let t = Toast {
            title: "Tom & <Jerry>".into(),
            body: "hi \"there\"\u{7}".into(),
            reply: true,
            buttons: vec![Button {
                id: "k1".into(),
                label: "Mark read".into(),
            }],
            ..Toast::default()
        };
        let xml = toast_xml(&t);
        assert!(xml.contains("<text>Tom &amp; &lt;Jerry&gt;</text>"));
        assert!(xml.contains("<text>hi &quot;there&quot;</text>"));
        assert!(xml.contains(r#"hint-inputId="reply""#));
        assert!(xml.contains(r#"arguments="b:k1""#));
    }

    #[test]
    fn tags_stay_short() {
        assert_eq!(short_tag("abc"), "abc");
        let long = "x".repeat(200);
        assert!(short_tag(&long).len() <= 64);
        assert_eq!(short_tag(&long), short_tag(&long));
    }
}
