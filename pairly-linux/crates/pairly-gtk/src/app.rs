//! The main window: a sidebar of devices and a page for the selected one. All state comes from
//! `pairlyd` over D-Bus; the UI never talks to the network itself.
//!
//! relm4 drives the message loop and runs async D-Bus work as commands on its Tokio runtime.
//! The device list and page are rebuilt from the latest state on every change: they are small,
//! and rebuilding keeps them trivially consistent with the daemon.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use futures_util::StreamExt;
use pairly_dbus::{DaemonProxy, Device, Transfer};
use relm4::adw::prelude::*;
use relm4::{Component, ComponentParts, ComponentSender, Sender, adw, gtk};

const APP_TITLE: &str = "Pairly";

#[derive(Debug, Clone)]
enum Status {
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
    Unpair(String),
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
    Failed(String),
    Done,
}

pub struct Widgets {
    toasts: adw::ToastOverlay,
    split: adw::NavigationSplitView,
    title: adw::WindowTitle,
    list: gtk::ListBox,
    content_page: adw::NavigationPage,
    content: gtk::Stack,
    device_slot: adw::Bin,
    status_page: adw::StatusPage,
    retry: gtk::Button,
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
        // Launching Pairly again brings this window to the front.
        relm4::main_application().connect_activate({
            let window = window.clone();
            move |_| window.present()
        });

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
        let sidebar_header = adw::HeaderBar::new();
        sidebar_header.set_title_widget(Some(&title));
        sidebar_header.pack_end(&pair_button);
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
        let sidebar_page = adw::NavigationPage::new(&sidebar_view, APP_TITLE);

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
        status_page.set_child(Some(&retry));
        let device_slot = adw::Bin::new();
        device_slot.add_controller(file_drop_target({
            let sender = sender.clone();
            move |paths| sender.input(Input::DropFiles(paths))
        }));
        let content = gtk::Stack::new();
        content.add_named(&status_page, Some("status"));
        content.add_named(&device_slot, Some("device"));
        let content_view = adw::ToolbarView::new();
        content_view.add_top_bar(&adw::HeaderBar::new());
        content_view.set_content(Some(&content));
        let content_page = adw::NavigationPage::new(&content_view, APP_TITLE);

        let split = adw::NavigationSplitView::builder()
            .min_sidebar_width(280.0)
            .max_sidebar_width(340.0)
            .build();
        split.set_sidebar(Some(&sidebar_page));
        split.set_content(Some(&content_page));
        let toasts = adw::ToastOverlay::new();
        toasts.set_child(Some(&split));
        window.set_content(Some(&toasts));

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
        // `gapplication action dev.pairly.Pairly pair`, e.g. from the tray).
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
        app.set_accels_for_action("app.quit", &["<Control>q"]);
        app.set_accels_for_action("window.close", &["<Control>w"]);

        sender.command(|out, shutdown| shutdown.register(watch_daemon(out)).drop_on_shutdown());

