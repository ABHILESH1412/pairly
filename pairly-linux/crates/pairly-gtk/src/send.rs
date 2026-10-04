//! `pairly-gtk --send [--to <device>] [files…]`: the entry point for file managers ("Send to
//! Phone"), the desktop file and the tray.
//!
//! With no files it asks for some. With exactly one connected device (or `--to`) it sends
//! straight away; with several it asks which. Progress and results are pairlyd's notifications.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use pairly_dbus::{DaemonProxy, Device};
use relm4::adw::prelude::*;
use relm4::{adw, gtk};
use zbus::zvariant::Value;

fn block_on<T>(f: impl Future<Output = zbus::Result<T>>) -> Result<T, String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    rt.block_on(async { tokio::time::timeout(Duration::from_secs(10), f).await })
        .map_err(|_| "pairlyd didn't answer".to_owned())?
        .map_err(|e| match e {
            zbus::Error::MethodError(_, Some(msg), _) => msg,
            other => other.to_string(),
        })
}

async fn daemon() -> zbus::Result<DaemonProxy<'static>> {
    let conn = zbus::Connection::session().await?;
    DaemonProxy::new(&conn).await
}

fn connected_devices() -> Result<Vec<Device>, String> {
    let devices = block_on(async { daemon().await?.list_devices().await })?;
    Ok(devices
        .into_iter()
        .filter(|d| d.paired && d.is_connected())
        .collect())
}

fn send(device: &Device, paths: &[String]) -> Result<(), String> {
    let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
    block_on(async { daemon().await?.send_files(&device.id, &refs).await })?;
    let what = match paths {
        [one] => std::path::Path::new(one)
            .file_name()
            .map_or_else(|| "1 file".to_owned(), |n| n.to_string_lossy().into_owned()),
        many => format!("{} files", many.len()),
    };
    notify(&format!("Sending {what} to {}", device.name), "");
    Ok(())
}

/// A desktop notification, for when there is no window to show the outcome in.
fn notify(summary: &str, body: &str) {
    let _ = block_on(async {
        let conn = zbus::Connection::session().await?;
        let hints: HashMap<&str, Value<'_>> = HashMap::new();
        conn.call_method(
            Some("org.freedesktop.Notifications"),
            "/org/freedesktop/Notifications",
            Some("org.freedesktop.Notifications"),
            "Notify",
            &(
                "Pairly",
                0u32,
                "dev.pairly.Pairly",
                summary,
                body,
                Vec::<&str>::new(),
                hints,
                -1i32,
            ),
        )
        .await?;
        Ok(())
    });
}

/// Pick the device, then send. Returns a window if the user has to choose.
fn deliver(
    app: &adw::Application,
    target: Option<&str>,
    paths: Vec<String>,
) -> Option<adw::ApplicationWindow> {
    let devices = match connected_devices() {
        Ok(d) => d,
        Err(e) => {
            notify("Couldn't send", &format!("Is Pairly running? {e}"));
            return None;
        }
    };
    let chosen = match target {
        Some(t) => devices.iter().find(|d| d.id == t || d.name == t),
        None if devices.len() == 1 => devices.first(),
        None => None,
    };
    if let Some(device) = chosen {
        if let Err(e) = send(device, &paths) {
            notify("Couldn't send", &e);
        }
        return None;
    }
    if devices.is_empty() || target.is_some() {
        notify(
            "Couldn't send",
            "The device isn't connected. Open Pairly on it, on the same network as this PC.",
        );
        return None;
    }
    Some(picker(app, devices, paths))
}

fn picker(
    app: &adw::Application,
    devices: Vec<Device>,
    paths: Vec<String>,
) -> adw::ApplicationWindow {
    let list = gtk::ListBox::new();
    list.add_css_class("boxed-list");
    list.set_selection_mode(gtk::SelectionMode::None);
    let status = gtk::Label::new(None);
    status.add_css_class("error");
    status.set_visible(false);
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
    view.add_top_bar(&adw::HeaderBar::new());
    view.set_content(Some(&body));
    let title = match paths.len() {
        1 => "Send 1 File To".to_owned(),
        n => format!("Send {n} Files To"),
    };
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title(title)
        .default_width(360)
        // A fixed size makes tiling compositors float it like a dialog.
        .resizable(false)
        .content(&view)
        .build();
    let paths = Rc::new(paths);
    for device in devices {
        let icon = if device.device_type == "phone" {
            "phone-symbolic"
        } else {
            "computer-symbolic"
        };
        let row = adw::ActionRow::builder()
            .title(&device.name)
            .activatable(true)
            .build();
        row.add_prefix(&gtk::Image::from_icon_name(icon));
        row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
        let (window, status, paths) = (window.clone(), status.clone(), paths.clone());
        row.connect_activated(move |_| match send(&device, &paths) {
            Ok(()) => window.close(),
            Err(e) => {
                status.set_label(&format!("Couldn't send: {e}"));
                status.set_visible(true);
            }
        });
        list.append(&row);
    }
    window
}

pub fn run(target: Option<String>, paths: Vec<String>) {
    let app = adw::Application::builder()
        .application_id("dev.pairly.Pairly.Send")
        .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
        .build();
    let paths = RefCell::new(Some(paths));
    app.connect_activate(move |app| {
        let Some(paths) = paths.take() else { return };
        if !paths.is_empty() {
            if let Some(window) = deliver(app, target.as_deref(), paths) {
                window.present();
            }
            return;
        }
        // No files given: ask for some. Hold the app while the portal dialog is open.
        let guard = app.hold();
        let (app, target) = (app.clone(), target.clone());
        gtk::FileDialog::builder()
            .title("Send Files")
            .accept_label("Send")
            .build()
            .open_multiple(
                None::<&gtk::Window>,
                gtk::gio::Cancellable::NONE,
                move |res| {
                    let _guard = guard;
                    let Ok(files) = res else { return };
                    let paths: Vec<String> = (0..files.n_items())
                        .filter_map(|i| files.item(i)?.downcast::<gtk::gio::File>().ok()?.path())
                        .map(|p| p.to_string_lossy().into_owned())
                        .collect();
                    if let Some(window) = deliver(&app, target.as_deref(), paths) {
                        window.present();
                    }
                },
            );
    });
    app.run_with_args::<&str>(&[]);
}
