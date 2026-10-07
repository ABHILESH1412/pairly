//! `pairly-gtk`: libadwaita front end. A pure D-Bus client of `pairlyd`; closing the window
//! leaves the daemon (and your connections) running.
//!
//! `pairly-gtk --pair` opens straight to the pairing QR code (used by the tray menu),
//! `pairly-gtk --reply <device> <notification> <title>` opens a reply window (used by pairlyd
//! when the notification server has no inline replies), and
//! `pairly-gtk --send [--to <device>] [files…]` sends files (file managers, the tray), and
//! `pairly-gtk --laser` is the laser pointer overlay, and `pairly-gtk --screen <name>` a
//! phone's screen (both started by pairlyd).
#![forbid(unsafe_code)]

mod app;
mod cast;
mod commands;
mod contacts;
mod files;
mod laser;
mod messages;
mod notification_apps;
mod qr;
mod reply;
mod screen;
mod screencopy;
mod send;
mod settings;

use std::collections::HashMap;

use relm4::RelmApp;
use relm4::gtk::gio::prelude::FileExt;
use zbus::zvariant::Value;

const APP_ID: &str = "io.github.abhilesh1412.Pairly";
const APP_PATH: &str = "/io/github/abhilesh1412/Pairly";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| a == "--laser") {
        laser::run();
        return;
    }
    if let [flag, token_file, ..] = args.as_slice()
        && flag == "--cast"
    {
        cast::run(token_file);
        return;
    }
    if let [flag, title, ..] = args.as_slice()
        && flag == "--screen"
    {
        screen::run(title);
        return;
    }
    if let [flag, device, notification, title, ..] = args.as_slice()
        && flag == "--reply"
    {
        reply::run(device.clone(), notification.clone(), title.clone());
        return;
    }
    if args.first().is_some_and(|a| a == "--send") {
        let mut rest = args[1..].to_vec();
        let target = match rest.as_slice() {
            [flag, device, ..] if flag == "--to" => {
                let device = device.clone();
                rest.drain(..2);
                Some(device)
            }
            _ => None,
        };
        // Desktop files pass paths (%F); some file managers pass file:// URIs.
        let paths = rest
            .iter()
            .map(|a| {
                let file = gtk_file(a);
                file.path()
                    .map_or_else(|| a.clone(), |p| p.to_string_lossy().into_owned())
            })
            .collect();
        send::run(target, paths);
        return;
    }
    let pair = args.iter().any(|a| a == "--pair");
    if pair && ask_running_instance_to_pair() {
        return;
    }
    // Our flags aren't GTK's: don't hand them to GApplication.
    RelmApp::new(APP_ID)
        .with_args(Vec::new())
        .run::<app::App>(app::Init { show_pairing: pair });
}

fn gtk_file(arg: &str) -> relm4::gtk::gio::File {
    use relm4::gtk::gio;
    if arg.contains("://") {
        gio::File::for_uri(arg)
    } else {
        gio::File::for_path(std::path::absolute(arg).unwrap_or_else(|_| arg.into()))
    }
}

/// If Pairly is already open, activate its `app.pair` action over D-Bus and return true.
fn ask_running_instance_to_pair() -> bool {
    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return false;
    };
    rt.block_on(async {
        let Ok(conn) = zbus::Connection::session().await else {
            return false;
        };
        let params: Vec<Value<'_>> = Vec::new();
        let platform_data: HashMap<&str, Value<'_>> = HashMap::new();
        conn.call_method(
            Some(APP_ID),
            APP_PATH,
            Some("org.gtk.Actions"),
            "Activate",
            &("pair", params, platform_data),
        )
        .await
        .is_ok()
    })
}
