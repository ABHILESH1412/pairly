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

    page.add(&file_sharing_group(&dialog, daemon.clone()));
    page.add(&relay_group(&dialog, daemon.clone()));
    page.add(&updates_group(daemon.clone()));

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

/// Run a daemon call off the GTK thread; `done` gets the result back on it.
fn call<T: Send + 'static>(
    fut: impl Future<Output = zbus::Result<T>> + Send + 'static,
    done: impl FnOnce(Result<T, String>) + 'static,
) {
    gtk::glib::spawn_future_local(async move {
        let result = match relm4::spawn(fut).await {
            Ok(r) => r.map_err(|e| describe(&e)),
            Err(e) => Err(e.to_string()),
        };
        done(result);
    });
}

/// What the updater is doing, in words (from the daemon's `UpdateStatus`).
fn update_text(current: &str, state: &str, detail: &str) -> String {
    match state {
        "checking" => "Checking for updates…".to_owned(),
        "up-to-date" => format!("Pairly {current} is up to date"),
        "available" => format!("Pairly {detail} is available"),
        "installing" => format!("Installing Pairly {detail}…"),
        "installed" => format!("Updated to {detail}: reopen Pairly to use it"),
        "managed" => format!("Pairly {detail} is available: update it with your package manager"),
        "failed" => format!("Couldn't update: {detail}"),
        _ => format!("Pairly {current}"),
    }
}

/// Sharing a folder with paired devices: on or off, which folder, and whether they may change
/// things there.
fn file_sharing_group(
    dialog: &adw::PreferencesDialog,
    daemon: Option<DaemonProxy<'static>>,
) -> adw::PreferencesGroup {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    let group = adw::PreferencesGroup::builder()
        .title("Share Files")
        .description(
            "Your paired devices can browse this folder and download from it, wherever they \
             are. Hidden files (like .ssh) are never shared.",
        )
        .build();
    let on = adw::SwitchRow::builder()
        .title("Share a folder with my devices")
        .sensitive(false)
        .build();
    let folder_row = adw::ActionRow::builder()
        .title("Folder")
        .subtitle_selectable(true)
        .sensitive(false)
        .build();
    let choose = gtk::Button::builder()
        .label("Choose…")
        .valign(gtk::Align::Center)
        .build();
    folder_row.add_suffix(&choose);
    let changes = adw::SwitchRow::builder()
        .title("Allow changes")
        .subtitle("They may also upload, rename and delete there")
        .sensitive(false)
        .build();
    group.add(&on);
    group.add(&folder_row);
    group.add(&changes);
    let Some(daemon) = daemon else { return group };

    let folder = Rc::new(RefCell::new(String::new()));
    // Set while showing the daemon's values, so that doesn't count as a change.
    let loading = Rc::new(Cell::new(true));
    let save = {
        let (dialog, daemon, on, changes, folder, loading) = (
            dialog.clone(),
            daemon.clone(),
            on.clone(),
            changes.clone(),
            folder.clone(),
            loading.clone(),
        );
        Rc::new(move || {
            if loading.get() {
                return;
            }
            let (share, allow) = (on.is_active(), changes.is_active());
            let path = folder.borrow().clone();
            let (dialog, daemon) = (dialog.clone(), daemon.clone());
            gtk::glib::spawn_future_local(async move {
                let result =
                    relm4::spawn(async move { daemon.set_file_sharing(share, &path, allow).await })
                        .await;
                let text = match result {
                    Ok(Ok(())) if share => "Sharing that folder with your devices".to_owned(),
                    Ok(Ok(())) => "Not sharing files".to_owned(),
                    Ok(Err(e)) => format!("Couldn't change file sharing: {}", describe(&e)),
                    Err(e) => format!("Couldn't change file sharing: {e}"),
                };
                dialog.add_toast(adw::Toast::new(&text));
            });
        })
    };
    on.connect_active_notify({
        let (save, folder_row, changes) = (save.clone(), folder_row.clone(), changes.clone());
        move |row| {
            folder_row.set_sensitive(row.is_active());
            changes.set_sensitive(row.is_active());
            save();
        }
    });
    changes.connect_active_notify({
        let save = save.clone();
        move |_| save()
    });
    choose.connect_clicked({
        let (dialog, folder, folder_row, save) = (
            dialog.clone(),
            folder.clone(),
            folder_row.clone(),
            save.clone(),
        );
        move |_| {
            let (folder, folder_row, save) = (folder.clone(), folder_row.clone(), save.clone());
            let start = gtk::gio::File::for_path(folder.borrow().as_str());
            let window = dialog.root().and_downcast::<gtk::Window>();
            gtk::FileDialog::builder()
                .title("Folder to Share")
                .accept_label("Share")
                .initial_folder(&start)
                .build()
                .select_folder(window.as_ref(), gtk::gio::Cancellable::NONE, move |res| {
                    if let Some(path) = res.ok().and_then(|f| f.path()) {
                        let path = path.display().to_string();
                        folder_row.set_subtitle(&path);
                        *folder.borrow_mut() = path;
                        save();
                    }
                });
        }
    });
    gtk::glib::spawn_future_local(async move {
        if let Ok(Ok((share, path, allow))) =
            relm4::spawn(async move { daemon.file_sharing().await }).await
        {
            folder_row.set_subtitle(&path);
            *folder.borrow_mut() = path;
            on.set_active(share);
            changes.set_active(allow);
            on.set_sensitive(true);
            folder_row.set_sensitive(share);
            changes.set_sensitive(share);
        }
        loading.set(false);
    });
    group
}

