//! The main window: a sidebar of devices (which can be hidden) and a page for the selected one. All state comes from
//! `pairlyd` over D-Bus; the UI never talks to the network itself.
//!
//! relm4 drives the message loop and runs async D-Bus work as commands on its Tokio runtime.
//! The device list and page are rebuilt from the latest state on every change: they are small,
//! and rebuilding keeps them trivially consistent with the daemon.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use futures_util::StreamExt;
use pairly_dbus::{DaemonProxy, Device, Player, Transfer};
use relm4::adw::prelude::*;
use relm4::{Component, ComponentParts, ComponentSender, Sender, adw, gtk};

const APP_TITLE: &str = "Pairly";

#[derive(Debug, Clone)]
enum Status {
    /// Switched off by the user.
    Off,
    Connecting,
    Ready,
    Unavailable(String),
}

pub struct Init {
    /// Open the pairing QR code as soon as the daemon is reachable.
    pub show_pairing: bool,
}

pub struct App {
    daemon: Option<DaemonProxy<'static>>,
    show_pairing_when_ready: bool,
    status: Status,
    me: Option<(String, String)>,
    devices: Vec<Device>,
    selected: Option<String>,
    qr_dialog: Option<adw::Dialog>,
    /// Devices we asked to ring (shows "Stop" until pressed).
    ringing: std::collections::HashSet<String>,
    /// Unfinished transfers, by id.
    transfers: BTreeMap<u64, Transfer>,
    /// Media players on each device.
    players: HashMap<String, Vec<Player>>,
    /// Pairly is switched off.
    off: bool,
}

