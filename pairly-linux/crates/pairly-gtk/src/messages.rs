//! The Messages window: a phone's text conversations, read and answered from the PC.

use std::cell::RefCell;
use std::rc::Rc;

use futures_util::StreamExt;
use pairly_dbus::{Contact, Conversation, DaemonProxy, MessageAttachment, TextMessage};
use relm4::adw::prelude::*;
use relm4::{adw, gtk};

const CSS: &str = "
.bubble { padding: 8px 12px; border-radius: 14px; }
.bubble.outgoing { background: @accent_bg_color; color: @accent_fg_color; }
.bubble.incoming { background: alpha(@window_fg_color, 0.08); }
.bubble-time { font-size: smaller; opacity: 0.6; }
.bubble-sender { font-size: smaller; font-weight: bold; opacity: 0.8; }
.bubble-picture { border-radius: 10px; }
";

fn describe(e: &zbus::Error) -> String {
    match e {
        zbus::Error::MethodError(_, Some(msg), _) => msg.clone(),
        other => other.to_string(),
    }
}

/// Largest picture handed to the phone, which shrinks it further to its carrier's limit.
const MAX_PICTURE: u64 = 600 * 1024;

/// Re-encode big photos as smaller JPEGs so they fit in a picture message; anything else (or
/// anything that fails to decode) is sent as it is.
fn shrink(files: Vec<String>) -> Vec<String> {
    files
        .into_iter()
        .map(|f| shrink_picture(&f).unwrap_or(f))
        .collect()
}

fn shrink_picture(path: &str) -> Option<String> {
    use gtk::gdk_pixbuf::{Colorspace, InterpType, Pixbuf};

    let size = std::fs::metadata(path).ok()?.len();
    if size <= MAX_PICTURE || path.to_ascii_lowercase().ends_with(".gif") {
        return None;
    }
    let dir = gtk::glib::user_cache_dir().join("pairly").join("outgoing");
    std::fs::create_dir_all(&dir).ok()?;
    let stem = std::path::Path::new(path)
        .file_stem()
        .map_or_else(|| "picture".into(), |s| s.to_string_lossy().into_owned());
    let out = dir.join(format!("{stem}.jpg"));
    let mut side = 1600;
    loop {
        let pix = Pixbuf::from_file_at_scale(path, side, side, true).ok()?;
        let pix = pix.apply_embedded_orientation().unwrap_or(pix);
        // JPEG has no transparency: put it on white.
        let pix = if pix.has_alpha() {
            let (w, h) = (pix.width(), pix.height());
            let flat = Pixbuf::new(Colorspace::Rgb, false, 8, w, h)?;
            flat.fill(0xffff_ffff);
            pix.composite(
                &flat,
                0,
                0,
                w,
                h,
                0.0,
                0.0,
                1.0,
                1.0,
                InterpType::Bilinear,
                255,
            );
            flat
        } else {
            pix
        };
        let bytes = pix.save_to_bufferv("jpeg", &[("quality", "80")]).ok()?;
        if bytes.len() as u64 <= MAX_PICTURE || side <= 400 {
            std::fs::write(&out, bytes).ok()?;
            return Some(out.to_string_lossy().into_owned());
        }
        side = side * 3 / 4;
    }
}

/// Run a D-Bus call on relm4's Tokio runtime and hand the result back on the GTK thread.
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

/// A callback set after the closures that call it are made.
type LateFn = Rc<RefCell<Option<Rc<dyn Fn()>>>>;

/// A typed phone number: digits, with `+`, spaces, dashes or brackets.
fn looks_like_number(s: &str) -> bool {
    s.chars().filter(char::is_ascii_digit).count() >= 3
        && s.chars().all(|c| c.is_ascii_digit() || " +-()".contains(c))
}

/// Compare numbers by their last ten digits, so "+91 98…" matches "98…".
fn number_key(n: &str) -> String {
    let digits: Vec<char> = n.chars().filter(char::is_ascii_digit).collect();
    digits[digits.len().saturating_sub(10)..].iter().collect()
}