/// The relay: how devices reach each other away from home.
fn relay_group(
    dialog: &adw::PreferencesDialog,
    daemon: Option<DaemonProxy<'static>>,
) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title("Away from home")
        .description(
            "Reach your devices over the internet through your own relay: the wss:// address \
             of your Cloudflare relay, or the pairly-relay:// one your server prints. Paired \
             phones learn it from this PC.",
        )
        .build();
    // The address holds the relay's access token: hidden unless asked.
    let row = adw::PasswordEntryRow::builder()
        .title("Relay address")
        .show_apply_button(true)
        .sensitive(daemon.is_some())
        .build();
    if let Some(daemon) = daemon.clone() {
        let row = row.clone();
        gtk::glib::spawn_future_local(async move {
            if let Ok(Ok(address)) = relm4::spawn(async move { daemon.relay().await }).await {
                row.set_text(&address);
            }
        });
    }
    row.connect_apply({
        let dialog = dialog.clone();
        move |row| {
            let address = row.text().trim().to_owned();
            let Some(daemon) = daemon.clone() else { return };
            let dialog = dialog.clone();
            gtk::glib::spawn_future_local(async move {
                let removed = address.is_empty();
                let result = relm4::spawn(async move { daemon.set_relay(&address).await }).await;
                let text = match result {
                    Ok(Ok(())) if removed => "Relay removed".to_owned(),
                    Ok(Ok(())) => "Relay saved. Reconnecting your devices…".to_owned(),
                    Ok(Err(e)) => format!("Couldn't use that relay: {}", describe(&e)),
                    Err(e) => format!("Couldn't use that relay: {e}"),
                };
                dialog.add_toast(adw::Toast::new(&text));
            });
        }
    });
    group.add(&row);
    group
}

/// Updates: automatic or not, where they stand, and a button to check (or install) now.
fn updates_group(daemon: Option<DaemonProxy<'static>>) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title("Updates")
        .description(
            "New versions come from Pairly's GitHub releases and are installed only when \
             they carry the project's signature.",
        )
        .build();
    let auto = adw::SwitchRow::builder()
        .title("Install updates automatically")
        .sensitive(daemon.is_some())
        .build();
    let status = adw::ActionRow::builder()
        .title("Pairly")
        .subtitle(format!("Version {VERSION}"))
        .build();
    let button = gtk::Button::builder()
        .label("Check Now")
        .valign(gtk::Align::Center)
        .sensitive(daemon.is_some())
        .build();
    status.add_suffix(&button);
    group.add(&auto);
    group.add(&status);
    let Some(daemon) = daemon else {
        return group;
    };

    // Shows the daemon's state; `available` (not installed automatically) offers Install.
    let refresh = {
        let (daemon, auto, status, button) =
            (daemon.clone(), auto.clone(), status.clone(), button.clone());
        move || {
            let (auto, status, button) = (auto.clone(), status.clone(), button.clone());
            let daemon = daemon.clone();
            call(async move { daemon.update_status().await }, move |r| {
                if let Ok((current, state, detail, on)) = r {
                    auto.set_active(on);
                    status.set_subtitle(&update_text(&current, &state, &detail));
                    button.set_label(if state == "available" {
                        "Install"
                    } else {
                        "Check Now"
                    });
                    button.set_sensitive(!matches!(state.as_str(), "checking" | "installing"));
                }
            });
        }
    };
    refresh();
    auto.connect_active_notify({
        let daemon = daemon.clone();
        move |row| {
            let (daemon, on) = (daemon.clone(), row.is_active());
            call(async move { daemon.set_auto_update(on).await }, |_| {});
        }
    });
    button.connect_clicked({
        let (daemon, refresh) = (daemon.clone(), refresh.clone());
        move |b| {
            let daemon = daemon.clone();
            let install = b.label().as_deref() == Some("Install");
            call(
                async move {
                    if install {
                        daemon.install_update().await
                    } else {
                        daemon.check_for_updates().await
                    }
                },
                |_| {},
            );
            refresh();
        }
    });
    // Follow the progress while the window is open (it stops once the window is gone).
    gtk::glib::timeout_add_seconds_local(2, {
        let (group, refresh) = (group.clone(), refresh.clone());
        move || {
            if group.root().is_none() {
                return gtk::glib::ControlFlow::Break;
            }
            refresh();
            gtk::glib::ControlFlow::Continue
        }
    });
    group
}
