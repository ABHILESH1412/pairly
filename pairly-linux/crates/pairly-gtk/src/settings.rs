//! The app's own preferences (light or dark look, the sidebar) and the Settings window: this
//! PC's name, the look, and what this version is.
//!
//! The preferences live in `$XDG_CONFIG_HOME/pairly/gtk.conf` as `key = value` lines; the daemon's
//! settings (the PC's name) go through D-Bus.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use pairly_dbus::DaemonProxy;
use relm4::adw::prelude::*;
use relm4::{adw, gtk};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const WEBSITE: &str = "https://github.com/ABHILESH1412/pairly";

/// Light or dark, or whatever the desktop uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Theme {
    #[default]
    System,
    Light,
    Dark,
}

impl Theme {
    const ALL: [Self; 3] = [Self::System, Self::Light, Self::Dark];

    fn key(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::System => "Follow System",
            Self::Light => "Light",
            Self::Dark => "Dark",
        }
    }

    /// Switch the whole app (every window) to this look.
    pub fn apply(self) {
        adw::StyleManager::default().set_color_scheme(match self {
            Self::System => adw::ColorScheme::Default,
            Self::Light => adw::ColorScheme::ForceLight,
            Self::Dark => adw::ColorScheme::ForceDark,
        });
    }
}

/// The app's remembered preferences.
#[derive(Debug, Clone, Copy)]
pub struct Prefs {
    pub theme: Theme,
    /// The device list shows next to the device (on wide windows).
    pub sidebar: bool,
    /// Pairly is switched off: the background service stays stopped, even after a restart.
    pub off: bool,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            theme: Theme::System,
            sidebar: true,
            off: false,
        }
    }
}

fn prefs_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("pairly/gtk.conf"))
}

impl Prefs {
    pub fn load() -> Self {
        let mut prefs = Self::default();
        let text = prefs_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .unwrap_or_default();
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            match (key.trim(), value.trim()) {
                ("theme", v) => {
                    prefs.theme = Theme::ALL
                        .into_iter()
                        .find(|t| t.key() == v)
                        .unwrap_or_default();
                }
                ("sidebar", v) => prefs.sidebar = v != "false",
                ("off", v) => prefs.off = v == "true",
                _ => {}
            }
        }
        prefs
    }

    pub fn save(self) {
        let Some(path) = prefs_path() else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let text = format!(
            "# Pairly's window preferences (Settings changes these).\ntheme = {}\nsidebar = {}\noff = {}\n",
            self.theme.key(),
            self.sidebar,
            self.off
        );
        let _ = std::fs::write(path, text);
    }
}

/// Whether Pairly is switched off (then the app doesn't call the daemon, which would start it).
static OFF: AtomicBool = AtomicBool::new(false);

pub fn is_off() -> bool {
    OFF.load(Ordering::Relaxed)
}

pub fn set_off(off: bool) {
    OFF.store(off, Ordering::Relaxed);
}

const SERVICE: &str = "pairlyd.service";

/// Start or stop the background service for good: enabled (starts at login) and running, or
/// disabled and stopped. Through the user's systemd, as `systemctl --user enable --now` would.
/// Fails when systemd doesn't manage the daemon.
pub async fn switch_service(on: bool) -> zbus::Result<()> {
    let conn = zbus::Connection::session().await?;
    let systemd = zbus::Proxy::new(
        &conn,
        "org.freedesktop.systemd1",
        "/org/freedesktop/systemd1",
        "org.freedesktop.systemd1.Manager",
    )
    .await?;
    let units = vec![SERVICE];
    if on {
        systemd
            .call_method("EnableUnitFiles", &(units, false, false))
            .await?;
        systemd.call_method("Reload", &()).await?;
        systemd
            .call_method("StartUnit", &(SERVICE, "replace"))
            .await?;
    } else {
        systemd
            .call_method("DisableUnitFiles", &(units, false))
            .await?;
        systemd.call_method("Reload", &()).await?;
        systemd
            .call_method("StopUnit", &(SERVICE, "replace"))
            .await?;
    }
    Ok(())
}

fn describe(e: &zbus::Error) -> String {
    match e {
        zbus::Error::MethodError(_, Some(msg), _) => msg.clone(),
        other => other.to_string(),
    }
}