fn title_of(c: &Conversation) -> String {
    let names: Vec<&str> = c
        .addresses
        .iter()
        .enumerate()
        .map(|(i, a)| {
            c.names
                .get(i)
                .map(String::as_str)
                .filter(|n| !n.is_empty())
                .unwrap_or(a)
        })
        .collect();
    if names.is_empty() {
        "Unknown".into()
    } else {
        names.join(", ")
    }
}

fn when(ms: i64) -> String {
    gtk::glib::DateTime::from_unix_local(ms / 1000)
        .ok()
        .and_then(|d| {
            let today = gtk::glib::DateTime::now_local().ok()?;
            let fmt = if d.ymd() == today.ymd() {
                "%H:%M"
            } else {
                "%e %b, %H:%M"
            };
            d.format(fmt).ok()
        })
        .map(|s| s.trim().to_owned())
        .unwrap_or_default()
}

struct State {
    daemon: DaemonProxy<'static>,
    device: String,
    conversations: Vec<Conversation>,
    current: Option<Conversation>,
    /// Oldest loaded message, for "Load earlier".
    oldest: Option<i64>,
    /// The phone's contacts, once loaded for a new message.
    contacts: Option<Vec<Contact>>,
}

struct Ui {
    window: adw::Window,
    toasts: adw::ToastOverlay,
    split: adw::NavigationSplitView,
    list: gtk::ListBox,
    content: adw::NavigationPage,
    messages: gtk::ListBox,
    scroller: gtk::ScrolledWindow,
    entry: gtk::Entry,
    /// Files to send with the next message.
    pending: RefCell<Vec<String>>,
    chips: gtk::Box,
    earlier: gtk::Button,
    stack: gtk::Stack,
    state: RefCell<State>,
}

