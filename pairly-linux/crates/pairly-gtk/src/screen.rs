//! `pairly-gtk --screen <name>`: shows a phone's screen and controls it.
//!
//! pairlyd starts this when you ask to see a phone's screen. It writes the stream on stdin as
//! messages `[kind u8][length u32 BE][payload]`:
//! - 1 started: `[width u32][height u32]`;
//! - 2 frame: `[flags u8][H.264 access unit]` (flag 1: key frame);
//! - 3 stopped: the reason, UTF-8.
//!
//! What you do in the window goes back on stdout, one line each (positions are fractions of
//! the video): `tap x y`, `long x y`, `swipe <ms> x1 y1 x2 y2 …`, `key back|home|recents|enter|
//! backspace`, `text <text>` and `stop`. Closing the window ends the stream.
//!
//! Video is decoded by GStreamer (hardware decoding when the GPU has it) into a GTK picture.

use std::cell::{Cell, RefCell};
use std::io::{Read, Write};
use std::rc::Rc;
use std::time::Instant;

use gstreamer::prelude::*;
use relm4::adw::prelude::*;
use relm4::{adw, gtk};

/// Pressing longer than this without moving is a long-press.
const LONG_PRESS_MS: u128 = 500;
/// Moving less than this (pixels) is still a tap.
const TAP_SLOP: f64 = 8.0;
/// Most points kept for one swipe.
const MAX_POINTS: usize = 120;

enum Msg {
    Started(u32, u32),
    Stopped(String),
}

/// One line to pairlyd. A closed stdout means pairlyd is gone; quit then.
fn emit(line: &str) {
    let mut out = std::io::stdout().lock();
    if writeln!(out, "{line}").and_then(|()| out.flush()).is_err() {
        std::process::exit(0);
    }
}

pub fn run(title: &str) {
    if let Err(e) = gtk::init() {
        eprintln!("pairly-gtk --screen: can't open the display: {e}");
        std::process::exit(1);
    }
    let _ = adw::init();
    if let Err(e) = gstreamer::init() {
        eprintln!("pairly-gtk --screen: GStreamer: {e}");
        std::process::exit(1);
    }
    let pipeline = match gstreamer::parse::launch(
        "appsrc name=src is-live=true format=time do-timestamp=true \
           caps=video/x-h264,stream-format=byte-stream,alignment=au \
         ! h264parse ! decodebin3 ! videoconvert ! gtk4paintablesink name=sink sync=false",
    ) {
        Ok(p) => p.downcast::<gstreamer::Pipeline>().expect("a pipeline"),
        Err(e) => {
            eprintln!(
                "pairly-gtk --screen: no video decoder ({e}); install gst-plugin-gtk4 and gst-libav"
            );
            std::process::exit(1);
        }
    };
    let src = pipeline
        .by_name("src")
        .and_then(|e| e.downcast::<gstreamer_app::AppSrc>().ok())
        .expect("appsrc");
    let paintable: gtk::gdk::Paintable = pipeline
        .by_name("sink")
        .expect("sink")
        .property("paintable");

    let picture = gtk::Picture::builder()
        .paintable(&paintable)
        .content_fit(gtk::ContentFit::Contain)
        .hexpand(true)
        .vexpand(true)
        .build();
    let status = gtk::Label::builder()
        .label("Waiting for the phone… accept “Start recording” on it.")
        .wrap(true)
        .justify(gtk::Justification::Center)
        .css_classes(["title-3"])
        .margin_start(24)
        .margin_end(24)
        .build();
    let stack = gtk::Stack::new();
    stack.add_named(&status, Some("status"));
    stack.add_named(&picture, Some("video"));

    let header = adw::HeaderBar::new();
    let window_title = adw::WindowTitle::new(title, "Phone screen");
    header.set_title_widget(Some(&window_title));
    for (icon, tip, key) in [
        ("go-previous-symbolic", "Back", "back"),
        ("go-home-symbolic", "Home", "home"),
        ("view-app-grid-symbolic", "Recent Apps", "recents"),
    ] {
        // Never keyboard-focused: Space and Enter are for typing on the phone, not for
        // pressing these.
        let button = gtk::Button::builder()
            .icon_name(icon)
            .tooltip_text(tip)
            .focusable(false)
            .focus_on_click(false)
            .build();
        button.connect_clicked(move |_| emit(&format!("key {key}")));
        header.pack_start(&button);
    }
    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    view.set_content(Some(&stack));
    let window = adw::Window::builder()
        .title(format!("{title} – Phone Screen"))
        .default_width(420)
        .default_height(880)
        .content(&view)
        .build();

    // The video's size, for mapping the pointer onto it.
    let video = Rc::new(Cell::new((0_u32, 0_u32)));
    connect_input(&picture, &window, &video);

    let main_loop = gtk::glib::MainLoop::new(None, false);
    window.connect_close_request({
        let main_loop = main_loop.clone();
        move |_| {
            emit("stop");
            main_loop.quit();
            gtk::glib::Propagation::Proceed
        }
    });

    // The stream arrives on stdin: frames go straight to the decoder from the reader thread;
    // the rest is for the window.
    let (tx, rx) = relm4::channel::<Msg>();
    std::thread::spawn({
        let src = src.clone();
        move || read_stream(&src, &tx)
    });
    gtk::glib::spawn_future_local({
        let (video, window) = (video.clone(), window.clone());
        async move {
            while let Some(msg) = rx.recv().await {
                match msg {
                    Msg::Started(w, h) => {
                        video.set((w, h));
                        stack.set_visible_child_name("video");
                        window_title.set_subtitle("Click, drag and scroll to control it");
                        // Fit a window of the phone's shape on screen.
                        if h > 0 {
                            let height = 880;
                            let width =
                                i32::try_from(u64::from(w) * 880 / u64::from(h)).unwrap_or(420);
                            window.set_default_size(width.max(280), height);
                        }
                    }
                    Msg::Stopped(reason) => {
                        status.set_label(&format!("Screen sharing ended: {reason}"));
                        stack.set_visible_child_name("status");
                        window_title.set_subtitle("Stopped");
                        // Keep showing why it ended until the window is closed.
                        break;
                    }
                }
            }
        }
    });

    if pipeline.set_state(gstreamer::State::Playing).is_err() {
        eprintln!("pairly-gtk --screen: the video pipeline didn't start");
    }
    window.present();
    main_loop.run();
    let _ = pipeline.set_state(gstreamer::State::Null);
}

