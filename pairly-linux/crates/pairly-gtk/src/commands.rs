//! The Commands dialog: what paired phones may run on this PC.

use std::rc::Rc;

use pairly_dbus::{Command, DaemonProxy};
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

/// Common commands, one click to add.
const SUGGESTIONS: [(&str, &str); 7] = [
    ("Lock Screen", "loginctl lock-session"),
    ("Suspend", "systemctl suspend"),
    (
        "Mute / Unmute",
        "wpctl set-mute @DEFAULT_AUDIO_SINK@ toggle",
    ),
    (
        "Volume Up",
        "wpctl set-volume -l 1.0 @DEFAULT_AUDIO_SINK@ 5%+",
    ),
    ("Volume Down", "wpctl set-volume @DEFAULT_AUDIO_SINK@ 5%-"),
    (
        "Screenshot",
        "grim ~/Pictures/screenshot-$(date +%Y%m%d-%H%M%S).png",
    ),
    ("Shut Down", "systemctl poweroff"),
];

struct Ui {
    daemon: DaemonProxy<'static>,
    list: adw::PreferencesGroup,
    rows: std::cell::RefCell<Vec<adw::ActionRow>>,
    toasts: adw::ToastOverlay,
}

pub fn open(parent: &impl IsA<gtk::Widget>, daemon: DaemonProxy<'static>) {
    let list = adw::PreferencesGroup::builder()
        .title("Commands")
        .description(
            "Your phone can run these after you confirm on it. It can't run anything else.",
        )
        .build();
    let name = adw::EntryRow::builder()
        .title("Name (e.g. Lock Screen)")
        .build();
    let line = adw::EntryRow::builder()
        .title("Command (e.g. loginctl lock-session)")
        .build();
    let add = adw::ButtonRow::builder().title("Add Command").build();
    add.add_css_class("suggested-action");
    let new_group = adw::PreferencesGroup::builder()
        .title("New Command")
        .build();
    new_group.add(&name);
    new_group.add(&line);
    new_group.add(&add);
    let suggestions = adw::PreferencesGroup::builder()
        .title("Suggestions")
        .description("Common commands; add one with a click.")
        .build();
    let page = adw::PreferencesPage::new();
    page.add(&list);
    page.add(&new_group);
    page.add(&suggestions);
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&page));
    let view = adw::ToolbarView::new();
    view.add_top_bar(&adw::HeaderBar::new());
    view.set_content(Some(&toasts));
    let dialog = adw::Dialog::builder()
        .title("Commands")
        .content_width(520)
        .content_height(560)
        .child(&view)
        .build();

    let ui = Rc::new(Ui {
        daemon,
        list,
        rows: std::cell::RefCell::default(),
        toasts,
    });
    add.connect_activated({
        let ui = ui.clone();
        move |_| {
            let (n, l) = (name.text().trim().to_owned(), line.text().trim().to_owned());
            if n.is_empty() || l.is_empty() {
                ui.toasts
                    .add_toast(adw::Toast::new("Enter a name and a command"));
                return;
            }
            let daemon = ui.daemon.clone();
            let ui2 = ui.clone();
            let (name, line) = (name.clone(), line.clone());
            call(
                async move { daemon.add_command(&n, &l).await },
                move |r| match r {
                    Ok(_) => {
                        name.set_text("");
                        line.set_text("");
                        ui2.reload();
                    }
                    Err(e) => ui2.toasts.add_toast(adw::Toast::new(&e)),
                },
            );
        }
    });
    for (title, line) in SUGGESTIONS {
        let row = adw::ActionRow::builder()
            .title(title)
            .subtitle(gtk::glib::markup_escape_text(line))
            .build();
        let button = gtk::Button::builder()
            .icon_name("list-add-symbolic")
            .tooltip_text("Add")
            .valign(gtk::Align::Center)
            .build();
        button.add_css_class("flat");
        let ui = ui.clone();
        button.connect_clicked(move |_| {
            let daemon = ui.daemon.clone();
            let ui2 = ui.clone();
            call(
                async move { daemon.add_command(title, line).await },
                move |r| match r {
                    Ok(_) => {
                        ui2.toasts
                            .add_toast(adw::Toast::new(&format!("Added {title}")));
                        ui2.reload();
                    }
                    Err(e) => ui2.toasts.add_toast(adw::Toast::new(&e)),
                },
            );
        });
        row.add_suffix(&button);
        suggestions.add(&row);
    }
    ui.reload();
    dialog.present(Some(parent));
}

impl Ui {
    fn reload(self: &Rc<Self>) {
        let daemon = self.daemon.clone();
        let ui = self.clone();
        call(
            async move { daemon.list_commands().await },
            move |r| match r {
                Ok(list) => ui.show(&list),
                Err(e) => ui.toasts.add_toast(adw::Toast::new(&e)),
            },
        );
    }

    fn show(self: &Rc<Self>, commands: &[Command]) {
        for row in self.rows.borrow_mut().drain(..) {
            self.list.remove(&row);
        }
        for c in commands {
            let row = adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(&c.name))
                .subtitle(gtk::glib::markup_escape_text(&c.command))
                .subtitle_lines(2)
                .build();
            row.add_css_class("monospace");
            let remove = gtk::Button::builder()
                .icon_name("user-trash-symbolic")
                .tooltip_text("Remove")
                .valign(gtk::Align::Center)
                .build();
            remove.add_css_class("flat");
            let (ui, id) = (self.clone(), c.id.clone());
            remove.connect_clicked(move |_| {
                let daemon = ui.daemon.clone();
                let (ui2, id) = (ui.clone(), id.clone());
                call(
                    async move { daemon.remove_command(&id).await },
                    move |r| match r {
                        Ok(()) => ui2.reload(),
                        Err(e) => ui2.toasts.add_toast(adw::Toast::new(&e)),
                    },
                );
            });
            row.add_suffix(&remove);
            self.list.add(&row);
            self.rows.borrow_mut().push(row);
        }
    }
}