#[derive(Debug)]
pub enum Input {
    Select(String),
    Pair(String),
    Ping(String),
    Ring(String, bool),
    SendClipboard(String),
    /// Choose files to send to a device.
    PickFiles(String),
    SendFiles(String, Vec<String>),
    /// Files dropped on the device page.
    DropFiles(Vec<String>),
    /// Ask for a link or text to send.
    AskText(String),
    SendText(String, String),
    AcceptTransfer(u64),
    CancelTransfer(u64),
    /// Open the Messages window for a phone.
    OpenMessages(String),
    OpenCommands,
    /// Choose which of this PC's apps send notifications to the phone.
    OpenNotificationApps,
    /// Open the Contacts window for a phone.
    OpenContacts(String),
    /// Browse a phone's files.
    OpenFiles(String),
    /// Watch and control a phone's screen.
    ShowScreen(String),
    /// Lock a phone, or power it off / restart it: (device, "lock" | "poweroff" | "restart").
    Power(String, &'static str),
    /// Ask whether to power off or restart a phone.
    AskPower(String),
    /// Control a device's player: (device, player, action).
    Media(String, String, &'static str),
    Unpair(String),
    /// Pause a paired device (true) or resume it.
    SetPaused(String, bool),
    OpenSettings,
    About,
    /// Switch Pairly on or off.
    SetOn(bool),
    ShowQr,
    QrClosed,
    RefreshQr,
}

#[derive(Debug)]
pub enum Cmd {
    Connected(DaemonProxy<'static>, (String, String)),
    Unavailable(String),
    Devices(Vec<Device>),
    Changed,
    PairingRequested {
        id: String,
        name: String,
        code: String,
    },
    PairingFinished {
        id: String,
        success: bool,
        message: String,
    },
    PingReceived {
        name: String,
        message: String,
    },
    Qr(Result<(String, u32), String>),
    Transfers(Vec<Transfer>),
    TransferChanged(Transfer),
    PlayersChanged(String),
    Players(String, Vec<Player>),
    Failed(String),
    /// The daemon runs a different version (it updated itself): offer to restart the app.
    NewVersion(String),
    Done,
}

pub struct Widgets {
    toasts: adw::ToastOverlay,
    split: adw::OverlaySplitView,
    title: adw::WindowTitle,
    list: gtk::ListBox,
    /// The content's header: the device's name.
    content_title: adw::WindowTitle,
    content: gtk::Stack,
    device_slot: adw::Bin,
    status_page: adw::StatusPage,
    retry: gtk::Button,
    turn_on: gtk::Button,
    power: gtk::Switch,
    power_label: gtk::Label,
    qr_picture: gtk::Picture,
    /// Rows of the transfers on the current device page, updated in place as bytes arrive.
    transfer_rows: HashMap<u64, (adw::ActionRow, gtk::ProgressBar)>,
}

/// D-Bus errors arrive as "org.freedesktop.DBus.Error.Failed: reason"; show only the reason.
fn describe(e: &zbus::Error) -> String {
    match e {
        zbus::Error::MethodError(_, Some(msg), _) => msg.clone(),
        other => other.to_string(),
    }
}

fn icon_for(device_type: &str) -> &'static str {
    match device_type {
        "phone" => "phone-symbolic",
        "tablet" => "tablet-symbolic",
        _ => "computer-symbolic",
    }
}

fn link_name(link: &str) -> &str {
    match link {
        "lan" | "memory" => "Local network",
        "bluetooth" => "Bluetooth",
        "relay" => "Internet",
        other => other,
    }
}

fn status_text(d: &Device) -> String {
    match (d.paired, d.is_connected()) {
        (false, _) => "Available to pair".to_owned(),
        (true, _) if d.paused => "Paused".to_owned(),
        (true, false) => "Offline".to_owned(),
        (true, true) if d.rtt_ms > 0 => {
            format!("Connected · {} · {} ms", link_name(&d.link), d.rtt_ms)
        }
        (true, true) => format!("Connected · {}", link_name(&d.link)),
    }
}

impl App {
    fn device(&self, id: &str) -> Option<&Device> {
        self.devices.iter().find(|d| d.id == id)
    }

    fn name_of(&self, id: &str) -> String {
        self.device(id)
            .map_or_else(|| "the device".to_owned(), |d| d.name.clone())
    }

    /// Run a daemon call in the background; failures become a toast.
    fn call<F, Fut>(&self, sender: &ComponentSender<Self>, f: F)
    where
        F: FnOnce(DaemonProxy<'static>) -> Fut + Send + 'static,
        Fut: Future<Output = zbus::Result<()>> + Send,
    {
        let Some(daemon) = self.daemon.clone() else {
            return;
        };
        sender.oneshot_command(async move {
            match f(daemon).await {
                Ok(()) => Cmd::Done,
                Err(e) => Cmd::Failed(describe(&e)),
            }
        });
    }

    fn refresh(&self, sender: &ComponentSender<Self>) {
        let Some(daemon) = self.daemon.clone() else {
            return;
        };
        sender.oneshot_command(async move {
            match daemon.list_devices().await {
                Ok(devices) => Cmd::Devices(devices),
                Err(e) => Cmd::Failed(describe(&e)),
            }
        });
    }

    fn refresh_players(&self, sender: &ComponentSender<Self>, device: String) {
        let Some(daemon) = self.daemon.clone() else {
            return;
        };
        sender.oneshot_command(async move {
            match daemon.list_players(&device).await {
                Ok(players) => Cmd::Players(device, players),
                Err(_) => Cmd::Done,
            }
        });
    }

    fn refresh_transfers(&self, sender: &ComponentSender<Self>) {
        let Some(daemon) = self.daemon.clone() else {
            return;
        };
        sender.oneshot_command(async move {
            match daemon.list_transfers().await {
                Ok(transfers) => Cmd::Transfers(transfers),
                Err(e) => Cmd::Failed(describe(&e)),
            }
        });
    }

    /// The selected device, if files can be sent to it now.
    fn send_target(&self) -> Option<&Device> {
        self.selected
            .as_deref()
            .and_then(|id| self.device(id))
            .filter(|d| d.paired && d.is_connected())
    }

    fn request_qr(&self, sender: &ComponentSender<Self>) {
        let Some(daemon) = self.daemon.clone() else {
            return;
        };
        sender.oneshot_command(async move {
            Cmd::Qr(daemon.start_qr_pairing().await.map_err(|e| describe(&e)))
        });
    }

    fn toast(widgets: &Widgets, text: &str) {
        widgets.toasts.add_toast(adw::Toast::new(text));
    }
}

/// Connect to the daemon and forward its signals until the component shuts down. The proxy
/// follows the well-known name, so a restarted daemon is picked up without reconnecting.
async fn watch_daemon(out: Sender<Cmd>) {
    loop {
        if crate::settings::is_off() {
            tokio::time::sleep(Duration::from_secs(1)).await;
            continue;
        }
        match connect().await {
            Ok((daemon, me)) => {
                let _ = out.send(Cmd::Connected(daemon.clone(), me));
                if let Err(e) = forward_signals(&daemon, &out).await {
                    let _ = out.send(Cmd::Unavailable(describe(&e)));
                }
            }
            Err(e) => {
                let _ = out.send(Cmd::Unavailable(describe(&e)));
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn connect() -> zbus::Result<(DaemonProxy<'static>, (String, String))> {
    let conn = zbus::Connection::session().await?;
    let daemon = DaemonProxy::new(&conn).await?;
    // Calling a method also starts pairlyd through D-Bus activation when it's installed.
    let me = daemon.get_identity().await?;
    Ok((daemon, me))
}

async fn forward_signals(daemon: &DaemonProxy<'static>, out: &Sender<Cmd>) -> zbus::Result<()> {
    let mut changed = daemon.receive_device_changed().await?;
    let mut requested = daemon.receive_pairing_requested().await?;
    let mut finished = daemon.receive_pairing_finished().await?;
    let mut pings = daemon.receive_ping_received().await?;
    let mut transfers = daemon.receive_transfer_changed().await?;
    let mut players = daemon.receive_players_changed().await?;
    let mut owner = daemon.inner().receive_owner_changed().await?;
    loop {
        let cmd = tokio::select! {
            Some(_) = changed.next() => Cmd::Changed,
            Some(s) = requested.next() => {
                let a = s.args()?;
                Cmd::PairingRequested { id: a.id.to_owned(), name: a.name.to_owned(), code: a.code.to_owned() }
            }
            Some(s) = finished.next() => {
                let a = s.args()?;
                Cmd::PairingFinished { id: a.id.to_owned(), success: a.success, message: a.message.to_owned() }
            }
            Some(s) = pings.next() => {
                let a = s.args()?;
                Cmd::PingReceived { name: a.name.to_owned(), message: a.message.to_owned() }
            }
            Some(s) = transfers.next() => Cmd::TransferChanged(s.args()?.transfer),
            Some(s) = players.next() => Cmd::PlayersChanged(s.args()?.id.to_owned()),
            Some(new_owner) = owner.next() => match new_owner {
                Some(_) => Cmd::Connected(daemon.clone(), daemon.get_identity().await?),
                None => Cmd::Unavailable("Pairly's background service stopped.".to_owned()),
            },
            else => return Ok(()),
        };
        if out.send(cmd).is_err() {
            return Ok(());
        }
    }
}

impl Component for App {
    type CommandOutput = Cmd;
    type Input = Input;
    type Output = ();
    type Init = Init;
    type Root = adw::ApplicationWindow;
    type Widgets = Widgets;

    fn init_root() -> Self::Root {
        adw::ApplicationWindow::builder()
            .title(APP_TITLE)
            .default_width(900)
            .default_height(620)
            .build()
    }

    fn init(init: Init, window: Self::Root, sender: ComponentSender<Self>) -> ComponentParts<Self> {
        install_css();
        // Launching Pairly again brings this window to the front.
        relm4::main_application().connect_activate({
            let window = window.clone();
            move |_| window.present()
        });

        let prefs = crate::settings::Prefs::load();
        prefs.theme.apply();
        crate::settings::set_off(prefs.off);

        // Sidebar: device list.
        let title = adw::WindowTitle::new(APP_TITLE, "");
        let pair_button = gtk::Button::builder()
            .icon_name("list-add-symbolic")
            .tooltip_text("Pair a New Device")
            .build();
        pair_button.connect_clicked({
            let sender = sender.clone();
            move |_| sender.input(Input::ShowQr)
        });
        let menu = gtk::gio::Menu::new();
        let section = gtk::gio::Menu::new();
        section.append(Some("Settings"), Some("app.settings"));
        section.append(Some("Commands"), Some("app.commands"));
        section.append(Some("PC Notifications"), Some("app.notification-apps"));
        menu.append_section(None, &section);
        let section = gtk::gio::Menu::new();
        section.append(Some("About Pairly"), Some("app.about"));
        menu.append_section(None, &section);
        let menu_button = gtk::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .tooltip_text("Main Menu")
            .menu_model(&menu)
            .primary(true)
            .build();
        let sidebar_header = adw::HeaderBar::new();
        sidebar_header.set_title_widget(Some(&title));
        sidebar_header.pack_start(&pair_button);
        sidebar_header.pack_end(&menu_button);
        let list = gtk::ListBox::new();
        list.add_css_class("navigation-sidebar");
        list.connect_row_activated({
            let sender = sender.clone();
            move |_, row| {
                let id = row.widget_name();
                if !id.is_empty() {
                    sender.input(Input::Select(id.to_string()));
                }
            }
        });
        let sidebar_view = adw::ToolbarView::new();
        sidebar_view.add_top_bar(&sidebar_header);
        sidebar_view.set_content(Some(
            &gtk::ScrolledWindow::builder()
                .child(&list)
                .vexpand(true)
                .build(),
        ));
        // The global switch: off stops Pairly entirely until it's switched back on.
        let power_label = gtk::Label::builder().xalign(0.0).hexpand(true).build();
        power_label.add_css_class("heading");
        let power = gtk::Switch::builder()
            .valign(gtk::Align::Center)
            .tooltip_text("Turn Pairly on or off")
            .active(!prefs.off)
            .build();
        power.connect_state_set({
            let sender = sender.clone();
            move |_, on| {
                sender.input(Input::SetOn(on));
                gtk::glib::Propagation::Proceed
            }
        });
        let power_bar = gtk::Box::builder()
            .spacing(12)
            .margin_top(10)
            .margin_bottom(10)
            .margin_start(16)
            .margin_end(16)
            .build();
        power_bar.append(&power_label);
        power_bar.append(&power);
        sidebar_view.add_bottom_bar(&power_bar);

        // Content: a status page or the selected device.
        let status_page = adw::StatusPage::new();
        let retry = gtk::Button::builder()
            .label("Pair a Device")
            .halign(gtk::Align::Center)
            .build();
        retry.add_css_class("pill");
        retry.add_css_class("suggested-action");
        retry.connect_clicked({
            let sender = sender.clone();
            move |_| sender.input(Input::ShowQr)
        });
        let turn_on = gtk::Button::builder()
            .label("Turn On")
            .halign(gtk::Align::Center)
            .build();
        turn_on.add_css_class("pill");
        turn_on.add_css_class("suggested-action");
        turn_on.connect_clicked({
            let sender = sender.clone();
            move |_| sender.input(Input::SetOn(true))
        });
        let status_buttons = gtk::Box::builder().halign(gtk::Align::Center).build();
        status_buttons.append(&retry);
        status_buttons.append(&turn_on);
        status_page.set_child(Some(&status_buttons));
        let device_slot = adw::Bin::new();
        device_slot.add_css_class("drop-zone");
        device_slot.add_controller(file_drop_target({
            let sender = sender.clone();
            move |paths| sender.input(Input::DropFiles(paths))
        }));
        let content = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::Crossfade)
            .build();
        content.add_named(&status_page, Some("status"));
        content.add_named(&device_slot, Some("device"));
        let split = adw::OverlaySplitView::builder()
            .min_sidebar_width(280.0)
            .max_sidebar_width(340.0)
            .show_sidebar(prefs.sidebar)
            .build();
        // Shows and hides the device list (F9, as in other GNOME apps).
        let sidebar_toggle = gtk::ToggleButton::builder()
            .icon_name("sidebar-show-symbolic")
            .tooltip_text("Show or Hide the Device List (F9)")
            .build();
        split
            .bind_property("show-sidebar", &sidebar_toggle, "active")
            .bidirectional()
            .sync_create()
            .build();
        split.connect_show_sidebar_notify(|split| {
            // Only a choice made on a wide window is remembered; narrow ones hide it anyway.
            if !split.is_collapsed() {
                let mut prefs = crate::settings::Prefs::load();
                prefs.sidebar = split.shows_sidebar();
                prefs.save();
            }
        });
        let content_title = adw::WindowTitle::new(APP_TITLE, "");
        let content_header = adw::HeaderBar::new();
        content_header.set_title_widget(Some(&content_title));
        content_header.pack_start(&sidebar_toggle);
        let content_view = adw::ToolbarView::new();
        content_view.add_top_bar(&content_header);
        content_view.set_content(Some(&content));
        split.set_sidebar(Some(&sidebar_view));
        split.set_content(Some(&content_view));
        let toasts = adw::ToastOverlay::new();
        toasts.set_child(Some(&split));
        window.set_content(Some(&toasts));

        // Narrow windows: the list slides over the page instead of sitting beside it.
        let narrow = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
            adw::BreakpointConditionLengthType::MaxWidth,
            600.0,
            adw::LengthUnit::Sp,
        ));
        narrow.add_setter(&split, "collapsed", Some(&true.to_value()));
        window.add_breakpoint(narrow);

        let qr_picture = gtk::Picture::builder()
            .can_shrink(false)
            .halign(gtk::Align::Center)
            .build();

        // `app.pair` opens the QR dialog (also reachable as
        // `gapplication action io.github.abhilesh1412.Pairly pair`, e.g. from the tray).
        let pair_action = gtk::gio::SimpleAction::new("pair", None);
        pair_action.connect_activate({
            let sender = sender.clone();
            move |_, _| sender.input(Input::ShowQr)
        });
        let app = relm4::main_application();
        app.add_action(&pair_action);
        let quit_action = gtk::gio::SimpleAction::new("quit", None);
        quit_action.connect_activate({
            let window = window.clone();
            move |_, _| window.close()
        });
        app.add_action(&quit_action);
        let toggle_sidebar = gtk::gio::SimpleAction::new("toggle-sidebar", None);
        toggle_sidebar.connect_activate({
            let split = split.clone();
            move |_, _| split.set_show_sidebar(!split.shows_sidebar())
        });
        app.add_action(&toggle_sidebar);
        for (name, input) in [
            ("settings", (|| Input::OpenSettings) as fn() -> Input),
            ("commands", || Input::OpenCommands),
            ("notification-apps", || Input::OpenNotificationApps),
            ("about", || Input::About),
        ] {
            let action = gtk::gio::SimpleAction::new(name, None);
            let sender = sender.clone();
            action.connect_activate(move |_, _| sender.input(input()));
            app.add_action(&action);
        }
        app.set_accels_for_action("app.quit", &["<Control>q"]);
        app.set_accels_for_action("app.settings", &["<Control>comma"]);
        app.set_accels_for_action("app.toggle-sidebar", &["F9"]);
        app.set_accels_for_action("window.close", &["<Control>w"]);

        sender.command(|out, shutdown| shutdown.register(watch_daemon(out)).drop_on_shutdown());

        let model = App {
            daemon: None,
            show_pairing_when_ready: init.show_pairing,
            status: if prefs.off {
                Status::Off
            } else {
                Status::Connecting
            },
            me: None,
            devices: Vec::new(),
            selected: None,
            qr_dialog: None,
            ringing: std::collections::HashSet::new(),
            transfers: BTreeMap::new(),
            players: HashMap::new(),
            off: prefs.off,
        };
        let mut widgets = Widgets {
            toasts,
            split,
            title,
            list,
            content_title,
            content,
            device_slot,
            status_page,
            retry,
            turn_on,
            power,
            power_label,
            qr_picture,
            transfer_rows: HashMap::new(),
        };
        model.render(&mut widgets, &sender);
        ComponentParts { model, widgets }
    }

    fn update_with_view(
        &mut self,
        widgets: &mut Self::Widgets,
        message: Self::Input,
        sender: ComponentSender<Self>,
        window: &Self::Root,
    ) {
        match message {
            Input::Select(id) => {
                self.refresh_players(&sender, id.clone());
                self.selected = Some(id);
                // On a narrow window the list covers the page: get it out of the way.
                if widgets.split.is_collapsed() {
                    widgets.split.set_show_sidebar(false);
                }
            }
            Input::Pair(id) => {
                self.call(&sender, move |d| async move { d.request_pair(&id).await })
            }
            Input::Ping(id) => {
                let name = self.name_of(&id);
                self.call(&sender, move |d| async move { d.ping(&id, "").await });
                Self::toast(widgets, &format!("Pinged {name}"));
            }
            Input::Ring(id, on) => {
                if on {
                    self.ringing.insert(id.clone());
                } else {
                    self.ringing.remove(&id);
                }
                self.call(&sender, move |d| async move { d.ring(&id, on).await });
            }
            Input::Power(id, action) => {
                let name = self.name_of(&id);
                self.call(
                    &sender,
                    move |d| async move { d.phone_power(&id, action).await },
                );
                let what = match action {
                    "lock" => "Locking",
                    "restart" => "Restarting",
                    _ => "Powering off",
                };
                Self::toast(widgets, &format!("{what} {name}…"));
            }
            Input::AskPower(id) => {
                let dialog = adw::AlertDialog::new(
                    Some(&format!("Power Off {}?", self.name_of(&id))),
                    Some("You'll need to turn it back on at the phone."),
                );
                dialog.add_responses(&[
                    ("cancel", "Cancel"),
                    ("restart", "Restart"),
                    ("poweroff", "Power Off"),
                ]);
                dialog.set_response_appearance("poweroff", adw::ResponseAppearance::Destructive);
                dialog.set_close_response("cancel");
                let sender = sender.clone();
                dialog.connect_response(None, move |_, response| {
                    let action = match response {
                        "poweroff" => "poweroff",
                        "restart" => "restart",
                        _ => return,
                    };
                    sender.input(Input::Power(id.clone(), action));
                });
                dialog.present(Some(window));
            }
            Input::SendClipboard(id) => {
                let name = self.name_of(&id);
                self.call(&sender, move |d| async move { d.send_clipboard(&id).await });
                Self::toast(widgets, &format!("Sent the clipboard to {name}"));
            }
            Input::PickFiles(id) => {
                let sender = sender.clone();
                gtk::FileDialog::builder()
                    .title(format!("Send Files to {}", self.name_of(&id)))
                    .accept_label("Send")
                    .build()
                    .open_multiple(Some(window), gtk::gio::Cancellable::NONE, move |res| {
                        if let Ok(files) = res {
                            let paths = local_paths(&files);
                            if !paths.is_empty() {
                                sender.input(Input::SendFiles(id, paths));
                            }
                        }
                    });
                return;
            }
            Input::DropFiles(paths) => {
                match self.send_target() {
                    Some(d) => sender.input(Input::SendFiles(d.id.clone(), paths)),
                    None => Self::toast(widgets, "Connect the device to send files"),
                }
                return;
            }
            Input::SendFiles(id, paths) => {
                let Some(daemon) = self.daemon.clone() else {
                    return;
                };
                let name = self.name_of(&id);
                let count = paths.len();
                sender.oneshot_command(async move {
                    let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
                    match daemon.send_files(&id, &refs).await {
                        Ok(_) => Cmd::Done,
                        Err(e) => Cmd::Failed(format!("Couldn't send: {}", describe(&e))),
                    }
                });
                let what = if count == 1 {
                    "1 file".to_owned()
                } else {
                    format!("{count} files")
                };
                Self::toast(widgets, &format!("Sending {what} to {name}"));
                return;
            }
            Input::AskText(id) => {
                text_dialog(&self.name_of(&id), id, &sender).present(Some(window));
                return;
            }
            Input::SendText(id, text) => {
                let url = is_link(&text);
                let name = self.name_of(&id);
                self.call(&sender, move |d| async move {
                    d.send_text(&id, &text, url).await
                });
                let what = if url { "the link" } else { "the text" };
                Self::toast(widgets, &format!("Sent {what} to {name}"));
                return;
            }
            Input::OpenMessages(id) => {
                if let Some(daemon) = self.daemon.clone() {
                    let name = self.name_of(&id);
                    crate::messages::open(window, daemon, id, &name);
                }
                return;
            }
            Input::ShowScreen(id) => {
                let name = self.name_of(&id);
                self.call(
                    &sender,
                    move |d| async move { d.show_phone_screen(&id).await },
                );
                Self::toast(widgets, &format!("Asking {name} to share its screen…"));
            }
            Input::OpenFiles(id) => {
                if let Some(daemon) = self.daemon.clone() {
                    let name = self.name_of(&id);
                    crate::files::open(window, daemon, id, &name);
                }
                return;
            }
            Input::OpenContacts(id) => {
                if let Some(daemon) = self.daemon.clone() {
                    let name = self.name_of(&id);
                    crate::contacts::open(window, daemon, id, &name);
                }
                return;
            }
            Input::OpenCommands => {
                if let Some(daemon) = self.daemon.clone() {
                    crate::commands::open(window, daemon);
                }
                return;
            }
            Input::OpenNotificationApps => {
                if let Some(daemon) = self.daemon.clone() {
                    crate::notification_apps::open(window, daemon);
                }
                return;
            }
            Input::OpenSettings => {
                let (commands, apps) = (sender.clone(), sender.clone());
                crate::settings::open(
                    window,
                    self.daemon.clone(),
                    self.me.clone(),
                    move || commands.input(Input::OpenCommands),
                    move || apps.input(Input::OpenNotificationApps),
                );
                return;
            }
            Input::SetOn(on) => {
                if on != self.off {
                    return; // already so (the switch echoing a change made here)
                }
                self.off = !on;
                crate::settings::set_off(self.off);
                let mut prefs = crate::settings::Prefs::load();
                prefs.off = self.off;
                prefs.save();
                let daemon = self.daemon.clone();
                if self.off {
                    self.daemon = None;
                    self.status = Status::Off;
                    self.devices.clear();
                    self.selected = None;
                } else {
                    self.status = Status::Connecting;
                }
                sender.oneshot_command(async move {
                    if crate::settings::switch_service(on).await.is_err() && !on {
                        // Not run by systemd: stop it directly (it starts on demand later).
                        if let Some(daemon) = daemon {
                            let _ = daemon.quit().await;
                        }
                    }
                    Cmd::Done
                });
            }
            Input::About => {
                crate::settings::about_dialog().present(Some(window));
                return;
            }
            Input::Media(device, player, action) => {
                self.call(&sender, move |d| async move {
                    d.media_control(&device, &player, action, 0).await
                });
                return;
            }
            Input::AcceptTransfer(t) => {
                self.call(&sender, move |d| async move { d.accept_transfer(t).await });
                return;
            }
            Input::CancelTransfer(t) => {
                self.call(&sender, move |d| async move { d.cancel_transfer(t).await });
                return;
            }
            Input::SetPaused(id, paused) => {
                let name = self.name_of(&id);
                self.call(
                    &sender,
                    move |d| async move { d.set_paused(&id, paused).await },
                );
                Self::toast(
                    widgets,
                    &if paused {
                        format!("Paused {name}: nothing passes either way until you resume it")
                    } else {
                        format!("Resumed {name}")
                    },
                );
                return;
            }
            Input::Unpair(id) => {
                self.selected = None;
                self.call(&sender, move |d| async move { d.unpair(&id).await });
            }
            Input::ShowQr => {
                if self.qr_dialog.is_none() && self.daemon.is_some() {
                    let dialog = qr_dialog(&widgets.qr_picture, &sender);
                    dialog.present(Some(window));
                    self.qr_dialog = Some(dialog);
                    self.request_qr(&sender);
                }
                return;
            }
            Input::RefreshQr => {
                if self.qr_dialog.is_some() {
                    self.request_qr(&sender);
                }
                return;
            }
            Input::QrClosed => {
                self.qr_dialog = None;
                widgets.qr_picture.set_paintable(gtk::gdk::Paintable::NONE);
                self.call(&sender, |d| async move { d.cancel_qr_pairing().await });
                return;
            }
        }
        self.render(widgets, &sender);
    }

    fn update_cmd_with_view(
        &mut self,
        widgets: &mut Self::Widgets,
        message: Self::CommandOutput,
        sender: ComponentSender<Self>,
        window: &Self::Root,
    ) {
        match message {
            Cmd::Connected(..) | Cmd::Unavailable(_) if self.off => return,
            Cmd::Connected(daemon, me) => {
                let check = daemon.clone();
                sender.oneshot_command(async move {
                    match check.update_status().await {
                        Ok((version, ..)) if version != crate::settings::VERSION => {
                            Cmd::NewVersion(version)
                        }
                        _ => Cmd::Done,
                    }
                });
                self.daemon = Some(daemon);
                self.me = Some(me);
                self.status = Status::Ready;
                self.refresh(&sender);
                self.refresh_transfers(&sender);
                if std::mem::take(&mut self.show_pairing_when_ready) {
                    sender.input(Input::ShowQr);
                }
            }
            Cmd::Unavailable(reason) => {
                self.daemon = None;
                self.status = Status::Unavailable(reason);
                self.devices.clear();
            }
            Cmd::Devices(devices) => {
                for d in devices.iter().filter(|d| d.paired && d.is_connected()) {
                    self.refresh_players(&sender, d.id.clone());
                }
                self.devices = devices;
                let known = self
                    .selected
                    .as_deref()
                    .is_some_and(|id| self.device(id).is_some());
                if !known {
                    // Show something useful instead of an empty page: the first paired device.
                    self.selected = self.devices.iter().find(|d| d.paired).map(|d| d.id.clone());
                }
            }
            Cmd::Changed => {
                self.refresh(&sender);
                return;
            }
            Cmd::PairingRequested { id, name, code } => {
                code_dialog(&id, &name, &code, &sender, self.daemon.clone()).present(Some(window));
                return;
            }
            Cmd::PairingFinished {
                id,
                success,
                message,
            } => {
                let name = self.name_of(&id);
                if success {
                    if let Some(dialog) = self.qr_dialog.take() {
                        dialog.close();
                    }
                    self.selected = Some(id);
                    Self::toast(widgets, &format!("Paired with {name}"));
                    self.refresh(&sender);
                } else {
                    Self::toast(widgets, &format!("Pairing with {name} failed: {message}"));
                }
            }
            Cmd::PingReceived { name, message } => {
                let text = if message.is_empty() {
                    format!("Ping from {name}")
                } else {
                    format!("{name}: {message}")
                };
                Self::toast(widgets, &text);
                return;
            }
            Cmd::Qr(result) => {
                match result {
                    Ok((uri, valid_secs)) => {
                        widgets
                            .qr_picture
                            .set_paintable(crate::qr::texture(&uri).as_ref());
                        // Swap in a fresh code shortly before this one expires.
                        let sender = sender.clone();
                        let refresh_in =
                            Duration::from_secs(u64::from(valid_secs.saturating_sub(10).max(10)));
                        gtk::glib::timeout_add_local_once(refresh_in, move || {
                            sender.input(Input::RefreshQr)
                        });
                    }
                    Err(e) => Self::toast(widgets, &format!("Couldn't create a pairing code: {e}")),
                }
                return;
            }
            Cmd::PlayersChanged(device) => {
                self.refresh_players(&sender, device);
                return;
            }
            Cmd::Players(device, players) => {
                self.players.insert(device, players);
            }
            Cmd::Transfers(transfers) => {
                self.transfers = transfers.into_iter().map(|t| (t.id, t)).collect();
            }
            Cmd::TransferChanged(t) => {
                if t.is_finished() {
                    self.transfers.remove(&t.id);
                    transfer_toast(widgets, window, &t);
                } else {
                    let same_state = self
                        .transfers
                        .get(&t.id)
                        .is_some_and(|old| old.state == t.state);
                    let row = widgets.transfer_rows.get(&t.id).cloned();
                    self.transfers.insert(t.id, t.clone());
                    // Progress only: update the row instead of rebuilding the page.
                    if same_state && let Some((row, bar)) = row {
                        row.set_subtitle(&transfer_subtitle(&t));
                        bar.set_fraction(t.fraction());
                        return;
                    }
                }
            }
            Cmd::Failed(e) => {
                Self::toast(widgets, &e);
                return;
            }
            Cmd::NewVersion(version) => {
                let toast = adw::Toast::builder()
                    .title(format!("Pairly was updated to {version}"))
                    .button_label("Restart")
                    .timeout(0)
                    .build();
                toast.connect_button_clicked(|_| restart_app());
                widgets.toasts.add_toast(toast);
                return;
            }
            Cmd::Done => return,
        }
        self.render(widgets, &sender);
    }
}

impl App {
    fn render(&self, w: &mut Widgets, sender: &ComponentSender<Self>) {
        w.power.set_active(!self.off);
        w.power_label.set_label(if self.off {
            "Pairly is off"
        } else {
            "Pairly is on"
        });
        w.title
            .set_subtitle(self.me.as_ref().map_or("", |(_, name)| name.as_str()));

        // Sidebar.
        while let Some(row) = w.list.row_at_index(0) {
            w.list.remove(&row);
        }
        let paired: Vec<&Device> = self.devices.iter().filter(|d| d.paired).collect();
        let nearby: Vec<&Device> = self.devices.iter().filter(|d| !d.paired).collect();
        for (heading, group) in [("Your Devices", &paired), ("Nearby", &nearby)] {
            if group.is_empty() {
                continue;
            }
            w.list.append(&section_header(heading));
            for d in group {
                let row = device_row(d, sender);
                w.list.append(&row);
                if self.selected.as_deref() == Some(d.id.as_str()) {
                    w.list.select_row(Some(&row));
                }
            }
        }

        // Content.
        let selected = self.selected.as_deref().and_then(|id| self.device(id));
        match (&self.status, selected) {
            (Status::Ready, Some(device)) => {
                w.content_title.set_title(&device.name);
                w.content_title.set_subtitle("");
                let ringing = self.ringing.contains(&device.id);
                let transfers: Vec<&Transfer> = self
                    .transfers
                    .values()
                    .filter(|t| t.device == device.id)
                    .collect();
                w.transfer_rows.clear();
                let players = self.players.get(&device.id).map_or(&[][..], Vec::as_slice);
                w.device_slot.set_child(Some(&device_page(
                    device,
                    ringing,
                    players,
                    &transfers,
                    &mut w.transfer_rows,
                    sender,
                )));
                w.content.set_visible_child_name("device");
            }
            (status, _) => {
                w.content_title.set_title(APP_TITLE);
                w.content_title.set_subtitle("");
                let (icon, title, body, show_button) = match status {
                    Status::Off => (
                        "system-shutdown-symbolic",
                        "Pairly Is Off",
                        "Your devices can't reach this PC and nothing is shared. Turn it on when you need it.".to_owned(),
                        false,
                    ),
                    Status::Connecting => (
                        "content-loading-symbolic",
                        "Connecting…",
                        String::new(),
                        false,
                    ),
                    Status::Unavailable(reason) => (
                        "dialog-warning-symbolic",
                        "Pairly Isn't Running",
                        format!(
                            "Start the background service with “systemctl --user start pairlyd”.\n{reason}"
                        ),
                        false,
                    ),
                    Status::Ready if self.devices.iter().any(|d| d.paired) => {
                        ("phone-symbolic", "Select a Device", String::new(), false)
                    }
                    Status::Ready => (
                        "phone-symbolic",
                        "No Devices Yet",
                        "Pair your phone by scanning a code with the Pairly app.".to_owned(),
                        true,
                    ),
                };
                w.status_page.set_icon_name(Some(icon));
                w.status_page.set_title(title);
                w.status_page.set_description(Some(&body));
                w.retry.set_visible(show_button);
                w.turn_on.set_visible(matches!(status, Status::Off));
                w.content.set_visible_child_name("status");
            }
        }
    }
}

fn section_header(text: &str) -> gtk::ListBoxRow {
    let label = gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .margin_top(12)
        .margin_start(6)
        .build();
    label.add_css_class("heading");
    label.add_css_class("dim-label");
    gtk::ListBoxRow::builder()
        .child(&label)
        .selectable(false)
        .activatable(false)
        .build()
}

fn device_row(d: &Device, sender: &ComponentSender<App>) -> gtk::ListBoxRow {
    let icon = gtk::Image::from_icon_name(icon_for(&d.device_type));
    icon.set_pixel_size(18);
    icon.set_valign(gtk::Align::Center);
    icon.add_css_class("row-avatar");
    let name = gtk::Label::builder()
        .label(&d.name)
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    let status = status_pill(d, true);
    let text = gtk::Box::new(gtk::Orientation::Vertical, 4);
    text.append(&name);
    text.append(&status);
    let row_box = gtk::Box::builder()
        .spacing(12)
        .margin_top(6)
        .margin_bottom(6)
        .build();
    row_box.append(&icon);
    row_box.append(&text);
    let row = gtk::ListBoxRow::builder().child(&row_box).build();
    row.set_widget_name(&d.id);
    if d.paired && d.is_connected() {
        row.add_controller(file_drop_target({
            let (sender, id) = (sender.clone(), d.id.clone());
            move |paths| sender.input(Input::SendFiles(id.clone(), paths))
        }));
    }
    row
}

/// Accept files dragged in from a file manager.
///
/// The plain list of file links (`text/uri-list`) is read first: GTK's own file-list format
/// goes through the desktop's document portal, and when that service isn't running the drop
/// fails silently. The file list is only the fallback for sources without links.
fn file_drop_target(on_drop: impl Fn(Vec<String>) + 'static) -> gtk::DropTargetAsync {
    use gtk::gdk;
    let formats = gdk::ContentFormatsBuilder::new()
        .add_mime_type(URI_LIST)
        .add_type(gdk::FileList::static_type())
        .build();
    let target = gtk::DropTargetAsync::new(Some(formats), gdk::DragAction::COPY);
    // Always a copy: the file manager must never delete the original.
    target.connect_drag_enter(|_, _, _, _| gdk::DragAction::COPY);
    target.connect_drag_motion(|_, _, _, _| gdk::DragAction::COPY);
    let on_drop = std::rc::Rc::new(on_drop);
    target.connect_drop(move |_, drop, _, _| {
        let (drop, on_drop) = (drop.clone(), on_drop.clone());
        gtk::glib::spawn_future_local(async move {
            let paths = dropped_paths(&drop).await;
            if paths.is_empty() {
                drop.finish(gdk::DragAction::empty());
            } else {
                drop.finish(gdk::DragAction::COPY);
                on_drop(paths);
            }
        });
        true
    });
    target
}

const URI_LIST: &str = "text/uri-list";

/// The local files in a drop: from its link list if it has one, else its file list.
async fn dropped_paths(drop: &gtk::gdk::Drop) -> Vec<String> {
    use gtk::gio::prelude::*;
    let local = |files: Vec<gtk::gio::File>| -> Vec<String> {
        files
            .iter()
            .filter_map(gtk::gio::File::path)
            .map(|p| p.to_string_lossy().into_owned())
            .collect()
    };
    if drop.formats().contain_mime_type(URI_LIST)
        && let Ok((stream, _)) = drop
            .read_future(&[URI_LIST], gtk::glib::Priority::DEFAULT)
            .await
    {
        let mut text = Vec::new();
        loop {
            match stream
                .read_bytes_future(64 * 1024, gtk::glib::Priority::DEFAULT)
                .await
            {
                Ok(chunk) if !chunk.is_empty() => text.extend_from_slice(&chunk),
                _ => break,
            }
        }
        let files: Vec<gtk::gio::File> = String::from_utf8_lossy(&text)
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(gtk::gio::File::for_uri)
            .collect();
        let paths = local(files);
        if !paths.is_empty() {
            return paths;
        }
    }
    match drop
        .read_value_future(
            gtk::gdk::FileList::static_type(),
            gtk::glib::Priority::DEFAULT,
        )
        .await
    {
        Ok(value) => value
            .get::<gtk::gdk::FileList>()
            .map(|list| local(list.files()))
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

fn local_paths(files: &gtk::gio::ListModel) -> Vec<String> {
    (0..files.n_items())
        .filter_map(|i| files.item(i)?.downcast::<gtk::gio::File>().ok()?.path())
        .map(|p| p.to_string_lossy().into_owned())
        .collect()
}

/// Links open on arrival; anything else is copied.
fn is_link(text: &str) -> bool {
    let t = text.trim();
    (t.starts_with("https://") || t.starts_with("http://")) && !t.contains(char::is_whitespace)
}

fn human_size(bytes: u64) -> String {
    gtk::glib::format_size(bytes).to_string()
}

fn transfer_subtitle(t: &Transfer) -> String {
    match (t.state.as_str(), t.incoming) {
        ("waiting", true) => format!("{} · wants to send this", human_size(t.size)),
        ("waiting", false) => format!("Waiting for {} to accept", t.device_name),
        _ => format!(
            "{} {} of {} · {:.0}%",
            if t.incoming { "Receiving" } else { "Sending" },
            human_size(t.bytes),
            human_size(t.size),
            t.fraction() * 100.0
        ),
    }
}

fn transfer_toast(w: &Widgets, window: &adw::ApplicationWindow, t: &Transfer) {
    let toast = match (t.state.as_str(), t.incoming) {
        ("done", true) => {
            let toast = adw::Toast::new(&format!("Received {}", t.name));
            if !t.path.is_empty() {
                toast.set_button_label(Some("Open"));
                let (window, path) = (window.clone(), t.path.clone());
                toast.connect_button_clicked(move |_| {
                    gtk::FileLauncher::new(Some(&gtk::gio::File::for_path(&path))).launch(
                        Some(&window),
                        gtk::gio::Cancellable::NONE,
                        |_| {},
                    );
                });
            }
            toast
        }
        ("done", false) => adw::Toast::new(&format!("Sent {} to {}", t.name, t.device_name)),
        ("failed", _) => adw::Toast::new(&format!("{} failed: {}", t.name, t.error)),
        _ => adw::Toast::new(&format!("{} was cancelled", t.name)),
    };
    w.toasts.add_toast(toast);
}

fn device_page(
    d: &Device,
    ringing: bool,
    players: &[Player],
    transfers: &[&Transfer],
    rows: &mut HashMap<u64, (adw::ActionRow, gtk::ProgressBar)>,
    sender: &ComponentSender<App>,
) -> gtk::Widget {
    let page = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(24)
        .margin_top(24)
        .margin_bottom(32)
        .margin_start(24)
        .margin_end(24)
        .build();
    page.append(&banner(d, sender));

    if d.paired && d.paused {
        let group = adw::PreferencesGroup::new();
        let row = adw::ActionRow::builder()
            .title("Paused")
            .subtitle(format!(
                "{} can't reach this PC and this PC can't reach it: no notifications, files, \
                 messages or control either way. It stays paired.",
                d.name
            ))
            .build();
        row.add_prefix(&gtk::Image::from_icon_name("media-playback-pause-symbolic"));
        let resume = gtk::Button::builder()
            .label("Resume")
            .valign(gtk::Align::Center)
            .build();
        resume.add_css_class("suggested-action");
        resume.add_css_class("pill");
        resume.connect_clicked({
            let (sender, id) = (sender.clone(), d.id.clone());
            move |_| sender.input(Input::SetPaused(id.clone(), false))
        });
        row.add_suffix(&resume);
        group.add(&row);
        page.append(&group);
    } else if d.paired {
        let (on, id) = (d.is_connected(), d.id.clone());
        let phone = d.device_type == "phone";
        let with = |f: fn(String) -> Input| {
            let id = id.clone();
            move || f(id.clone())
        };
        let mut tiles = Vec::new();
        if phone {
            tiles.push(tile(
                "phone-symbolic",
                "Phone Screen",
                "See and control it",
                "purple",
                on,
                sender,
                with(Input::ShowScreen),
            ));
            tiles.push(tile(
                "mail-unread-symbolic",
                "Messages",
                "Read and send texts",
                "teal",
                on,
                sender,
                with(Input::OpenMessages),
            ));
            tiles.push(tile(
                "x-office-address-book-symbolic",
                "Contacts & Calls",
                "Call or text from here",
                "coral",
                on,
                sender,
                with(Input::OpenContacts),
            ));
            tiles.push(tile(
                "folder-symbolic",
                "Browse Files",
                "The phone's storage",
                "amber",
                on,
                sender,
                with(Input::OpenFiles),
            ));
        }
        tiles.push(tile(
            "document-send-symbolic",
            "Send Files",
            "Or drop them on this page",
            "blue",
            on,
            sender,
            with(Input::PickFiles),
        ));
        tiles.push(tile(
            "insert-link-symbolic",
            "Link or Text",
            "Opens or copies there",
            "blue",
            on,
            sender,
            with(Input::AskText),
        ));
        tiles.push(tile(
            "edit-paste-symbolic",
            "Clipboard",
            "Send what you copied",
            "pink",
            on,
            sender,
            with(Input::SendClipboard),
        ));
        let ring = if ringing {
            tile(
                "find-location-symbolic",
                "Stop Ringing",
                "Ringing now…",
                "red",
                on,
                sender,
                with(|id| Input::Ring(id, false)),
            )
        } else {
            tile(
                "find-location-symbolic",
                if phone { "Find My Phone" } else { "Ring It" },
                "Rings, even on silent",
                "green",
                on,
                sender,
                with(|id| Input::Ring(id, true)),
            )
        };
        tiles.push(ring);
        tiles.push(tile(
            "preferences-system-notifications-symbolic",
            "Ping",
            "Show a notification",
            "green",
            on,
            sender,
            with(Input::Ping),
        ));
        if phone {
            tiles.push(tile(
                "system-lock-screen-symbolic",
                "Lock Phone",
                "Lock its screen now",
                "gray",
                on,
                sender,
                with(|id| Input::Power(id, "lock")),
            ));
            tiles.push(tile(
                "system-shutdown-symbolic",
                "Power Off",
                "Power off or restart",
                "red",
                on,
                sender,
                with(Input::AskPower),
            ));
        }
        tiles.push(tile(
            "utilities-terminal-symbolic",
            "Commands",
            "What it may run here",
            "gray",
            true,
            sender,
            || Input::OpenCommands,
        ));

        // 2 tiles a row on narrow windows, up to 6 on wide ones.
        let grid = gtk::FlowBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .homogeneous(true)
            .min_children_per_line(2)
            .max_children_per_line(6)
            .column_spacing(12)
            .row_spacing(12)
            .build();
        for button in tiles {
            grid.append(&button);
            if let Some(child) = button.parent() {
                child.set_focusable(false);
            }
        }
        page.append(&grid);
    } else {
        let actions = adw::PreferencesGroup::new();
        let pair_button = gtk::Button::builder()
            .label("Pair")
            .valign(gtk::Align::Center)
            .build();
        pair_button.add_css_class("suggested-action");
        pair_button.add_css_class("pill");
        pair_button.connect_clicked({
            let (sender, id) = (sender.clone(), d.id.clone());
            move |_| sender.input(Input::Pair(id.clone()))
        });
        let pair = adw::ActionRow::builder()
            .title("Pair with a Code")
            .subtitle("Both screens will show the same 6-digit code to compare")
            .build();
        pair.add_suffix(&pair_button);
        actions.add(&pair);
        page.append(&actions);
    }

    if let Some(p) = players.iter().find(|p| p.playing).or(players.first())
        && d.is_connected()
    {
        page.append(&now_playing(&d.id, p, sender));
    }

    if !transfers.is_empty() {
        let group = adw::PreferencesGroup::builder().title("Transfers").build();
        for t in transfers {
            let (row, bar) = transfer_row(t, sender);
            rows.insert(t.id, (row.clone(), bar));
            group.add(&row);
        }
        page.append(&group);
    }

    let details = adw::PreferencesGroup::builder().title("Details").build();
    let id_row = adw::ActionRow::builder()
        .title("Device ID")
        .subtitle(&d.id)
        .subtitle_selectable(true)
        .build();
    id_row.add_css_class("property");
    details.add(&id_row);
    page.append(&details);

    if d.paired {
        let danger = adw::PreferencesGroup::new();
        let unpair = adw::ButtonRow::builder().title("Unpair").build();
        unpair.add_css_class("destructive-action");
        unpair.connect_activated({
            let (sender, id, name) = (sender.clone(), d.id.clone(), d.name.clone());
            move |row| {
                let dialog = adw::AlertDialog::new(
                    Some(&format!("Unpair {name}?")),
                    Some("You'll need to pair again to reconnect."),
                );
                dialog.add_responses(&[("cancel", "Cancel"), ("unpair", "Unpair")]);
                dialog.set_response_appearance("unpair", adw::ResponseAppearance::Destructive);
                dialog.set_close_response("cancel");
                let (sender, id) = (sender.clone(), id.clone());
                dialog.connect_response(Some("unpair"), move |_, _| {
                    sender.input(Input::Unpair(id.clone()))
                });
                dialog.present(Some(row));
            }
        });
        danger.add(&unpair);
        page.append(&danger);
    }

    // Wide windows fit more tiles per row; very wide ones stop growing.
    let clamp = adw::Clamp::builder()
        .maximum_size(1280)
        .tightening_threshold(900)
        .child(&page)
        .build();
    gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&clamp)
        .build()
        .upcast()
}

/// The top of a device page: a drawn phone (or laptop) showing its battery, the name, and tags
/// for the connection, on a soft lavender banner.
fn banner(d: &Device, sender: &ComponentSender<App>) -> gtk::Box {
    let name = gtk::Label::builder()
        .label(&d.name)
        .xalign(0.0)
        .wrap(true)
        .build();
    name.add_css_class("title-1");
    let tags = gtk::Box::builder().spacing(6).build();
    let tag = |text: &str, kind: &str| {
        let label = gtk::Label::builder()
            .label(text)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        label.add_css_class("tag");
        label.add_css_class(kind);
        tags.append(&label);
    };
    match (d.paired, d.is_connected()) {
        (false, _) => tag("Not paired", "info"),
        (true, _) if d.paused => tag("❚❚ Paused", "offline"),
        (true, false) => tag("● Offline", "offline"),
        (true, true) => {
            tag("● Connected", "connected");
            let link = if d.rtt_ms > 0 {
                format!("{} · {} ms", link_name(&d.link), d.rtt_ms)
            } else {
                link_name(&d.link).to_owned()
            };
            tag(&link, "info");
        }
    }
    if d.battery >= 0 && d.charging {
        tag("Charging", "connected");
    } else if (0..=15).contains(&d.battery) {
        tag("Battery low", "offline");
    }
    let text = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(10)
        .valign(gtk::Align::Center)
        .hexpand(true)
        .build();
    text.append(&name);
    text.append(&tags);

    let banner = gtk::Box::builder().spacing(24).build();
    banner.add_css_class("device-banner");
    banner.append(&device_art(&d.device_type, d.battery, d.charging));
    banner.append(&text);
    if d.paired {
        // Pausing cuts the connection both ways (enforced by the daemon, not just here).
        let (icon, tip) = if d.paused {
            (
                "media-playback-start-symbolic",
                "Resume: allow this device again",
            )
        } else {
            (
                "media-playback-pause-symbolic",
                "Pause: no connection either way until resumed",
            )
        };
        let toggle = gtk::Button::builder()
            .icon_name(icon)
            .tooltip_text(tip)
            .valign(gtk::Align::Center)
            .build();
        toggle.add_css_class("circular");
        toggle.add_css_class("pause-toggle");
        toggle.connect_clicked({
            let (sender, id, paused) = (sender.clone(), d.id.clone(), d.paused);
            move |_| sender.input(Input::SetPaused(id.clone(), !paused))
        });
        banner.append(&toggle);
    }
    banner
}

/// A drawn phone or laptop with the battery level on its screen.
fn device_art(device_type: &str, battery: i32, charging: bool) -> gtk::Overlay {
    let laptop = !matches!(device_type, "phone" | "tablet");
    let (w, h) = if laptop { (96, 66) } else { (52, 88) };
    let art = gtk::DrawingArea::builder()
        .content_width(w)
        .content_height(h)
        .build();
    art.add_css_class("device-art");
    let level = (battery >= 0).then(|| f64::from(battery.clamp(0, 100)) / 100.0);
    art.set_draw_func(move |area, cr, w, h| {
        let c = area.color();
        let ink = |alpha: f64| {
            cr.set_source_rgba(
                f64::from(c.red()),
                f64::from(c.green()),
                f64::from(c.blue()),
                alpha,
            );
        };
        let (w, h) = (f64::from(w), f64::from(h));
        let rounded = |x: f64, y: f64, rw: f64, rh: f64, r: f64| {
            cr.new_sub_path();
            cr.arc(x + rw - r, y + r, r, -std::f64::consts::FRAC_PI_2, 0.0);
            cr.arc(x + rw - r, y + rh - r, r, 0.0, std::f64::consts::FRAC_PI_2);
            cr.arc(
                x + r,
                y + rh - r,
                r,
                std::f64::consts::FRAC_PI_2,
                std::f64::consts::PI,
            );
            cr.arc(
                x + r,
                y + r,
                r,
                std::f64::consts::PI,
                1.5 * std::f64::consts::PI,
            );
            cr.close_path();
        };
        cr.set_line_width(2.5);
        // The screen (and, for a laptop, its base).
        let (sx, sy, sw, sh) = if laptop {
            (8.0, 2.0, w - 16.0, h - 12.0)
        } else {
            (2.0, 2.0, w - 4.0, h - 4.0)
        };
        rounded(sx, sy, sw, sh, if laptop { 6.0 } else { 10.0 });
        ink(0.10);
        let _ = cr.fill_preserve();
        ink(1.0);
        let _ = cr.stroke();
        if laptop {
            rounded(1.0, h - 9.0, w - 2.0, 7.0, 3.5);
            ink(1.0);
            let _ = cr.fill();
        } else {
            // The earpiece.
            rounded(w / 2.0 - 7.0, 7.0, 14.0, 3.0, 1.5);
            ink(0.6);
            let _ = cr.fill();
        }
        // A battery bar along the bottom of the screen.
        if let Some(level) = level {
            let (bx, by, bw) = (sx + 8.0, sy + sh - 11.0, sw - 16.0);
            rounded(bx, by, bw, 5.0, 2.5);
            ink(0.2);
            let _ = cr.fill();
            if level > 0.0 {
                rounded(bx, by, (bw * level).max(5.0), 5.0, 2.5);
                if charging {
                    cr.set_source_rgb(0.23, 0.43, 0.07);
                } else if level <= 0.15 {
                    cr.set_source_rgb(0.64, 0.18, 0.18);
                } else {
                    ink(1.0);
                }
                let _ = cr.fill();
            }
        }
    });
    let overlay = gtk::Overlay::builder()
        .child(&art)
        .valign(gtk::Align::Center)
        .build();
    if battery >= 0 {
        let label = gtk::Label::new(Some(&format!("{battery}%")));
        label.add_css_class("device-art-level");
        label.set_valign(gtk::Align::Center);
        // Above the battery bar, in the screen's middle.
        label.set_margin_bottom(if laptop { 12 } else { 6 });
        overlay.add_overlay(&label);
        overlay.set_tooltip_text(Some(&if charging {
            format!("Battery {battery}% · charging")
        } else {
            format!("Battery {battery}%")
        }));
    }
    overlay
}

/// One action: an icon, a name and a hint on a soft pastel card (`hue`: purple, teal, coral,
/// amber, blue, pink, green, gray or red).
#[allow(clippy::too_many_arguments)]
fn tile(
    icon: &str,
    title: &str,
    hint: &str,
    hue: &str,
    enabled: bool,
    sender: &ComponentSender<App>,
    msg: impl Fn() -> Input + 'static,
) -> gtk::Button {
    let image = gtk::Image::from_icon_name(icon);
    image.set_pixel_size(24);
    image.set_halign(gtk::Align::Start);
    let name = gtk::Label::builder()
        .label(title)
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    name.add_css_class("tile-title");
    let hint_label = gtk::Label::builder()
        .label(hint)
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    hint_label.add_css_class("tile-hint");
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .valign(gtk::Align::Center)
        .build();
    content.append(&image);
    content.append(&name);
    content.append(&hint_label);
    let button = gtk::Button::builder()
        .child(&content)
        .sensitive(enabled)
        .tooltip_text(hint)
        .build();
    button.add_css_class("tile");
    button.add_css_class(&format!("hue-{hue}"));
    let sender = sender.clone();
    button.connect_clicked(move |_| sender.input(msg()));
    button
}

/// "● Connected · Local network · 3 ms" in green, "● Offline" in red, "● Available" in accent.
fn status_pill(d: &Device, short: bool) -> gtk::Box {
    let kind = match (d.paired, d.is_connected()) {
        (false, _) => "available",
        (true, _) if d.paused => "paused",
        (true, true) => "connected",
        (true, false) => "offline",
    };
    let dot = gtk::Box::builder().valign(gtk::Align::Center).build();
    dot.add_css_class("status-dot");
    let text = if short {
        match kind {
            "connected" => "Connected".to_owned(),
            "offline" => "Offline".to_owned(),
            "paused" => "Paused".to_owned(),
            _ => "Available".to_owned(),
        }
    } else {
        status_text(d)
    };
    let label = gtk::Label::builder()
        .label(text)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    let pill = gtk::Box::builder()
        .spacing(8)
        .halign(gtk::Align::Start)
        .build();
    pill.add_css_class("status-pill");
    pill.add_css_class(kind);
    if short {
        pill.add_css_class("compact");
    }
    pill.append(&dot);
    pill.append(&label);
    pill
}

/// The pastel look: soft colour cards, each action in its own gentle colour, and a lavender
/// banner with a drawn device. Shapes and spacing here; colours in [`LIGHT`] and [`DARK`].
const CSS: &str = "
.status-pill { padding: 4px 12px; border-radius: 999px; font-weight: bold; font-size: smaller; }
.status-pill .status-dot { min-width: 8px; min-height: 8px; border-radius: 999px; }
.status-pill.connected { background: alpha(@success_color, 0.15); color: @success_color; }
.status-pill.connected .status-dot { background: @success_color; }
.status-pill.offline { background: alpha(@error_color, 0.15); color: @error_color; }
.status-pill.offline .status-dot { background: @error_color; }
.status-pill.available { background: alpha(@accent_color, 0.15); color: @accent_color; }
.status-pill.available .status-dot { background: @accent_color; }
.status-pill.compact { padding: 2px 8px; }
.status-pill.paused { background: alpha(@warning_color, 0.18); color: @warning_color; }
.status-pill.paused .status-dot { background: @warning_color; }
.pause-toggle { min-width: 44px; min-height: 44px; }
.device-banner { border-radius: 26px; padding: 22px 28px; }
.device-art-level { font-weight: bold; font-size: 13px; }
.tag { border-radius: 999px; padding: 3px 10px; font-size: smaller; font-weight: bold; }
.row-avatar { min-width: 36px; min-height: 36px; border-radius: 999px; }
button.tile { border-radius: 20px; padding: 16px; min-height: 96px; min-width: 150px; box-shadow: none; transition: filter 150ms ease; }
button.tile:disabled { opacity: 0.45; }
.tile-title { font-weight: bold; }
.tile-hint { font-size: smaller; }
.drop-zone:drop(active) { outline: 2px dashed @accent_color; outline-offset: -10px; border-radius: 26px; }
";

/// Colours for the light look.
const LIGHT: &str = "
.device-banner { background-color: #EEEDFE; color: #26215C; }
.device-art { color: #3C3489; }
.tag { background-color: rgba(255,255,255,0.85); }
.tag.connected { color: #27500A; }
.tag.info { color: #3C3489; }
.tag.offline { color: #A32D2D; }
.row-avatar { background-color: #EEEDFE; color: #3C3489; }
button.tile:hover { filter: brightness(0.96); }
button.tile:active { filter: brightness(0.92); }
button.tile.hue-purple { background-color: #EEEDFE; color: #3C3489; }
button.tile.hue-purple .tile-hint { color: #534AB7; }
button.tile.hue-teal { background-color: #E1F5EE; color: #085041; }
button.tile.hue-teal .tile-hint { color: #0F6E56; }
button.tile.hue-coral { background-color: #FAECE7; color: #712B13; }
button.tile.hue-coral .tile-hint { color: #993C1D; }
button.tile.hue-amber { background-color: #FAEEDA; color: #633806; }
button.tile.hue-amber .tile-hint { color: #854F0B; }
button.tile.hue-blue { background-color: #E6F1FB; color: #0C447C; }
button.tile.hue-blue .tile-hint { color: #185FA5; }
button.tile.hue-pink { background-color: #FBEAF0; color: #72243E; }
button.tile.hue-pink .tile-hint { color: #993556; }
button.tile.hue-green { background-color: #EAF3DE; color: #27500A; }
button.tile.hue-green .tile-hint { color: #3B6D11; }
button.tile.hue-gray { background-color: #F1EFE8; color: #444441; }
button.tile.hue-gray .tile-hint { color: #5F5E5A; }
button.tile.hue-red { background-color: #FCEBEB; color: #791F1F; }
button.tile.hue-red .tile-hint { color: #A32D2D; }
";

/// Colours for the dark look (deep versions of the same hues, light text).
const DARK: &str = "
.device-banner { background-color: #3C3489; color: #EEEDFE; }
.device-art { color: #CECBF6; }
.tag { background-color: rgba(0,0,0,0.28); }
.tag.connected { color: #C0DD97; }
.tag.info { color: #CECBF6; }
.tag.offline { color: #F7C1C1; }
.row-avatar { background-color: #3C3489; color: #CECBF6; }
button.tile:hover { filter: brightness(1.15); }
button.tile:active { filter: brightness(1.3); }
button.tile.hue-purple { background-color: #3C3489; color: #CECBF6; }
button.tile.hue-purple .tile-hint { color: #AFA9EC; }
button.tile.hue-teal { background-color: #085041; color: #9FE1CB; }
button.tile.hue-teal .tile-hint { color: #5DCAA5; }
button.tile.hue-coral { background-color: #712B13; color: #F5C4B3; }
button.tile.hue-coral .tile-hint { color: #F0997B; }
button.tile.hue-amber { background-color: #633806; color: #FAC775; }
button.tile.hue-amber .tile-hint { color: #EF9F27; }
button.tile.hue-blue { background-color: #0C447C; color: #B5D4F4; }
button.tile.hue-blue .tile-hint { color: #85B7EB; }
button.tile.hue-pink { background-color: #72243E; color: #F4C0D1; }
button.tile.hue-pink .tile-hint { color: #ED93B1; }
button.tile.hue-green { background-color: #27500A; color: #C0DD97; }
button.tile.hue-green .tile-hint { color: #97C459; }
button.tile.hue-gray { background-color: #444441; color: #D3D1C7; }
button.tile.hue-gray .tile-hint { color: #B4B2A9; }
button.tile.hue-red { background-color: #791F1F; color: #F7C1C1; }
button.tile.hue-red .tile-hint { color: #F09595; }
";

fn install_css() {
    let Some(display) = gtk::gdk::Display::default() else {
        return;
    };
    let base = gtk::CssProvider::new();
    base.load_from_string(CSS);
    gtk::style_context_add_provider_for_display(
        &display,
        &base,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
    // The colours follow light or dark (the user's choice or the desktop's), live.
    let colors = gtk::CssProvider::new();
    gtk::style_context_add_provider_for_display(
        &display,
        &colors,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
    let style = adw::StyleManager::default();
    colors.load_from_string(if style.is_dark() { DARK } else { LIGHT });
    style.connect_dark_notify(move |style| {
        colors.load_from_string(if style.is_dark() { DARK } else { LIGHT });
    });
}

/// The device's current player with previous / play-pause / next.
fn now_playing(device: &str, p: &Player, sender: &ComponentSender<App>) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title("Now Playing")
        .build();
    let title = if p.title.is_empty() {
        p.name.as_str()
    } else {
        p.title.as_str()
    };
    let subtitle = match (p.artist.is_empty(), p.title.is_empty()) {
        (false, _) => format!("{} · {}", p.artist, p.name),
        (true, false) => p.name.clone(),
        (true, true) => String::new(),
    };
    let row = adw::ActionRow::builder()
        .title(title)
        .subtitle(subtitle)
        .title_lines(1)
        .subtitle_lines(1)
        .build();
    let art = gtk::Image::from_icon_name("audio-x-generic-symbolic");
    art.set_pixel_size(48);
    if let Some(path) = p.art_url.strip_prefix("file://") {
        art.set_from_file(Some(path));
    }
    row.add_prefix(&art);
    let button = |icon: &str, tooltip: &str, enabled: bool, action: &'static str| {
        let b = gtk::Button::builder()
            .icon_name(icon)
            .tooltip_text(tooltip)
            .valign(gtk::Align::Center)
            .sensitive(enabled)
            .build();
        b.add_css_class("flat");
        let (sender, device, player) = (sender.clone(), device.to_owned(), p.id.clone());
        b.connect_clicked(move |_| {
            sender.input(Input::Media(device.clone(), player.clone(), action))
        });
        b
    };
    row.add_suffix(&button(
        "media-skip-backward-symbolic",
        "Previous",
        p.can_previous,
        "previous",
    ));
    let (icon, tip) = if p.playing {
        ("media-playback-pause-symbolic", "Pause")
    } else {
        ("media-playback-start-symbolic", "Play")
    };
    let play = button(icon, tip, p.can_play || p.can_pause, "play_pause");
    play.remove_css_class("flat");
    play.add_css_class("circular");
    row.add_suffix(&play);
    row.add_suffix(&button(
        "media-skip-forward-symbolic",
        "Next",
        p.can_next,
        "next",
    ));
    group.add(&row);
    group
}

fn transfer_row(t: &Transfer, sender: &ComponentSender<App>) -> (adw::ActionRow, gtk::ProgressBar) {
    let row = adw::ActionRow::builder()
        .title(&t.name)
        .subtitle(transfer_subtitle(t))
        .title_lines(1)
        .build();
    let icon = if t.incoming {
        "folder-download-symbolic"
    } else {
        "document-send-symbolic"
    };
    row.add_prefix(&gtk::Image::from_icon_name(icon));
    let bar = gtk::ProgressBar::builder()
        .fraction(t.fraction())
        .valign(gtk::Align::Center)
        .width_request(120)
        .visible(t.state == "running")
        .build();
    row.add_suffix(&bar);
    let button = |label: &str, msg: Input| {
        let b = gtk::Button::builder()
            .label(label)
            .valign(gtk::Align::Center)
            .build();
        let (sender, msg) = (sender.clone(), std::cell::Cell::new(Some(msg)));
        b.connect_clicked(move |_| {
            if let Some(msg) = msg.take() {
                sender.input(msg);
            }
        });
        b
    };
    if t.incoming && t.state == "waiting" {
        let accept = button("Accept", Input::AcceptTransfer(t.id));
        accept.add_css_class("suggested-action");
        row.add_suffix(&button("Decline", Input::CancelTransfer(t.id)));
        row.add_suffix(&accept);
    } else {
        let cancel = gtk::Button::builder()
            .icon_name("process-stop-symbolic")
            .tooltip_text("Cancel")
            .valign(gtk::Align::Center)
            .build();
        cancel.add_css_class("flat");
        let (sender, id) = (sender.clone(), t.id);
        cancel.connect_clicked(move |_| sender.input(Input::CancelTransfer(id)));
        row.add_suffix(&cancel);
    }
    (row, bar)
}

fn text_dialog(name: &str, id: String, sender: &ComponentSender<App>) -> adw::AlertDialog {
    let dialog = adw::AlertDialog::new(
        Some(&format!("Send to {name}")),
        Some("A link opens in the browser there; other text is copied to the clipboard."),
    );
    // Enter picks the default response ("Send").
    let entry = adw::EntryRow::builder()
        .title("Link or text")
        .activates_default(true)
        .build();
    let list = gtk::ListBox::new();
    list.add_css_class("boxed-list");
    list.append(&entry);
    dialog.set_extra_child(Some(&list));
    dialog.add_responses(&[("cancel", "Cancel"), ("send", "Send")]);
    dialog.set_response_appearance("send", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("send"));
    dialog.set_close_response("cancel");
    dialog.set_response_enabled("send", false);
    entry.connect_changed({
        let dialog = dialog.clone();
        move |e| dialog.set_response_enabled("send", !e.text().trim().is_empty())
    });
    let sender = sender.clone();
    dialog.connect_response(Some("send"), move |_, _| {
        let text = entry.text().trim().to_owned();
        if !text.is_empty() {
            sender.input(Input::SendText(id.clone(), text));
        }
    });
    dialog
}

fn qr_dialog(picture: &gtk::Picture, sender: &ComponentSender<App>) -> adw::Dialog {
    let frame = gtk::Box::builder().halign(gtk::Align::Center).build();
    frame.add_css_class("card");
    frame.set_overflow(gtk::Overflow::Hidden);
    if picture.parent().is_some() {
        picture.unparent();
    }
    frame.append(picture);
    let title = gtk::Label::new(Some("Scan with Pairly on Your Phone"));
    title.add_css_class("title-3");
    let hint = gtk::Label::builder()
        .label("Open Pairly on your phone, tap Scan, and point the camera at this code. It works once and refreshes every few minutes.")
        .wrap(true)
        .justify(gtk::Justification::Center)
        .build();
    hint.add_css_class("dim-label");
    let other = gtk::Label::builder()
        .label("Phones on this network also appear under “Nearby” to pair with a code.")
        .wrap(true)
        .justify(gtk::Justification::Center)
        .build();
    other.add_css_class("caption");
    other.add_css_class("dim-label");
    let body = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(18)
        .margin_top(12)
        .margin_bottom(24)
        .margin_start(24)
        .margin_end(24)
        .build();
    body.append(&frame);
    body.append(&title);
    body.append(&hint);
    body.append(&other);
    let view = adw::ToolbarView::new();
    view.add_top_bar(&adw::HeaderBar::new());
    view.set_content(Some(&body));
    let dialog = adw::Dialog::builder()
        .title("Pair a Device")
        .content_width(440)
        .child(&view)
        .build();
    dialog.connect_closed({
        let sender = sender.clone();
        move |_| sender.input(Input::QrClosed)
    });
    dialog
}

fn code_dialog(
    id: &str,
    name: &str,
    code: &str,
    sender: &ComponentSender<App>,
    daemon: Option<DaemonProxy<'static>>,
) -> adw::AlertDialog {
    let dialog = adw::AlertDialog::new(
        Some(&format!("Pair with {name}?")),
        Some(&format!("Check that {name} shows the same code:")),
    );
    let code_label = gtk::Label::new(Some(code));
    code_label.add_css_class("title-1");
    code_label.add_css_class("monospace");
    dialog.set_extra_child(Some(&code_label));
    dialog.add_responses(&[("cancel", "Cancel"), ("accept", "Codes Match")]);
    dialog.set_response_appearance("accept", adw::ResponseAppearance::Suggested);
    dialog.set_close_response("cancel");
    let (sender, id) = (sender.clone(), id.to_owned());
    dialog.connect_response(None, move |_, response| {
        let Some(daemon) = daemon.clone() else { return };
        let (id, accept) = (id.clone(), response == "accept");
        sender.oneshot_command(async move {
            match daemon.confirm_pair(&id, accept).await {
                Ok(()) => Cmd::Done,
                Err(e) => Cmd::Failed(describe(&e)),
            }
        });
    });
    dialog
}

/// Start this app again (the new version, now at the same path): a moment after this one quits,
/// since a second copy would only hand over to the running one.
fn restart_app() {
    if let Ok(exe) = std::env::current_exe() {
        let exe = exe
            .to_string_lossy()
            .trim_end_matches(" (deleted)")
            .to_owned();
        let _ = std::process::Command::new("sh")
            .arg("-c")
            .arg("sleep 1; exec \"$0\"")
            .arg(exe)
            .spawn();
    }
    relm4::main_application().quit();
}