/// The Settings window. `me` is this PC's (id, name) as the daemon knows it (`None` while the
/// daemon isn't running).
pub fn open(
    parent: &impl IsA<gtk::Widget>,
    daemon: Option<DaemonProxy<'static>>,
    me: Option<(String, String)>,
    open_commands: impl Fn() + 'static,
    open_notification_apps: impl Fn() + 'static,
) {
    let dialog = adw::PreferencesDialog::builder()
        .title("Settings")
        .search_enabled(false)
        .build();
    let page = adw::PreferencesPage::new();

    // Appearance.
    let look = adw::PreferencesGroup::builder().title("Appearance").build();
    let themes = gtk::StringList::new(&Theme::ALL.map(Theme::label));
    let theme_row = adw::ComboRow::builder()
        .title("Style")
        .subtitle("Light, dark, or the same as your desktop")
        .model(&themes)
        .build();
    let current = Prefs::load().theme;
    theme_row.set_selected(
        Theme::ALL
            .iter()
            .position(|t| *t == current)
            .and_then(|i| u32::try_from(i).ok())
            .unwrap_or(0),
    );
    theme_row.connect_selected_notify(|row| {
        let theme = Theme::ALL
            .get(row.selected() as usize)
            .copied()
            .unwrap_or_default();
        theme.apply();
        let mut prefs = Prefs::load();
        prefs.theme = theme;
        prefs.save();
    });
    look.add(&theme_row);
    page.add(&look);

    // This PC.
    let this_pc = adw::PreferencesGroup::builder()
        .title("This PC")
        .description("Your phone shows this name. It updates there after a few seconds.")
        .build();
    let name_row = adw::EntryRow::builder()
        .title("Device name")
        .show_apply_button(true)
        .sensitive(daemon.is_some())
        .build();
    if let Some((_, name)) = &me {
        name_row.set_text(name);
    }
    name_row.connect_apply({
        let (dialog, daemon) = (dialog.clone(), daemon.clone());
        move |row| {
            let name = row.text().trim().to_owned();
            if name.is_empty() || name.chars().count() > 64 {
                dialog.add_toast(adw::Toast::new("Use 1 to 64 characters"));
                return;
            }
            let Some(daemon) = daemon.clone() else { return };
            let dialog = dialog.clone();
            gtk::glib::spawn_future_local(async move {
                let result = relm4::spawn(async move { daemon.set_name(&name).await }).await;
                let text = match result {
                    Ok(Ok(())) => "Renamed. Reconnecting your devices…".to_owned(),
                    Ok(Err(e)) => format!("Couldn't rename: {}", describe(&e)),
                    Err(e) => format!("Couldn't rename: {e}"),
                };
                dialog.add_toast(adw::Toast::new(&text));
            });
        }
    });
    this_pc.add(&name_row);
    let commands = adw::ActionRow::builder()
        .title("Commands")
        .subtitle("What your phone may run on this PC")
        .activatable(true)
        .build();
    commands.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
    commands.connect_activated(move |_| open_commands());
    this_pc.add(&commands);
    let notifications = adw::ActionRow::builder()
        .title("PC notifications")
        .subtitle("Which apps' notifications go to your phone")
        .activatable(true)
        .build();
    notifications.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
    notifications.connect_activated(move |_| open_notification_apps());
    this_pc.add(&notifications);
    if let Some((id, _)) = &me {
        let id_row = adw::ActionRow::builder()
            .title("Device ID")
            .subtitle(id)
            .subtitle_selectable(true)
            .build();
        id_row.add_css_class("property");
        this_pc.add(&id_row);
    }
    page.add(&this_pc);

    // About.
    let about = adw::PreferencesGroup::builder().title("About").build();
    let version = adw::ActionRow::builder()
        .title("Version")
        .subtitle(VERSION)
        .build();
    version.add_css_class("property");
    about.add(&version);
    let more = adw::ButtonRow::builder()
        .title("About Pairly")
        .end_icon_name("go-next-symbolic")
        .build();
    more.connect_activated({
        let dialog = dialog.clone();
        move |_| about_dialog().present(Some(&dialog))
    });
    about.add(&more);
    page.add(&about);

    dialog.add(&page);
    dialog.present(Some(parent));
}

/// Name, version, licence and links.
pub fn about_dialog() -> adw::AboutDialog {
    adw::AboutDialog::builder()
        .application_name("Pairly")
        .application_icon("io.github.abhilesh1412.Pairly")
        .developer_name("Abhilesh Singh")
        .version(VERSION)
        .comments("Your phone and your PC, working as one: notifications, files, messages, calls, the clipboard and each other's screens.")
        .website(WEBSITE)
        .issue_url(format!("{WEBSITE}/issues"))
        .license_type(gtk::License::Gpl30)
        .build()
}