pub fn open(
    parent: &impl IsA<gtk::Window>,
    daemon: DaemonProxy<'static>,
    device: String,
    name: &str,
) {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(CSS);
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }

    // Sidebar: conversations.
    let list = gtk::ListBox::new();
    list.add_css_class("navigation-sidebar");
    let new_button = gtk::Button::builder()
        .icon_name("document-new-symbolic")
        .tooltip_text("New Message")
        .build();
    let sidebar_header = adw::HeaderBar::new();
    sidebar_header.pack_end(&new_button);
    let sidebar_view = adw::ToolbarView::new();
    sidebar_view.add_top_bar(&sidebar_header);
    sidebar_view.set_content(Some(
        &gtk::ScrolledWindow::builder()
            .child(&list)
            .vexpand(true)
            .build(),
    ));
    let sidebar = adw::NavigationPage::new(&sidebar_view, "Messages");

    // Content: one conversation.
    let messages = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .margin_start(12)
        .margin_end(12)
        .margin_top(6)
        .margin_bottom(6)
        .build();
    messages.add_css_class("background");
    let earlier = gtk::Button::builder()
        .label("Load Earlier Messages")
        .halign(gtk::Align::Center)
        .margin_top(6)
        .visible(false)
        .build();
    earlier.add_css_class("flat");
    let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
    column.append(&earlier);
    column.append(&messages);
    let clamp = adw::Clamp::builder()
        .maximum_size(720)
        .child(&column)
        .build();
    let scroller = gtk::ScrolledWindow::builder()
        .child(&clamp)
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    let entry = gtk::Entry::builder()
        .placeholder_text("Text message")
        .hexpand(true)
        .build();
    let send = gtk::Button::builder()
        .icon_name("mail-send-symbolic")
        .tooltip_text("Send")
        .sensitive(false)
        .build();
    send.add_css_class("suggested-action");
    send.add_css_class("circular");
    let compose = gtk::Box::builder()
        .spacing(6)
        .margin_start(12)
        .margin_end(12)
        .margin_top(6)
        .margin_bottom(6)
        .build();
    let attach = gtk::Button::builder()
        .icon_name("mail-attachment-symbolic")
        .tooltip_text("Attach a Picture")
        .build();
    attach.add_css_class("flat");
    // Sending pictures (MMS) doesn't get through the carrier yet: hidden until it does.
    attach.set_visible(false);
    compose.append(&attach);
    compose.append(&entry);
    compose.append(&send);
    let empty = adw::StatusPage::builder()
        .icon_name("mail-unread-symbolic")
        .title("Select a Conversation")
        .build();
    let thread_view = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let chips = gtk::Box::builder()
        .spacing(6)
        .margin_start(12)
        .margin_end(12)
        .visible(false)
        .build();
    thread_view.append(&scroller);
    thread_view.append(&chips);
    thread_view.append(&compose);
    let stack = gtk::Stack::new();
    stack.add_named(&empty, Some("empty"));
    stack.add_named(&thread_view, Some("thread"));
    let content_view = adw::ToolbarView::new();
    content_view.add_top_bar(&adw::HeaderBar::new());
    content_view.set_content(Some(&stack));
    let content = adw::NavigationPage::new(&content_view, name);

    let split = adw::NavigationSplitView::builder()
        .sidebar(&sidebar)
        .content(&content)
        .min_sidebar_width(260.0)
        .build();
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&split));
    let window = adw::Window::builder()
        .title(format!("Messages — {name}"))
        .default_width(920)
        .default_height(640)
        .transient_for(parent)
        .content(&toasts)
        .build();
    let narrow = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
        adw::BreakpointConditionLengthType::MaxWidth,
        600.0,
        adw::LengthUnit::Sp,
    ));
    narrow.add_setter(&split, "collapsed", Some(&true.to_value()));
    window.add_breakpoint(narrow);

    let ui = Rc::new(Ui {
        window: window.clone(),
        toasts,
        split,
        list: list.clone(),
        content,
        messages,
        scroller,
        entry: entry.clone(),
        pending: RefCell::default(),
        chips: chips.clone(),
        earlier: earlier.clone(),
        stack,
        state: RefCell::new(State {
            daemon: daemon.clone(),
            device: device.clone(),
            conversations: Vec::new(),
            current: None,
            oldest: None,
            contacts: None,
        }),
    });

    list.connect_row_activated({
        let ui = ui.clone();
        move |_, row| {
            let index = usize::try_from(row.index()).unwrap_or(0);
            let conv = ui.state.borrow().conversations.get(index).cloned();
            if let Some(conv) = conv {
                ui.show(conv);
            }
        }
    });
    entry.connect_changed({
        let send = send.clone();
        move |e| send.set_sensitive(!e.text().trim().is_empty())
    });
    let submit = {
        let ui = ui.clone();
        move || ui.submit()
    };
    entry.connect_activate({
        let submit = submit.clone();
        move |_| submit()
    });
    send.connect_clicked(move |_| submit());
    attach.connect_clicked({
        let ui = ui.clone();
        move |_| ui.pick_attachment()
    });
    earlier.connect_clicked({
        let ui = ui.clone();
        move |_| ui.load_messages(true)
    });
    new_button.connect_clicked({
        let ui = ui.clone();
        move |_| ui.compose_new()
    });

    // Live updates while the window is open.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<i64>();
    let watcher = relm4::spawn({
        let daemon = daemon.clone();
        let device = device.clone();
        async move {
            let Ok(mut stream) = daemon.receive_sms_received().await else {
                return;
            };
            while let Some(signal) = stream.next().await {
                if let Ok(args) = signal.args()
                    && args.id == device
                    && tx.send(args.thread_id).is_err()
                {
                    return;
                }
            }
        }
    });
    gtk::glib::spawn_future_local({
        let ui = ui.clone();
        async move {
            while let Some(thread) = rx.recv().await {
                ui.load_conversations();
                let current = ui.state.borrow().current.as_ref().map(|c| c.thread_id);
                if current == Some(thread) {
                    ui.load_messages(false);
                }
            }
        }
    });
    window.connect_close_request(move |_| {
        watcher.abort();
        gtk::glib::Propagation::Proceed
    });

    ui.load_conversations();
    window.present();
}

