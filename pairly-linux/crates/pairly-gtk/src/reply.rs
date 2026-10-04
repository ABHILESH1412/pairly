//! `pairly-gtk --reply <device> <notification> <title>`: a small window for replying to a
//! mirrored notification when the notification server can't do inline replies (mako, dunst).

use std::time::Duration;

use pairly_dbus::DaemonProxy;
use relm4::adw::prelude::*;
use relm4::{adw, gtk};

fn send(device: &str, notification: &str, text: &str) -> Result<(), String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    rt.block_on(async {
        let conn = zbus::Connection::session().await?;
        let daemon = DaemonProxy::new(&conn).await?;
        tokio::time::timeout(
            Duration::from_secs(5),
            daemon.reply_to_notification(device, notification, text),
        )
        .await
        .map_err(|_| zbus::Error::Failure("timed out".into()))?
    })
    .map_err(|e| match e {
        zbus::Error::MethodError(_, Some(msg), _) => msg,
        other => other.to_string(),
    })
}

pub fn run(device: String, notification: String, title: String) {
    let app = adw::Application::builder()
        .application_id("io.github.abhilesh1412.Pairly.Reply")
        .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.connect_activate(move |app| {
        let entry = adw::EntryRow::builder()
            .title("Message")
            .show_apply_button(false)
            .build();
        let list = gtk::ListBox::new();
        list.add_css_class("boxed-list");
        list.append(&entry);
        let status = gtk::Label::new(None);
        status.add_css_class("error");
        status.set_visible(false);
        let send_button = gtk::Button::builder().label("Send").build();
        send_button.add_css_class("suggested-action");
        let header = adw::HeaderBar::new();
        header.pack_end(&send_button);
        let body = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(12)
            .margin_bottom(18)
            .margin_start(18)
            .margin_end(18)
            .build();
        body.append(&list);
        body.append(&status);
        let view = adw::ToolbarView::new();
        view.add_top_bar(&header);
        view.set_content(Some(&body));
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title(format!("Reply to {title}"))
            .default_width(420)
            // A fixed size makes tiling compositors (Hyprland, Sway) float it like a dialog.
            .resizable(false)
            .content(&view)
            .build();

        let submit = {
            let (window, entry, status, device, notification) = (
                window.clone(),
                entry.clone(),
                status.clone(),
                device.clone(),
                notification.clone(),
            );
            move || {
                let text = entry.text().to_string();
                if text.trim().is_empty() {
                    return;
                }
                match send(&device, &notification, &text) {
                    Ok(()) => window.close(),
                    Err(e) => {
                        status.set_label(&format!("Couldn't send: {e}"));
                        status.set_visible(true);
                    }
                }
            }
        };
        send_button.connect_clicked({
            let submit = submit.clone();
            move |_| submit()
        });
        entry.connect_entry_activated(move |_| submit());
        window.present();
        entry.grab_focus();
    });
    app.run_with_args::<&str>(&[]);
}
