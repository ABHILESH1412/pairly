//! `pairly-gtk --laser`: the presentation laser pointer, a glowing red dot over everything on
//! screen (full-screen slides included) that clicks pass through.
//!
//! pairlyd starts this when a phone first points and writes one command per line on stdin:
//! `show`, `move <dx> <dy>` (fractions of the screen the dot is on), `hide` and `quit`. The dot
//! is drawn on a layer-shell overlay, so it needs a compositor with wlr-layer-shell (Hyprland,
//! Sway, KDE); GNOME doesn't offer one.
//!
//! Every monitor gets an overlay, and the dot travels across them like a mouse pointer. It
//! starts on the monitor showing a full-screen window (the slides on a projector), else on the
//! focused one.

use std::cell::{Cell, RefCell};
use std::io::BufRead;
use std::rc::Rc;

use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use relm4::gtk;
use relm4::gtk::prelude::*;

/// Radius of the glow, and of the bright core, in pixels.
const GLOW: f64 = 22.0;
const CORE: f64 = 6.0;

/// A monitor's place in the desktop, in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Rect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

impl Rect {
    fn contains(&self, (x, y): (f64, f64)) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }

    fn center(&self) -> (f64, f64) {
        (self.x + self.w / 2.0, self.y + self.h / 2.0)
    }

    /// The nearest point inside.
    fn clamp(&self, (x, y): (f64, f64)) -> (f64, f64) {
        (
            x.clamp(self.x, self.x + self.w - 1.0),
            y.clamp(self.y, self.y + self.h - 1.0),
        )
    }

    fn distance(&self, p: (f64, f64)) -> f64 {
        let (cx, cy) = self.clamp(p);
        (cx - p.0).hypot(cy - p.1)
    }
}

/// Where the dot goes after moving by (`dx`, `dy`), fractions of the monitor it's on: across a
/// shared edge onto the next monitor, else stopped at the edge of the desktop.
fn moved(screens: &[Rect], from: (f64, f64), dx: f64, dy: f64) -> (f64, f64) {
    let Some(here) = screens
        .iter()
        .find(|r| r.contains(from))
        .or_else(|| screens.first())
    else {
        return from;
    };
    let to = (from.0 + dx * here.w, from.1 + dy * here.h);
    if screens.iter().any(|r| r.contains(to)) {
        return to;
    }
    // Off every monitor (past the desktop's edge, or into a gap between monitors): onto the
    // nearest one.
    screens
        .iter()
        .min_by(|a, b| a.distance(to).total_cmp(&b.distance(to)))
        .map_or(from, |r| r.clamp(to))
}

/// The monitor (by connector name, like `HDMI-A-1`) to start on: on Hyprland, the one showing
/// a full-screen window, else the focused one. `None` elsewhere.
fn hyprland_start_monitor() -> Option<String> {
    std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE")?;
    let json = |what: &str| -> Option<serde_json::Value> {
        let out = std::process::Command::new("hyprctl")
            .args(["-j", what])
            .output()
            .ok()?;
        serde_json::from_slice(&out.stdout).ok()
    };
    let monitors = json("monitors")?;
    let monitors = monitors.as_array()?;
    let name_of = |id: i64| {
        monitors
            .iter()
            .find(|m| m["id"].as_i64() == Some(id))
            .and_then(|m| m["name"].as_str())
            .map(str::to_owned)
    };
    let focused = monitors
        .iter()
        .find(|m| m["focused"].as_bool() == Some(true))
        .and_then(|m| m["id"].as_i64());
    let clients = json("clients").unwrap_or_default();
    let fullscreen: Vec<i64> = clients
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| {
            c["mapped"].as_bool() == Some(true) && c["fullscreen"].as_i64().unwrap_or(0) > 0
        })
        .filter_map(|c| c["monitor"].as_i64())
        .collect();
    // A slideshow somewhere: that monitor (the focused one if several are full-screen).
    let pick = if fullscreen.contains(&focused.unwrap_or(-1)) {
        focused
    } else {
        fullscreen.first().copied().or(focused)
    };
    name_of(pick?)
}

