//! `pairly-gtk --laser`: the presentation laser pointer, a glowing red dot over everything on
//! screen (full-screen slides included) that clicks pass through.
//!
//! pairlyd starts this when a phone first points and writes one command per line on stdin:
//! `show`, `move <dx> <dy>` (fractions of the screen), `hide` and `quit`. The dot is drawn on
//! a layer-shell overlay, so it needs a compositor with wlr-layer-shell (Hyprland, Sway, KDE);
//! GNOME doesn't offer one.

use std::cell::Cell;
use std::io::BufRead;
use std::rc::Rc;

use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use relm4::gtk;
use relm4::gtk::prelude::*;

/// Radius of the glow, and of the bright core, in pixels.
const GLOW: f64 = 22.0;
const CORE: f64 = 6.0;

pub fn run() {
    if let Err(e) = gtk::init() {
        eprintln!("pairly-gtk --laser: can't open the display: {e}");
        std::process::exit(1);
    }
    if !gtk4_layer_shell::is_supported() {
        eprintln!("pairly-gtk --laser: this desktop has no overlay layer (wlr-layer-shell)");
        std::process::exit(2);
    }
    let provider = gtk::CssProvider::new();
    provider.load_from_string("window.pairly-laser { background: transparent; }");
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }

    let window = gtk::Window::new();
    window.add_css_class("pairly-laser");
    window.init_layer_shell();
    window.set_layer(Layer::Overlay);
    window.set_namespace(Some("pairly-laser"));
    for edge in [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom] {
        window.set_anchor(edge, true);
    }
    window.set_exclusive_zone(-1);
    window.set_keyboard_mode(KeyboardMode::None);

    // Where the dot is, as fractions of the screen.
    let pos = Rc::new(Cell::new((0.5_f64, 0.5_f64)));
    let area = gtk::DrawingArea::new();
    area.set_draw_func({
        let pos = pos.clone();
        move |_, cr, w, h| {
            let (fx, fy) = pos.get();
            let (x, y) = (fx * f64::from(w), fy * f64::from(h));
            let glow = gtk::cairo::RadialGradient::new(x, y, CORE, x, y, GLOW);
            glow.add_color_stop_rgba(0.0, 1.0, 0.05, 0.05, 0.55);
            glow.add_color_stop_rgba(1.0, 1.0, 0.0, 0.0, 0.0);
            let _ = cr.set_source(&glow);
            cr.arc(x, y, GLOW, 0.0, std::f64::consts::TAU);
            let _ = cr.fill();
            cr.set_source_rgb(1.0, 0.1, 0.1);
            cr.arc(x, y, CORE, 0.0, std::f64::consts::TAU);
            let _ = cr.fill();
            cr.set_source_rgba(1.0, 0.85, 0.85, 0.9);
            cr.arc(x, y, CORE / 3.0, 0.0, std::f64::consts::TAU);
            let _ = cr.fill();
        }
    });
    window.set_child(Some(&area));
    // Click-through: an empty input region, so the presentation underneath gets every click.
    let pass_through = |w: &gtk::Window| {
        if let Some(surface) = w.surface() {
            surface.set_input_region(Some(&gtk::cairo::Region::create()));
        }
    };
    window.connect_realize(pass_through);
    window.connect_map(pass_through);

    let main_loop = gtk::glib::MainLoop::new(None, false);
    // Commands arrive on stdin; a small thread reads them and hands each line to GTK.
    let (tx, rx) = relm4::channel::<String>();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            tx.send(line).ok();
        }
        // pairlyd went away: stop too.
        tx.send("quit".into()).ok();
    });
    gtk::glib::spawn_future_local({
        let main_loop = main_loop.clone();
        async move {
            while let Some(line) = rx.recv().await {
                let mut words = line.split_whitespace();
                match words.next() {
                    Some("show") => {
                        pos.set((0.5, 0.5));
                        area.queue_draw();
                        window.set_visible(true);
                    }
                    Some("move") => {
                        let mut next = || words.next().and_then(|w| w.parse::<f64>().ok());
                        if let (Some(dx), Some(dy)) = (next(), next()) {
                            let (x, y) = pos.get();
                            pos.set(((x + dx).clamp(0.0, 1.0), (y + dy).clamp(0.0, 1.0)));
                            area.queue_draw();
                        }
                    }
                    Some("hide") => window.set_visible(false),
                    Some("quit") => break,
                    _ => {}
                }
            }
            main_loop.quit();
        }
    });
    main_loop.run();
}
