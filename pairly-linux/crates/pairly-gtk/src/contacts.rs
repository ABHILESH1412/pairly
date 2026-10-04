//! The Contacts window: a phone's contacts, searchable, with Call (the phone dials) and Text.

use std::cell::RefCell;
use std::rc::Rc;

use pairly_dbus::{Contact, DaemonProxy};
use relm4::adw::prelude::*;
use relm4::{adw, gtk};

fn describe(e: &zbus::Error) -> String {
    match e {
        zbus::Error::MethodError(_, Some(msg), _) => msg.clone(),
        other => other.to_string(),
    }
}

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

struct Ui {
    daemon: DaemonProxy<'static>,
    device: String,
    window: adw::Window,
    toasts: adw::ToastOverlay,
    list: gtk::ListBox,
    stack: gtk::Stack,
    contacts: RefCell<Vec<Contact>>,
}

pub fn open(
    parent: &impl IsA<gtk::Window>,
    daemon: DaemonProxy<'static>,
    device: String,
    name: &str,
) {
    let search = gtk::SearchEntry::builder()
        .placeholder_text("Search contacts")
        .hexpand(true)
        .build();
    let dial_entry = gtk::Entry::builder()
        .placeholder_text("Or dial a number")
        .input_purpose(gtk::InputPurpose::Phone)
        .hexpand(true)
        .build();
    let dial = gtk::Button::builder()
        .icon_name("call-start-symbolic")
        .tooltip_text("Call on the phone")
        .build();
    dial.add_css_class("suggested-action");
    dial.add_css_class("circular");
    let dial_row = gtk::Box::builder().spacing(6).build();
    dial_row.append(&dial_entry);
    dial_row.append(&dial);
    let top = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .margin_start(12)
        .margin_end(12)
        .margin_top(8)
        .margin_bottom(8)
        .build();
    top.append(&search);
    top.append(&dial_row);

    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .build();
    list.add_css_class("boxed-list");
    let clamp = adw::Clamp::builder()
        .maximum_size(640)
        .child(&list)
        .margin_start(12)
        .margin_end(12)
        .margin_bottom(12)
        .build();
    let scroller = gtk::ScrolledWindow::builder()
        .child(&clamp)
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    let loading = adw::StatusPage::builder()
        .icon_name("x-office-address-book-symbolic")
        .title("Loading Contacts…")
        .build();
    let stack = gtk::Stack::new();
    stack.add_named(&loading, Some("loading"));
    stack.add_named(&scroller, Some("list"));
    let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
    body.append(&top);
    body.append(&stack);
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&body));
    let view = adw::ToolbarView::new();
    view.add_top_bar(&adw::HeaderBar::new());
    view.set_content(Some(&toasts));
    let window = adw::Window::builder()
        .title(format!("Contacts — {name}"))
        .default_width(560)
        .default_height(680)
        .transient_for(parent)
        .content(&view)
        .build();

    let ui = Rc::new(Ui {
        daemon,
        device,
        window: window.clone(),
        toasts,
        list: list.clone(),
        stack,
        contacts: RefCell::default(),
    });
    // Filter by name or number as you type.
    list.set_filter_func({
        let search = search.clone();
        let ui = ui.clone();
        move |row| {
            let q = search.text().to_lowercase();
            if q.is_empty() {
                return true;
            }
            let index = usize::try_from(row.index()).unwrap_or(usize::MAX);
            ui.contacts.borrow().get(index).is_some_and(|c| {
                c.name.to_lowercase().contains(&q)
                    || c.numbers
                        .iter()
                        .any(|n| n.replace(' ', "").contains(&q.replace(' ', "")))
            })
        }
    });
    search.connect_search_changed({
        let list = list.clone();
        move |_| list.invalidate_filter()
    });
    let dial_now = {
        let (ui, entry) = (ui.clone(), dial_entry.clone());
        move || {
            let number = entry.text().trim().to_owned();
            if !number.is_empty() {
                ui.dial(number);
            }
        }
    };
    dial_entry.connect_activate({
        let dial_now = dial_now.clone();
        move |_| dial_now()
    });
    dial.connect_clicked(move |_| dial_now());

    ui.load();
    window.present();
}

impl Ui {
    fn toast(&self, text: &str) {
        self.toasts.add_toast(adw::Toast::new(text));
    }

    fn load(self: &Rc<Self>) {
        let (daemon, device) = (self.daemon.clone(), self.device.clone());
        let ui = self.clone();
        call(
            async move { daemon.list_contacts(&device).await },
            move |r| match r {
                Ok(list) => ui.show(list),
                Err(e) => {
                    ui.stack.set_visible_child_name("list");
                    ui.toast(&format!("Couldn't load contacts: {e}"));
                }
            },
        );
    }

    fn show(self: &Rc<Self>, contacts: Vec<Contact>) {
        while let Some(row) = self.list.row_at_index(0) {
            self.list.remove(&row);
        }
        for c in &contacts {
            let row = adw::ExpanderRow::builder()
                .title(gtk::glib::markup_escape_text(&c.name))
                .subtitle(gtk::glib::markup_escape_text(&c.numbers.join(", ")))
                .build();
            for number in &c.numbers {
                let line = adw::ActionRow::builder()
                    .title(gtk::glib::markup_escape_text(number))
                    .build();
                let text = gtk::Button::builder()
                    .icon_name("mail-send-symbolic")
                    .tooltip_text("Text")
                    .valign(gtk::Align::Center)
                    .build();
                text.add_css_class("flat");
                let calling = gtk::Button::builder()
                    .icon_name("call-start-symbolic")
                    .tooltip_text("Call on the phone")
                    .valign(gtk::Align::Center)
                    .build();
                calling.add_css_class("flat");
                let (ui, n) = (self.clone(), number.clone());
                calling.connect_clicked(move |_| ui.dial(n.clone()));
                let (ui, n, name) = (self.clone(), number.clone(), c.name.clone());
                text.connect_clicked(move |_| ui.compose(&name, n.clone()));
                line.add_suffix(&text);
                line.add_suffix(&calling);
                row.add_row(&line);
            }
            self.list.append(&row);
        }
        *self.contacts.borrow_mut() = contacts;
        self.stack.set_visible_child_name("list");
    }

    fn dial(self: &Rc<Self>, number: String) {
        let (daemon, device) = (self.daemon.clone(), self.device.clone());
        let ui = self.clone();
        let shown = number.clone();
        call(
            async move { daemon.dial(&device, &number).await },
            move |r| match r {
                Ok(()) => ui.toast(&format!("Calling {shown} on the phone…")),
                Err(e) => ui.toast(&format!("Couldn't call: {e}")),
            },
        );
    }

    fn compose(self: &Rc<Self>, name: &str, number: String) {
        let dialog = adw::AlertDialog::new(Some(&format!("Text {name}")), Some(&number));
        let entry = adw::EntryRow::builder()
            .title("Message")
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
        let ui = self.clone();
        dialog.connect_response(Some("send"), move |_, _| {
            let text = entry.text().trim().to_owned();
            if text.is_empty() {
                return;
            }
            let (daemon, device, number) = (ui.daemon.clone(), ui.device.clone(), number.clone());
            let ui2 = ui.clone();
            call(
                async move { daemon.send_sms(&device, &[number.as_str()], &text).await },
                move |r| match r {
                    Ok(()) => ui2.toast("Sending…"),
                    Err(e) => ui2.toast(&format!("Couldn't send: {e}")),
                },
            );
        });
        dialog.present(Some(&self.window));
    }
}