impl Ui {
    fn toast(&self, text: &str) {
        self.toasts.add_toast(adw::Toast::new(text));
    }

    fn load_conversations(self: &Rc<Self>) {
        let (daemon, device) = {
            let st = self.state.borrow();
            (st.daemon.clone(), st.device.clone())
        };
        let ui = self.clone();
        call(
            async move { daemon.list_conversations(&device).await },
            move |result| match result {
                Ok(list) => ui.show_conversations(list),
                Err(e) => ui.toast(&format!("Couldn't load messages: {e}")),
            },
        );
    }

    fn show_conversations(&self, list: Vec<Conversation>) {
        while let Some(row) = self.list.row_at_index(0) {
            self.list.remove(&row);
        }
        for c in &list {
            let row = adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(&title_of(c)))
                .subtitle(gtk::glib::markup_escape_text(
                    c.snippet.lines().next().unwrap_or(""),
                ))
                .subtitle_lines(1)
                .activatable(true)
                .build();
            let time = gtk::Label::new(Some(&when(c.date_ms)));
            time.add_css_class("dim-label");
            time.add_css_class("caption");
            row.add_suffix(&time);
            if !c.read {
                row.add_css_class("heading");
            }
            self.list.append(&row);
        }
        let mut st = self.state.borrow_mut();
        // Keep the open conversation's details current.
        if let Some(cur) = &st.current
            && let Some(fresh) = list.iter().find(|c| c.thread_id == cur.thread_id)
        {
            st.current = Some(fresh.clone());
        }
        st.conversations = list;
    }

    fn show(self: &Rc<Self>, conv: Conversation) {
        self.content.set_title(&title_of(&conv));
        {
            let mut st = self.state.borrow_mut();
            st.current = Some(conv);
            st.oldest = None;
        }
        self.stack.set_visible_child_name("thread");
        self.split.set_show_content(true);
        self.load_messages(false);
        self.entry.grab_focus();
    }

    /// Load the newest page, or (`earlier`) the page before the oldest shown.
    fn load_messages(self: &Rc<Self>, earlier: bool) {
        let (daemon, device, thread, before) = {
            let st = self.state.borrow();
            let Some(conv) = &st.current else { return };
            let before = if earlier { st.oldest.unwrap_or(0) } else { 0 };
            (st.daemon.clone(), st.device.clone(), conv.thread_id, before)
        };
        let ui = self.clone();
        call(
            async move { daemon.list_messages(&device, thread, before).await },
            move |result| match result {
                Ok(page) => ui.show_messages(page, earlier),
                Err(e) => ui.toast(&format!("Couldn't load the conversation: {e}")),
            },
        );
    }

    fn show_messages(self: &Rc<Self>, page: Vec<TextMessage>, earlier: bool) {
        if !earlier {
            while let Some(row) = self.messages.row_at_index(0) {
                self.messages.remove(&row);
            }
        }
        self.earlier.set_visible(page.len() >= 50);
        if let Some(first) = page.first() {
            self.state.borrow_mut().oldest = Some(first.date_ms);
        }
        for (i, m) in page.iter().enumerate() {
            let row = self.bubble(m);
            if earlier {
                self.messages.insert(&row, i32::try_from(i).unwrap_or(0));
            } else {
                self.messages.append(&row);
            }
        }
        if !earlier {
            // Scroll to the newest message once laid out.
            let adj = self.scroller.vadjustment();
            gtk::glib::idle_add_local_once(move || adj.set_value(adj.upper()));
        }
    }

    fn submit(self: &Rc<Self>) {
        let text = self.entry.text().trim().to_owned();
        let files = self.pending.borrow().clone();
        let (daemon, device, addresses) = {
            let st = self.state.borrow();
            let Some(conv) = &st.current else { return };
            (st.daemon.clone(), st.device.clone(), conv.addresses.clone())
        };
        if (text.is_empty() && files.is_empty()) || addresses.is_empty() {
            return;
        }
        self.entry.set_text("");
        if files.is_empty() {
            self.send_text(daemon, device, addresses, text);
            return;
        }
        self.pending.borrow_mut().clear();
        self.show_chips();
        let ui = self.clone();
        call(
            async move {
                let files = tokio::task::spawn_blocking(move || shrink(files))
                    .await
                    .map_err(|e| zbus::Error::Failure(e.to_string()))?;
                let refs: Vec<&str> = addresses.iter().map(String::as_str).collect();
                let files: Vec<&str> = files.iter().map(String::as_str).collect();
                daemon.send_mms(&device, &refs, &text, &files).await
            },
            move |result| match result {
                Ok(()) => ui.toast("Sending picture message…"),
                Err(e) => ui.toast(&format!("Couldn't send: {e}")),
            },
        );
    }

    fn pick_attachment(self: &Rc<Self>) {
        let filter = gtk::FileFilter::new();
        filter.set_name(Some("Pictures, videos and audio"));
        for m in ["image/*", "video/*", "audio/*"] {
            filter.add_mime_type(m);
        }
        let filters = gtk::gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);
        let ui = self.clone();
        gtk::FileDialog::builder()
            .title("Attach to the Message")
            .filters(&filters)
            .build()
            .open_multiple(
                Some(&self.window),
                gtk::gio::Cancellable::NONE,
                move |res| {
                    if let Ok(files) = res {
                        for i in 0..files.n_items() {
                            if let Some(path) = files
                                .item(i)
                                .and_then(|f| f.downcast::<gtk::gio::File>().ok())
                                .and_then(|f| f.path())
                            {
                                ui.pending
                                    .borrow_mut()
                                    .push(path.to_string_lossy().into_owned());
                            }
                        }
                        ui.show_chips();
                    }
                },
            );
    }

    /// The attachments waiting to be sent, each removable.
    fn show_chips(self: &Rc<Self>) {
        while let Some(child) = self.chips.first_child() {
            self.chips.remove(&child);
        }
        let pending = self.pending.borrow().clone();
        self.chips.set_visible(!pending.is_empty());
        for (i, path) in pending.iter().enumerate() {
            let name = std::path::Path::new(path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let chip = gtk::Button::builder()
                .label(format!("{name}  ✕"))
                .tooltip_text("Remove")
                .build();
            chip.add_css_class("pill");
            chip.add_css_class("small");
            let ui = self.clone();
            chip.connect_clicked(move |_| {
                let mut p = ui.pending.borrow_mut();
                if i < p.len() {
                    p.remove(i);
                }
                drop(p);
                ui.show_chips();
            });
            self.chips.append(&chip);
        }
    }

    /// Fetch an attachment into a local file, then hand it to `done`.
    fn fetch(self: &Rc<Self>, a: &MessageAttachment, done: impl FnOnce(String) + 'static) {
        let (daemon, device) = {
            let st = self.state.borrow();
            (st.daemon.clone(), st.device.clone())
        };
        let (part, name) = (a.part_id, a.name.clone());
        let ui = self.clone();
        call(
            async move { daemon.sms_attachment(&device, part, &name).await },
            move |r| match r {
                Ok(path) => done(path),
                Err(e) => ui.toast(&format!("Couldn't load the attachment: {e}")),
            },
        );
    }

    fn sender_name(&self, address: &str) -> String {
        let st = self.state.borrow();
        st.current
            .as_ref()
            .and_then(|c| {
                let i = c.addresses.iter().position(|a| a == address)?;
                c.names.get(i).filter(|n| !n.is_empty()).cloned()
            })
            .unwrap_or_else(|| address.to_owned())
    }

    fn bubble(self: &Rc<Self>, m: &TextMessage) -> gtk::ListBoxRow {
        let bubble = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(4)
            .halign(if m.outgoing {
                gtk::Align::End
            } else {
                gtk::Align::Start
            })
            .margin_top(3)
            .margin_bottom(3)
            .build();
        bubble.add_css_class("bubble");
        bubble.add_css_class(if m.outgoing { "outgoing" } else { "incoming" });
        // In a group, say who wrote it.
        if !m.outgoing && m.participants.len() > 1 {
            let sender = gtk::Label::builder()
                .label(self.sender_name(&m.address))
                .xalign(0.0)
                .build();
            sender.add_css_class("bubble-sender");
            bubble.append(&sender);
        }
        for a in &m.attachments {
            bubble.append(&self.attachment_widget(a));
        }
        if !m.body.is_empty() {
            let text = gtk::Label::builder()
                .label(&m.body)
                .wrap(true)
                .wrap_mode(gtk::pango::WrapMode::WordChar)
                .xalign(0.0)
                .selectable(true)
                .max_width_chars(48)
                .build();
            bubble.append(&text);
        }
        let time = gtk::Label::builder()
            .label(when(m.date_ms))
            .xalign(0.0)
            .build();
        time.add_css_class("bubble-time");
        bubble.append(&time);
        gtk::ListBoxRow::builder()
            .child(&bubble)
            .activatable(false)
            .selectable(false)
            .build()
    }

    /// A picture shown inline, or a button for any other attachment.
    fn attachment_widget(self: &Rc<Self>, a: &MessageAttachment) -> gtk::Widget {
        if a.mime.starts_with("image/") {
            let picture = gtk::Picture::builder()
                .content_fit(gtk::ContentFit::Contain)
                .width_request(240)
                .height_request(180)
                .can_shrink(true)
                .build();
            picture.add_css_class("bubble-picture");
            let p = picture.clone();
            self.fetch(a, move |path| p.set_filename(Some(path)));
            let click = gtk::GestureClick::new();
            let (ui, a2) = (self.clone(), a.clone());
            click.connect_released(move |_, _, _, _| ui.open_attachment(&a2));
            picture.add_controller(click);
            return picture.upcast();
        }
        let label = if a.size > 0 {
            format!("📎 {} ({})", a.name, gtk::glib::format_size(a.size))
        } else {
            format!("📎 {}", a.name)
        };
        let button = gtk::Button::builder().label(label).build();
        button.add_css_class("flat");
        let (ui, a2) = (self.clone(), a.clone());
        button.connect_clicked(move |_| ui.open_attachment(&a2));
        button.upcast()
    }

    fn open_attachment(self: &Rc<Self>, a: &MessageAttachment) {
        let window = self.window.clone();
        self.fetch(a, move |path| {
            gtk::FileLauncher::new(Some(&gtk::gio::File::for_path(&path))).launch(
                Some(&window),
                gtk::gio::Cancellable::NONE,
                |_| {},
            );
        });
    }

    fn send_text(
        self: &Rc<Self>,
        daemon: DaemonProxy<'static>,
        device: String,
        addresses: Vec<String>,
        text: String,
    ) {
        let ui = self.clone();
        call(
            async move {
                let refs: Vec<&str> = addresses.iter().map(String::as_str).collect();
                daemon.send_sms(&device, &refs, &text).await
            },
            move |result| match result {
                // The phone reports the sent message back, which refreshes the thread.
                Ok(()) => ui.toast("Sending…"),
                Err(e) => ui.toast(&format!("Couldn't send: {e}")),
            },
        );
    }

    /// A new message: pick people from the phone's contacts (several make a group message) or
    /// type a number.
    fn compose_new(self: &Rc<Self>) {
        let send = gtk::Button::with_label("Send");
        send.add_css_class("suggested-action");
        send.set_sensitive(false);
        let header = adw::HeaderBar::new();
        header.pack_end(&send);

        let chips = gtk::FlowBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .column_spacing(6)
            .row_spacing(6)
            .visible(false)
            .build();
        let to = gtk::SearchEntry::builder()
            .placeholder_text("Name or phone number")
            .input_purpose(gtk::InputPurpose::Phone)
            .build();
        let use_number = adw::ActionRow::builder()
            .activatable(true)
            .visible(false)
            .build();
        use_number.add_prefix(&gtk::Image::from_icon_name("call-start-symbolic"));
        let typed = gtk::ListBox::new();
        typed.add_css_class("boxed-list");
        typed.set_selection_mode(gtk::SelectionMode::None);
        typed.append(&use_number);
        typed.set_visible(false);
        let people = gtk::ListBox::new();
        people.add_css_class("boxed-list");
        people.set_selection_mode(gtk::SelectionMode::None);
        let status = gtk::Label::builder()
            .label("Loading contacts…")
            .wrap(true)
            .css_classes(["dim-label"])
            .build();
        let list_box = gtk::Box::new(gtk::Orientation::Vertical, 12);
        list_box.append(&typed);
        list_box.append(&status);
        list_box.append(&people);
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&list_box)
            .build();
        let body = gtk::Entry::builder()
            .placeholder_text("Message")
            .hexpand(true)
            .build();
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();
        content.append(&chips);
        content.append(&to);
        content.append(&scroller);
        content.append(&body);
        let view = adw::ToolbarView::new();
        view.add_top_bar(&header);
        view.set_content(Some(&content));
        let dialog = adw::Dialog::builder()
            .title("New Message")
            .content_width(440)
            .content_height(600)
            .child(&view)
            .build();

        // Chosen (name, number) pairs.
        let chosen: Rc<RefCell<Vec<(String, String)>>> = Rc::default();
        let typed_number = {
            let to = to.clone();
            move || {
                let t = to.text().trim().to_owned();
                looks_like_number(&t).then_some(t)
            }
        };
        let refresh = Rc::new({
            let (send, body, chips, chosen, typed_number) = (
                send.clone(),
                body.clone(),
                chips.clone(),
                chosen.clone(),
                typed_number.clone(),
            );
            move || {
                let has_people = !chosen.borrow().is_empty() || typed_number().is_some();
                send.set_sensitive(has_people && !body.text().trim().is_empty());
                chips.set_visible(!chosen.borrow().is_empty());
            }
        });
        // Chips redraw themselves when one is removed: filled in once `draw` exists.
        let redraw_chips: LateFn = Rc::default();
        let draw = {
            let (chips, chosen, refresh, redraw) = (
                chips.clone(),
                chosen.clone(),
                refresh.clone(),
                redraw_chips.clone(),
            );
            Rc::new(move || {
                chips.remove_all();
                for (i, (name, number)) in chosen.borrow().iter().enumerate() {
                    let chip = gtk::Button::builder()
                        .label(format!("{name}  ✕"))
                        .tooltip_text(number.as_str())
                        .build();
                    chip.add_css_class("pill");
                    chip.add_css_class("small");
                    let (chosen, redraw) = (chosen.clone(), redraw.clone());
                    chip.connect_clicked(move |_| {
                        {
                            let mut c = chosen.borrow_mut();
                            if i < c.len() {
                                c.remove(i);
                            }
                        }
                        if let Some(f) = redraw.borrow().clone() {
                            f();
                        }
                    });
                    chips.insert(&chip, -1);
                }
                refresh();
            })
        };
        *redraw_chips.borrow_mut() = Some(draw.clone());
        let add = {
            let (chosen, draw, to) = (chosen.clone(), draw.clone(), to.clone());
            Rc::new(move |name: String, number: String| {
                let key = number_key(&number);
                if !chosen.borrow().iter().any(|(_, n)| number_key(n) == key) {
                    chosen.borrow_mut().push((name, number));
                }
                to.set_text("");
                draw();
            })
        };

        // Filter contacts by name or number as you type.
        people.set_filter_func({
            let to = to.clone();
            move |row| {
                let q = to.text().trim().to_lowercase();
                if q.is_empty() {
                    return true;
                }
                let Some(row) = row.downcast_ref::<adw::ActionRow>() else {
                    return true;
                };
                let digits: String = q.chars().filter(char::is_ascii_digit).collect();
                row.title().to_lowercase().contains(&q)
                    || (!digits.is_empty()
                        && row
                            .subtitle()
                            .unwrap_or_default()
                            .chars()
                            .filter(char::is_ascii_digit)
                            .collect::<String>()
                            .contains(&digits))
            }
        });
        to.connect_search_changed({
            let (people, use_number, typed, refresh, typed_number) = (
                people.clone(),
                use_number.clone(),
                typed.clone(),
                refresh.clone(),
                typed_number.clone(),
            );
            move |_| {
                people.invalidate_filter();
                let number = typed_number();
                typed.set_visible(number.is_some());
                use_number.set_visible(number.is_some());
                if let Some(n) = number {
                    use_number.set_title(&format!("Send to {n}"));
                }
                refresh();
            }
        });
        let add_typed = {
            let (add, typed_number) = (add.clone(), typed_number.clone());
            move || {
                if let Some(n) = typed_number() {
                    add(n.clone(), n);
                }
            }
        };
        use_number.connect_activated({
            let add_typed = add_typed.clone();
            move |_| add_typed()
        });
        to.connect_activate(move |_| add_typed());
        body.connect_changed({
            let refresh = refresh.clone();
            move |_| refresh()
        });

        // Fill the contact list (loaded once per window).
        let fill = {
            let (people, status, add) = (people.clone(), status.clone(), add.clone());
            move |contacts: &[Contact]| {
                status.set_visible(contacts.is_empty());
                status.set_label("No contacts on the phone. Type a number instead.");
                for c in contacts {
                    for number in &c.numbers {
                        let row = adw::ActionRow::builder()
                            .title(gtk::glib::markup_escape_text(&c.name).as_str())
                            .subtitle(gtk::glib::markup_escape_text(number).as_str())
                            .activatable(true)
                            .build();
                        row.add_prefix(&gtk::Image::from_icon_name("avatar-default-symbolic"));
                        let (add, name, number) = (add.clone(), c.name.clone(), number.clone());
                        row.connect_activated(move |_| add(name.clone(), number.clone()));
                        people.append(&row);
                    }
                }
            }
        };
        let cached = self.state.borrow().contacts.clone();
        if let Some(contacts) = cached {
            fill(&contacts);
        } else {
            let (daemon, device) = {
                let st = self.state.borrow();
                (st.daemon.clone(), st.device.clone())
            };
            let (ui, status) = (self.clone(), status.clone());
            call(
                async move { daemon.list_contacts(&device).await },
                move |result| match result {
                    Ok(contacts) => {
                        fill(&contacts);
                        ui.state.borrow_mut().contacts = Some(contacts);
                    }
                    Err(e) => status.set_label(&format!(
                        "Couldn't load contacts ({e}). Type a number instead."
                    )),
                },
            );
        }

        let ui = self.clone();
        send.connect_clicked({
            let dialog = dialog.clone();
            move |_| {
                let text = body.text().trim().to_owned();
                let mut numbers: Vec<String> =
                    chosen.borrow().iter().map(|(_, n)| n.clone()).collect();
                if let Some(n) = typed_number()
                    && !numbers.iter().any(|x| number_key(x) == number_key(&n))
                {
                    numbers.push(n);
                }
                if numbers.is_empty() || text.is_empty() {
                    return;
                }
                let (daemon, device) = {
                    let st = ui.state.borrow();
                    (st.daemon.clone(), st.device.clone())
                };
                ui.send_text(daemon, device, numbers, text);
                dialog.close();
            }
        });
        dialog.present(Some(&self.window));
    }
}