        let model = App {
            daemon: None,
            show_pairing_when_ready: init.show_pairing,
            status: Status::Connecting,
            me: None,
            devices: Vec::new(),
            selected: None,
            qr_dialog: None,
            ringing: std::collections::HashSet::new(),
            transfers: BTreeMap::new(),
        };
        let mut widgets = Widgets {
            toasts,
            split,
            title,
            list,
            content_page,
            content,
            device_slot,
            status_page,
            retry,
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
                self.selected = Some(id);
                widgets.split.set_show_content(true);
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
            Input::AcceptTransfer(t) => {
                self.call(&sender, move |d| async move { d.accept_transfer(t).await });
                return;
            }
            Input::CancelTransfer(t) => {
                self.call(&sender, move |d| async move { d.cancel_transfer(t).await });
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
            Cmd::Connected(daemon, me) => {
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
            Cmd::Done => return,
        }
        self.render(widgets, &sender);
    }
}

impl App {
    fn render(&self, w: &mut Widgets, sender: &ComponentSender<Self>) {
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
                w.content_page.set_title(&device.name);
                let ringing = self.ringing.contains(&device.id);
                let transfers: Vec<&Transfer> = self
                    .transfers
                    .values()
                    .filter(|t| t.device == device.id)
                    .collect();
                w.transfer_rows.clear();
                w.device_slot.set_child(Some(&device_page(
                    device,
                    ringing,
                    &transfers,
                    &mut w.transfer_rows,
                    sender,
                )));
                w.content.set_visible_child_name("device");
            }
            (status, _) => {
                w.content_page.set_title(APP_TITLE);
                let (icon, title, body, show_button) = match status {
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
    icon.set_pixel_size(24);
    let name = gtk::Label::builder()
        .label(&d.name)
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    let status = gtk::Label::builder()
        .label(status_text(d))
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    status.add_css_class("caption");
    status.add_css_class(if d.is_connected() {
        "success"
    } else {
        "dim-label"
    });
    let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
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
fn file_drop_target(on_drop: impl Fn(Vec<String>) + 'static) -> gtk::DropTarget {
    let target = gtk::DropTarget::new(
        gtk::gdk::FileList::static_type(),
        gtk::gdk::DragAction::COPY,
    );
    target.connect_drop(move |_, value, _, _| {
        let Ok(list) = value.get::<gtk::gdk::FileList>() else {
            return false;
        };
        let paths: Vec<String> = list
            .files()
            .iter()
            .filter_map(|f| f.path())
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        if paths.is_empty() {
            return false;
        }
        on_drop(paths);
        true
    });
    target
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

/// A row with a single button that sends `msg`.
fn button_row(
    title: &str,
    subtitle: &str,
    label: &str,
    enabled: bool,
    sender: &ComponentSender<App>,
    msg: impl Fn() -> Input + 'static,
) -> adw::ActionRow {
    let button = gtk::Button::builder()
        .label(label)
        .valign(gtk::Align::Center)
        .sensitive(enabled)
        .build();
    let sender = sender.clone();
    button.connect_clicked(move |_| sender.input(msg()));
    let row = adw::ActionRow::builder()
        .title(title)
        .subtitle(subtitle)
        .build();
    row.add_suffix(&button);
    row
}

fn device_page(
    d: &Device,
    ringing: bool,
    transfers: &[&Transfer],
    rows: &mut HashMap<u64, (adw::ActionRow, gtk::ProgressBar)>,
    sender: &ComponentSender<App>,
) -> gtk::Widget {
    let page = adw::PreferencesPage::new();

    let icon = gtk::Image::from_icon_name(icon_for(&d.device_type));
    icon.set_pixel_size(72);
    icon.add_css_class("dim-label");
    let name = gtk::Label::new(Some(&d.name));
    name.add_css_class("title-1");
    let status = gtk::Label::new(Some(&status_text(d)));
    status.add_css_class(if d.is_connected() {
        "success"
    } else {
        "dim-label"
    });
    let header = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .margin_bottom(12)
        .build();
    header.append(&icon);
    header.append(&name);
    header.append(&status);
    let header_group = adw::PreferencesGroup::new();
    header_group.add(&header);
    page.add(&header_group);

    let actions = adw::PreferencesGroup::new();
    if d.paired {
        let (on, id) = (d.is_connected(), d.id.clone());
        let files_subtitle = if on {
            "Or drop files on this window"
        } else {
            "Available when connected"
        };
        actions.add(&button_row(
            "Files",
            files_subtitle,
            "Send…",
            on,
            sender,
            {
                let id = id.clone();
                move || Input::PickFiles(id.clone())
            },
        ));
        actions.add(&button_row(
            "Link or Text",
            "Links open in the browser; text is copied",
            "Send…",
            on,
            sender,
            {
                let id = id.clone();
                move || Input::AskText(id.clone())
            },
        ));
        actions.add(&button_row(
            "Clipboard",
            "Send what you copied on this PC",
            "Send",
            on,
            sender,
            {
                let id = id.clone();
                move || Input::SendClipboard(id.clone())
            },
        ));
        let (ring_label, ring_subtitle) = if ringing {
            ("Stop", "Ringing…")
        } else {
            ("Ring", "Ring loudly, even on silent, to find it")
        };
        actions.add(&button_row(
            "Find My Phone",
            ring_subtitle,
            ring_label,
            on,
            sender,
            {
                let id = id.clone();
                move || Input::Ring(id.clone(), !ringing)
            },
        ));
        actions.add(&button_row(
            "Ping",
            "Make it show a notification",
            "Ping",
            on,
            sender,
            move || Input::Ping(id.clone()),
        ));
    } else {
        let pair_button = gtk::Button::builder()
            .label("Pair")
            .valign(gtk::Align::Center)
            .build();
        pair_button.add_css_class("suggested-action");
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
    }
    page.add(&actions);

    if !transfers.is_empty() {
        let group = adw::PreferencesGroup::builder().title("Transfers").build();
        for t in transfers {
            let (row, bar) = transfer_row(t, sender);
            rows.insert(t.id, (row.clone(), bar));
            group.add(&row);
        }
        page.add(&group);
    }

    let details = adw::PreferencesGroup::builder().title("Details").build();
    if d.battery >= 0 {
        let level = if d.charging {
            format!("{}% · charging", d.battery)
        } else {
            format!("{}%", d.battery)
        };
        let battery = adw::ActionRow::builder()
            .title("Battery")
            .subtitle(level)
            .build();
        battery.add_css_class("property");
        details.add(&battery);
    }
    let id_row = adw::ActionRow::builder()
        .title("Device ID")
        .subtitle(&d.id)
        .subtitle_selectable(true)
        .build();
    id_row.add_css_class("property");
    details.add(&id_row);
    page.add(&details);

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
        page.add(&danger);
    }
    page.upcast()
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