/// Read pairlyd's messages from stdin until it closes.
fn read_stream(src: &gstreamer_app::AppSrc, tx: &relm4::Sender<Msg>) {
    let mut stdin = std::io::stdin().lock();
    let mut head = [0_u8; 5];
    while stdin.read_exact(&mut head).is_ok() {
        let len = u32::from_be_bytes([head[1], head[2], head[3], head[4]]) as usize;
        if len > 4 * 1024 * 1024 {
            break;
        }
        let mut body = vec![0_u8; len];
        if stdin.read_exact(&mut body).is_err() {
            break;
        }
        match head[0] {
            1 if body.len() >= 8 => {
                let w = u32::from_be_bytes([body[0], body[1], body[2], body[3]]);
                let h = u32::from_be_bytes([body[4], body[5], body[6], body[7]]);
                tx.send(Msg::Started(w, h)).ok();
            }
            2 if !body.is_empty() => {
                let mut buffer = gstreamer::Buffer::from_mut_slice(body.split_off(1));
                if body[0] & 1 == 0
                    && let Some(b) = buffer.get_mut()
                {
                    b.set_flags(gstreamer::BufferFlags::DELTA_UNIT);
                }
                if src.push_buffer(buffer).is_err() {
                    break;
                }
            }
            3 => {
                tx.send(Msg::Stopped(String::from_utf8_lossy(&body).into_owned()))
                    .ok();
            }
            _ => {}
        }
    }
    let _ = src.end_of_stream();
    tx.send(Msg::Stopped("the connection closed".into())).ok();
}

/// Where `(x, y)` in the picture falls on the video, as fractions; `None` outside it.
fn to_video(picture: &gtk::Picture, video: (u32, u32), x: f64, y: f64) -> Option<(f64, f64)> {
    let (vw, vh) = (f64::from(video.0), f64::from(video.1));
    let (w, h) = (f64::from(picture.width()), f64::from(picture.height()));
    if vw <= 0.0 || vh <= 0.0 || w <= 0.0 || h <= 0.0 {
        return None;
    }
    let scale = (w / vw).min(h / vh);
    let (dw, dh) = (vw * scale, vh * scale);
    let (ox, oy) = ((w - dw) / 2.0, (h - dh) / 2.0);
    let (fx, fy) = ((x - ox) / dw, (y - oy) / dh);
    ((0.0..=1.0).contains(&fx) && (0.0..=1.0).contains(&fy)).then_some((fx, fy))
}