pub fn run() {
    if let Err(e) = gtk::init() {
        eprintln!("pairly-gtk --laser: can't open the display: {e}");
        std::process::exit(1);
    }
    if !gtk4_layer_shell::is_supported() {
        eprintln!("pairly-gtk --laser: this desktop has no overlay layer (wlr-layer-shell)");
        std::process::exit(2);
    }
    let Some(display) = gtk::gdk::Display::default() else {
        eprintln!("pairly-gtk --laser: no display");
        std::process::exit(1);
    };
    let provider = gtk::CssProvider::new();
    provider.load_from_string("window.pairly-laser { background: transparent; }");
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    // The dot, in desktop coordinates (logical pixels).
    let pos = Rc::new(Cell::new((0.0_f64, 0.0_f64)));
    // One overlay per monitor: (where it is, its connector, its window, its drawing).
    type Overlay = (Rect, Option<String>, gtk::Window, gtk::DrawingArea);
    let overlays: Rc<RefCell<Vec<Overlay>>> = Rc::default();
    let visible = Rc::new(Cell::new(false));

    let build = {
        let (display, overlays, pos, visible) = (
            display.clone(),
            overlays.clone(),
            pos.clone(),
            visible.clone(),
        );
        move || {
            for (_, _, window, _) in overlays.borrow_mut().drain(..) {
                window.destroy();
            }
            let monitors = display.monitors();
            for i in 0..monitors.n_items() {
                let Some(monitor) = monitors.item(i).and_downcast::<gtk::gdk::Monitor>() else {
                    continue;
                };
                let g = monitor.geometry();
                let rect = Rect {
                    x: f64::from(g.x()),
                    y: f64::from(g.y()),
                    w: f64::from(g.width()),
                    h: f64::from(g.height()),
                };
                let (window, area) = overlay(&monitor, rect, &pos);
                window.set_visible(visible.get());
                overlays.borrow_mut().push((
                    rect,
                    monitor.connector().map(|c| c.to_string()),
                    window,
                    area,
                ));
            }
        }
    };
    build();
    // A monitor plugged in or out: overlays for the new set.
    display.monitors().connect_items_changed({
        let build = build.clone();
        move |_, _, _, _| build()
    });

    let redraw = {
        let overlays = overlays.clone();
        move || {
            for (_, _, _, area) in overlays.borrow().iter() {
                area.queue_draw();
            }
        }
    };

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
            let screens = || -> Vec<Rect> { overlays.borrow().iter().map(|o| o.0).collect() };
            while let Some(line) = rx.recv().await {
                let mut words = line.split_whitespace();
                match words.next() {
                    Some("show") => {
                        let start = hyprland_start_monitor();
                        let rects = overlays.borrow();
                        let rect = rects
                            .iter()
                            .find(|o| start.is_some() && o.1 == start)
                            .or_else(|| rects.first())
                            .map(|o| o.0);
                        drop(rects);
                        if let Some(rect) = rect {
                            pos.set(rect.center());
                        }
                        visible.set(true);
                        for (_, _, window, _) in overlays.borrow().iter() {
                            window.set_visible(true);
                        }
                        redraw();
                    }
                    Some("move") => {
                        let mut next = || words.next().and_then(|w| w.parse::<f64>().ok());
                        if let (Some(dx), Some(dy)) = (next(), next()) {
                            pos.set(moved(&screens(), pos.get(), dx, dy));
                            redraw();
                        }
                    }
                    Some("hide") => {
                        visible.set(false);
                        for (_, _, window, _) in overlays.borrow().iter() {
                            window.set_visible(false);
                        }
                    }
                    Some("quit") => break,
                    _ => {}
                }
            }
            main_loop.quit();
        }
    });
    main_loop.run();
}

/// A click-through overlay covering `monitor`, drawing the dot when it's there.
fn overlay(
    monitor: &gtk::gdk::Monitor,
    rect: Rect,
    pos: &Rc<Cell<(f64, f64)>>,
) -> (gtk::Window, gtk::DrawingArea) {
    let window = gtk::Window::new();
    window.add_css_class("pairly-laser");
    window.init_layer_shell();
    window.set_layer(Layer::Overlay);
    window.set_monitor(Some(monitor));
    window.set_namespace(Some("pairly-laser"));
    for edge in [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom] {
        window.set_anchor(edge, true);
    }
    window.set_exclusive_zone(-1);
    window.set_keyboard_mode(KeyboardMode::None);

    let area = gtk::DrawingArea::new();
    area.set_draw_func({
        let pos = pos.clone();
        move |_, cr, _, _| {
            let (gx, gy) = pos.get();
            // Also near the edge, so the glow continues across onto the next monitor.
            if gx < rect.x - GLOW
                || gx > rect.x + rect.w + GLOW
                || gy < rect.y - GLOW
                || gy > rect.y + rect.h + GLOW
            {
                return;
            }
            let (x, y) = (gx - rect.x, gy - rect.y);
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
    (window, area)
}

#[cfg(test)]
mod tests {
    use super::*;

    // A laptop (1920×1080) with a projector (1280×720) on its right.
    const LAPTOP: Rect = Rect {
        x: 0.0,
        y: 0.0,
        w: 1920.0,
        h: 1080.0,
    };
    const PROJECTOR: Rect = Rect {
        x: 1920.0,
        y: 0.0,
        w: 1280.0,
        h: 720.0,
    };

    #[test]
    fn the_dot_crosses_onto_the_next_monitor() {
        let screens = [LAPTOP, PROJECTOR];
        // From the projector's middle, half its width to the left: onto the laptop.
        let p = moved(&screens, PROJECTOR.center(), -0.75, 0.0);
        assert!(LAPTOP.contains(p), "{p:?}");
        // Back right across the edge: on the projector, moving by its own size.
        let p = moved(&screens, (1900.0, 300.0), 0.1, 0.0);
        assert!(PROJECTOR.contains(p), "{p:?}");
    }

    #[test]
    fn the_dot_stays_on_the_desktop() {
        let screens = [LAPTOP, PROJECTOR];
        // Far past the right edge: at the projector's edge.
        let p = moved(&screens, PROJECTOR.center(), 5.0, 0.0);
        assert!(PROJECTOR.contains(p) && p.0 > 3100.0, "{p:?}");
        // Down from the projector into the space below it (it's shorter than the laptop):
        // kept on a monitor.
        let p = moved(&screens, PROJECTOR.center(), 0.0, 1.0);
        assert!(screens.iter().any(|r| r.contains(p)), "{p:?}");
        // One monitor only: as before, the dot stops at the edges.
        assert_eq!(moved(&[LAPTOP], LAPTOP.center(), -2.0, 0.0), (0.0, 540.0));
    }
}
