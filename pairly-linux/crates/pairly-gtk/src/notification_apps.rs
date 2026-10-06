//! The "PC Notifications" dialog: which of this PC's apps send their notifications to paired
//! devices. Apps appear once they've posted a notification.

use pairly_dbus::{DaemonProxy, NotificationApp};
use relm4::adw::prelude::*;
use relm4::{adw, gtk};

fn describe(e: &zbus::Error) -> String {
    match e {
        zbus::Error::MethodError(_, Some(msg), _) => msg.clone(),
        other => other.to_string(),
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

/// "Today", "Yesterday" or a date, for when an app last posted.
fn last_seen(unix: u64) -> String {
    let Ok(then) = gtk::glib::DateTime::from_unix_local(i64::try_from(unix).unwrap_or(0)) else {
        return String::new();
    };
    let Ok(now) = gtk::glib::DateTime::now_local() else {
        return String::new();
    };
    let yesterday = now.add_days(-1).map(|d| d.ymd()).ok();
    if then.ymd() == now.ymd() {
        "Last notification today".into()
    } else if Some(then.ymd()) == yesterday {
        "Last notification yesterday".into()
    } else {
        then.format("Last notification on %e %b")
            .map(|s| s.trim().replace("  ", " "))
            .unwrap_or_default()
    }
}

pub fn open(parent: &impl IsA<gtk::Widget>, daemon: DaemonProxy<'static>) {
    let list = adw::PreferencesGroup::builder()
        .title("Send to Your Phone")
        .description(
            "Notifications from apps switched on here appear on your paired phone. \\
             Apps show up after their first notification.",
        )
        .build();
    let status = adw::ActionRow::builder()
        .title("Loading…")
        .css_classes(["dim-label"])
        .build();
    list.add(&status);
    let page = adw::PreferencesPage::new();
    page.add(&list);
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&page));
    let view = adw::ToolbarView::new();
    view.add_top_bar(&adw::HeaderBar::new());
    view.set_content(Some(&toasts));
    let dialog = adw::Dialog::builder()
        .title("PC Notifications")
        .content_width(480)
        .content_height(560)
        .child(&view)
        .build();

    let loader = daemon.clone();
    call(
        async move { loader.list_notification_apps().await },
        move |result| match result {
            Err(e) => status.set_title(&gtk::glib::markup_escape_text(&e)),
            Ok(apps) if apps.is_empty() => {
                status.set_title("No app has shown a notification yet");
            }
            Ok(mut apps) => {
                list.remove(&status);
                // Most recently active first, then by name.
                apps.sort_by(|a, b| {
                    b.last_seen
                        .cmp(&a.last_seen)
                        .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
                });
                for app in apps {
                    list.add(&row(&app, &daemon, &toasts));
                }
            }
        },
    );
    dialog.present(Some(parent));
}

fn row(
    app: &NotificationApp,
    daemon: &DaemonProxy<'static>,
    toasts: &adw::ToastOverlay,
) -> adw::SwitchRow {
    let row = adw::SwitchRow::builder()
        .title(gtk::glib::markup_escape_text(&app.name).as_str())
        .subtitle(last_seen(app.last_seen))
        .active(app.send)
        .build();
    let (daemon, toasts, name) = (daemon.clone(), toasts.clone(), app.name.clone());
    row.connect_active_notify(move |row| {
        let (daemon, name, send) = (daemon.clone(), name.clone(), row.is_active());
        let toasts = toasts.clone();
        call(
            async move { daemon.set_notification_app_send(&name, send).await },
            move |result| {
                if let Err(e) = result {
                    toasts.add_toast(adw::Toast::new(&e));
                }
            },
        );
    });
    row
}