/// A press in progress: when it started, the points (on the video) it went through, and where
/// it began in the picture.
type Stroke = (Instant, Vec<(f64, f64)>, (f64, f64));

/// Mouse and keyboard in the window become taps, swipes, keys and text on the phone.
fn connect_input(picture: &gtk::Picture, window: &adw::Window, video: &Rc<Cell<(u32, u32)>>) {
    let stroke: Rc<RefCell<Option<Stroke>>> = Rc::default();

    let drag = gtk::GestureDrag::new();
    drag.set_button(1);
    drag.connect_drag_begin({
        let (stroke, picture, video) = (stroke.clone(), picture.clone(), video.clone());
        move |_, x, y| {
            *stroke.borrow_mut() =
                to_video(&picture, video.get(), x, y).map(|p| (Instant::now(), vec![p], (x, y)));
        }
    });
    drag.connect_drag_update({
        let (stroke, picture, video) = (stroke.clone(), picture.clone(), video.clone());
        move |_, dx, dy| {
            if let Some((_, points, start)) = stroke.borrow_mut().as_mut()
                && points.len() < MAX_POINTS
                && let Some(p) = to_video(&picture, video.get(), start.0 + dx, start.1 + dy)
            {
                points.push(p);
            }
        }
    });
    drag.connect_drag_end({
        let stroke = stroke.clone();
        move |_, dx, dy| {
            let Some((started, points, _)) = stroke.borrow_mut().take() else {
                return;
            };
            let held = started.elapsed().as_millis();
            let first = points[0];
            if dx.hypot(dy) < TAP_SLOP || points.len() < 2 {
                let kind = if held >= LONG_PRESS_MS { "long" } else { "tap" };
                emit(&format!("{kind} {:.4} {:.4}", first.0, first.1));
            } else {
                let path: Vec<String> = points
                    .iter()
                    .map(|(x, y)| format!("{x:.4} {y:.4}"))
                    .collect();
                emit(&format!(
                    "swipe {} {}",
                    held.clamp(50, 5000),
                    path.join(" ")
                ));
            }
        }
    });
    picture.add_controller(drag);

    // Right click: Back, as on many phones' mice.
    let right = gtk::GestureClick::new();
    right.set_button(3);
    right.connect_released(|_, _, _, _| emit("key back"));
    picture.add_controller(right);

    // The wheel scrolls: a short swipe from the pointer.
    let pointer = Rc::new(Cell::new((0.5_f64, 0.5_f64)));
    let motion = gtk::EventControllerMotion::new();
    motion.connect_motion({
        let (pointer, picture, video) = (pointer.clone(), picture.clone(), video.clone());
        move |_, x, y| {
            if let Some(p) = to_video(&picture, video.get(), x, y) {
                pointer.set(p);
            }
        }
    });
    picture.add_controller(motion);
    let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
    scroll.connect_scroll(move |_, _, dy| {
        let (x, y) = pointer.get();
        let to = (y - dy * 0.12).clamp(0.02, 0.98);
        emit(&format!("swipe 120 {x:.4} {y:.4} {x:.4} {to:.4}"));
        gtk::glib::Propagation::Stop
    });
    picture.add_controller(scroll);

    // Typing goes to the phone's focused text field. Captured before any widget sees the key,
    // so Space and Enter never press a button in the window.
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    keys.connect_key_pressed(|_, key, _, _| {
        use gtk::gdk::Key;
        let line = match key {
            Key::Return | Key::KP_Enter => "key enter".to_owned(),
            Key::BackSpace => "key backspace".to_owned(),
            Key::Escape => "key back".to_owned(),
            _ => match key.to_unicode().filter(|c| !c.is_control()) {
                Some(c) => format!("text {c}"),
                None => return gtk::glib::Propagation::Proceed,
            },
        };
        emit(&line);
        gtk::glib::Propagation::Stop
    });
    window.add_controller(keys);
}
