//! `pairly-gtk --screen <name> [--pc]`: shows a phone's (or, with `--pc`, another PC's) screen
//! and controls it.
//!
//! pairlyd starts this when you ask to see a device's screen. It writes the stream on stdin as
//! messages `[kind u8][length u32 BE][payload]`:
//! - 1 started: `[width u32][height u32]`;
//! - 2 frame: `[flags u8][H.264 access unit]` (flag 1: key frame);
//! - 3 stopped: the reason, UTF-8;
//! - 4 sound: `[rate u32][channels u8][16-bit samples]`, played as it comes.
//!
//! What you do in the window goes back on stdout, one line each (positions are fractions of
//! the video). A phone gets touches: `tap x y`, `long x y`, `swipe <ms> x1 y1 x2 y2 …`,
//! `key back|home|recents|enter|backspace`, `text <text>`. A PC gets the mouse and keyboard:
//! `move x y`, `button left|right|middle press|release x y`, `wheel dx dy`,
//! `combo <ctrl+alt+shift+super or -> [key <name>|text <text>]` and, in Control mode,
//! `gesture <fingers> left|right|up|down`. Then `stop`. Closing the window ends the stream.
//!
//! Control mode (another PC): this desktop's own shortcuts and touchpad gestures (Super+E,
//! Alt+Tab, a 4-finger swipe…) go to the other PC too. It's on while the toggle is, or while
//! the window is full screen, and only while the window has focus; Ctrl+Alt pressed and
//! released on their own turns it off.
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
/// A touchpad swipe shorter than this (pixels) isn't a gesture.
const SWIPE_DISTANCE: f64 = 80.0;

/// Control mode: every shortcut and gesture goes to the other PC.
struct Control {
    on: Cell<bool>,
    window: adw::Window,
    toggle: gtk::ToggleButton,
    banner: adw::Banner,
}

impl Control {
    fn set(&self, on: bool) {
        if self.on.replace(on) == on {
            return;
        }
        self.toggle.set_active(on);
        self.banner.set_revealed(on);
        // Asks the desktop to pass its own shortcuts (and, on Hyprland, its gestures) to
        // this window while it has focus.
        if let Some(toplevel) = self.window.surface().and_downcast::<gtk::gdk::Toplevel>() {
            if on {
                toplevel.inhibit_system_shortcuts(None::<&gtk::gdk::Event>);
            } else {
                toplevel.restore_system_shortcuts();
            }
        }
        if !on && self.window.is_fullscreen() {
            self.window.unfullscreen();
        }
    }
}

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

pub fn run(title: &str, pc: bool) {
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
    let waiting = if pc {
        "Waiting for the PC… it may ask which screen to share."
    } else {
        "Waiting for the phone… accept “Start recording” on it."
    };
    let status = gtk::Label::builder()
        .label(waiting)
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
    let window_title = adw::WindowTitle::new(title, if pc { "PC screen" } else { "Phone screen" });
    header.set_title_widget(Some(&window_title));
    // Keys this desktop keeps for itself (the Super key, Alt+Tab) as buttons.
    let buttons: &[(&str, &str, &str)] = if pc {
        &[
            (
                "view-app-grid-symbolic",
                "Start Menu (Super key)",
                "combo super",
            ),
            (
                "focus-windows-symbolic",
                "Switch Windows (Alt+Tab)",
                "combo alt key tab",
            ),
        ]
    } else {
        &[
            ("go-previous-symbolic", "Back", "key back"),
            ("go-home-symbolic", "Home", "key home"),
            ("view-app-grid-symbolic", "Recent Apps", "key recents"),
        ]
    };
    for &(icon, tip, line) in buttons {
        // Never keyboard-focused: Space and Enter are for typing on the other device, not for
        // pressing these.
        let button = gtk::Button::builder()
            .icon_name(icon)
            .tooltip_text(tip)
            .focusable(false)
            .focus_on_click(false)
            .build();
        button.connect_clicked(move |_| emit(line));
        header.pack_start(&button);
    }
    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    // Control mode and full screen (another PC only).
    let control_toggle = gtk::ToggleButton::builder()
        .icon_name("input-keyboard-symbolic")
        .tooltip_text(
            "Control Mode: every shortcut and gesture goes to this PC (Ctrl+Alt to release)",
        )
        .focusable(false)
        .focus_on_click(false)
        .build();
    let fullscreen = gtk::Button::builder()
        .icon_name("view-fullscreen-symbolic")
        .tooltip_text("Full Screen (Control Mode on)")
        .focusable(false)
        .focus_on_click(false)
        .build();
    let banner = adw::Banner::builder()
        .title(format!(
            "Controlling {title}: every shortcut and gesture goes there. Ctrl+Alt gives them back."
        ))
        .button_label("Release")
        .revealed(false)
        .build();
    if pc {
        header.pack_end(&fullscreen);
        header.pack_end(&control_toggle);
        view.add_top_bar(&banner);
    }
    view.set_content(Some(&stack));
    let (width, height) = if pc { (1280, 800) } else { (420, 880) };
    let window = adw::Window::builder()
        .title(format!(
            "{title} – {} Screen",
            if pc { "PC" } else { "Phone" }
        ))
        .default_width(width)
        .default_height(height)
        .content(&view)
        .build();

    // The video's size, for mapping the pointer onto it.
    let video = Rc::new(Cell::new((0_u32, 0_u32)));
    if pc {
        let control = Rc::new(Control {
            on: Cell::new(false),
            window: window.clone(),
            toggle: control_toggle.clone(),
            banner: banner.clone(),
        });
        control_toggle.connect_toggled({
            let control = control.clone();
            move |t| control.set(t.is_active())
        });
        banner.connect_button_clicked({
            let control = control.clone();
            move |_| control.set(false)
        });
        fullscreen.connect_clicked({
            let window = window.clone();
            move |_| window.fullscreen()
        });
        // Full screen means working on the other PC: Control mode with it.
        window.connect_fullscreened_notify({
            let control = control.clone();
            move |w| control.set(w.is_fullscreen())
        });
        connect_pc_input(&picture, &window, &video, &control);
    } else {
        connect_input(&picture, &window, &video);
    }

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
                        window_title.set_subtitle(if pc {
                            "Use the mouse and keyboard to control it"
                        } else {
                            "Click, drag and scroll to control it"
                        });
                        // Fit a window of the screen's shape.
                        if pc && w > 0 {
                            let height = i32::try_from(u64::from(h) * 1280 / u64::from(w))
                                .unwrap_or(800)
                                .min(900);
                            window.set_default_size(1280, height.max(300));
                        } else if h > 0 {
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
    let mut sound: Option<Sound> = None;
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
            4 if body.len() > 5 => {
                let rate = u32::from_be_bytes([body[0], body[1], body[2], body[3]]);
                let channels = body[4];
                if sound
                    .as_ref()
                    .is_none_or(|s| (s.rate, s.channels) != (rate, channels))
                {
                    sound = Sound::new(rate, channels);
                }
                if let Some(s) = &sound {
                    let _ = s
                        .src
                        .push_buffer(gstreamer::Buffer::from_mut_slice(body.split_off(5)));
                }
            }
            _ => {}
        }
    }
    if let Some(s) = sound {
        let _ = s.pipeline.set_state(gstreamer::State::Null);
    }
    let _ = src.end_of_stream();
    tx.send(Msg::Stopped("the connection closed".into())).ok();
}

/// The shared device's sound, played as it arrives: a short queue that drops the oldest when
/// it falls behind, so the sound stays with the picture.
struct Sound {
    rate: u32,
    channels: u8,
    src: gstreamer_app::AppSrc,
    pipeline: gstreamer::Pipeline,
}

impl Sound {
    fn new(rate: u32, channels: u8) -> Option<Self> {
        let description = format!(
            "appsrc name=sound is-live=true format=time do-timestamp=true \
               caps=audio/x-raw,format=S16LE,layout=interleaved,rate={rate},channels={channels} \
             ! queue max-size-time=200000000 max-size-buffers=0 max-size-bytes=0 leaky=downstream \
             ! audioconvert ! audioresample ! autoaudiosink sync=false"
        );
        let pipeline = gstreamer::parse::launch(&description)
            .ok()?
            .downcast::<gstreamer::Pipeline>()
            .ok()?;
        let src = pipeline
            .by_name("sound")?
            .downcast::<gstreamer_app::AppSrc>()
            .ok()?;
        pipeline.set_state(gstreamer::State::Playing).ok()?;
        Some(Self {
            rate,
            channels,
            src,
            pipeline,
        })
    }
}

/// Where `(x, y)` in the picture falls on the video, as fractions (beyond 0–1 outside it);
/// `None` before the video starts.
fn video_point(picture: &gtk::Picture, video: (u32, u32), x: f64, y: f64) -> Option<(f64, f64)> {
    let (vw, vh) = (f64::from(video.0), f64::from(video.1));
    let (w, h) = (f64::from(picture.width()), f64::from(picture.height()));
    if vw <= 0.0 || vh <= 0.0 || w <= 0.0 || h <= 0.0 {
        return None;
    }
    let scale = (w / vw).min(h / vh);
    let (dw, dh) = (vw * scale, vh * scale);
    let (ox, oy) = ((w - dw) / 2.0, (h - dh) / 2.0);
    Some(((x - ox) / dw, (y - oy) / dh))
}

/// Where `(x, y)` in the picture falls on the video, as fractions; `None` outside it.
fn to_video(picture: &gtk::Picture, video: (u32, u32), x: f64, y: f64) -> Option<(f64, f64)> {
    video_point(picture, video, x, y)
        .filter(|(fx, fy)| (0.0..=1.0).contains(fx) && (0.0..=1.0).contains(fy))
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
            Key::Delete | Key::KP_Delete => "key delete".to_owned(),
            Key::Left | Key::KP_Left => "key left".to_owned(),
            Key::Right | Key::KP_Right => "key right".to_owned(),
            Key::Up | Key::KP_Up => "key up".to_owned(),
            Key::Down | Key::KP_Down => "key down".to_owned(),
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

/// The mouse button a GTK button number is, by name.
fn button_name(button: u32) -> Option<&'static str> {
    match button {
        1 => Some("left"),
        2 => Some("middle"),
        3 => Some("right"),
        _ => None,
    }
}

/// Held modifiers as `ctrl+alt+shift+super` (or `-` for none).
fn modifiers_word(state: gtk::gdk::ModifierType) -> String {
    use gtk::gdk::ModifierType as M;
    let held: Vec<&str> = [
        (M::CONTROL_MASK, "ctrl"),
        (M::ALT_MASK, "alt"),
        (M::SHIFT_MASK, "shift"),
        (M::SUPER_MASK, "super"),
    ]
    .into_iter()
    .filter_map(|(mask, name)| state.contains(mask).then_some(name))
    .collect();
    if held.is_empty() {
        "-".to_owned()
    } else {
        held.join("+")
    }
}

/// A key that isn't text, by the name pairlyd knows.
fn special_name(key: gtk::gdk::Key) -> Option<String> {
    use gtk::gdk::Key;
    let name = match key {
        Key::Return | Key::KP_Enter => "enter",
        Key::BackSpace => "backspace",
        Key::Delete | Key::KP_Delete => "delete",
        Key::Tab | Key::ISO_Left_Tab => "tab",
        Key::Escape => "escape",
        Key::Left | Key::KP_Left => "left",
        Key::Right | Key::KP_Right => "right",
        Key::Up | Key::KP_Up => "up",
        Key::Down | Key::KP_Down => "down",
        Key::Home | Key::KP_Home => "home",
        Key::End | Key::KP_End => "end",
        Key::Page_Up | Key::KP_Page_Up => "page_up",
        Key::Page_Down | Key::KP_Page_Down => "page_down",
        Key::space => "space",
        Key::AudioRaiseVolume => "volume_up",
        Key::AudioLowerVolume => "volume_down",
        Key::AudioMute => "mute",
        _ => {
            // F1 … F24.
            let name = key.name()?;
            let n: u8 = name.strip_prefix('F')?.parse().ok()?;
            return (1..=24).contains(&n).then(|| format!("f{n}"));
        }
    };
    Some(name.to_owned())
}

/// What a key press sends to another PC, if anything: special keys and shortcuts with the
/// modifiers held, plain characters as text (Shift is already in them).
fn pc_key_line(key: gtk::gdk::Key, state: gtk::gdk::ModifierType) -> Option<String> {
    let mods = modifiers_word(state);
    let shortcut = ["ctrl", "alt", "super"].iter().any(|m| mods.contains(m));
    if let Some(name) = special_name(key) {
        // A plain Space is text.
        return Some(if name == "space" && !shortcut {
            "combo - text  ".to_owned()
        } else {
            format!("combo {mods} key {name}")
        });
    }
    let c = key.to_unicode().filter(|c| !c.is_control())?;
    Some(if shortcut {
        // Ctrl+Shift+T comes as "T": the key is "t", Shift is in the modifiers.
        format!("combo {mods} text {}", c.to_lowercase())
    } else {
        format!("combo - text {c}")
    })
}

/// A mouse button held down on another PC, and where (on the video).
type HeldButton = (&'static str, (f64, f64));

/// Another PC: the pointer, its buttons, the wheel and the keyboard (shortcuts included) go
/// there as they are.
fn connect_pc_input(
    picture: &gtk::Picture,
    window: &adw::Window,
    video: &Rc<Cell<(u32, u32)>>,
    control: &Rc<Control>,
) {
    // On the video, held to its edges (a drag may go past them).
    let point = {
        let (picture, video) = (picture.clone(), video.clone());
        move |x: f64, y: f64| {
            video_point(&picture, video.get(), x, y)
                .map(|(fx, fy)| (fx.clamp(0.0, 1.0), fy.clamp(0.0, 1.0)))
        }
    };
    // Buttons held down there (and where), let go if this window loses focus mid-drag.
    let held: Rc<RefCell<Vec<HeldButton>>> = Rc::default();

    let motion = gtk::EventControllerMotion::new();
    motion.connect_motion({
        let (picture, video, point, held) =
            (picture.clone(), video.clone(), point.clone(), held.clone());
        move |_, x, y| {
            // Outside the video only while dragging.
            let at = if held.borrow().is_empty() {
                to_video(&picture, video.get(), x, y)
            } else {
                point(x, y)
            };
            if let Some((fx, fy)) = at {
                emit(&format!("move {fx:.4} {fy:.4}"));
            }
        }
    });
    picture.add_controller(motion);

    // Any button: pressed where it happens, released where the pointer is then.
    let drag = gtk::GestureDrag::new();
    drag.set_button(0);
    drag.connect_drag_begin({
        let (point, held) = (point.clone(), held.clone());
        move |gesture, x, y| {
            let (Some(name), Some((fx, fy))) = (button_name(gesture.current_button()), point(x, y))
            else {
                return;
            };
            held.borrow_mut().push((name, (fx, fy)));
            emit(&format!("button {name} press {fx:.4} {fy:.4}"));
        }
    });
    drag.connect_drag_end({
        let held = held.clone();
        move |gesture, dx, dy| {
            let Some(name) = button_name(gesture.current_button()) else {
                return;
            };
            held.borrow_mut().retain(|(b, _)| *b != name);
            let at = gesture
                .start_point()
                .and_then(|(x, y)| point(x + dx, y + dy));
            if let Some((fx, fy)) = at {
                emit(&format!("button {name} release {fx:.4} {fy:.4}"));
            }
        }
    });
    picture.add_controller(drag);
    window.connect_is_active_notify({
        let held = held.clone();
        move |w| {
            if !w.is_active() {
                for (name, (fx, fy)) in held.borrow_mut().drain(..) {
                    emit(&format!("button {name} release {fx:.4} {fy:.4}"));
                }
            }
        }
    });

    let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
    scroll.connect_scroll(|_, dx, dy| {
        emit(&format!("wheel {dx:.3} {dy:.3}"));
        gtk::glib::Propagation::Stop
    });
    picture.add_controller(scroll);

    // Keys go over as they're pressed. The Super key alone (pressed and let go with nothing in
    // between) opens the Start menu or overview there. Captured before any widget sees them.
    let lone_super = Rc::new(Cell::new(false));
    let release = Rc::new(RefCell::new(CtrlAlt::default()));
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    keys.connect_key_pressed({
        let (lone_super, release) = (lone_super.clone(), release.clone());
        move |_, key, _, state| {
            use gtk::gdk::Key;
            release.borrow_mut().pressed(key);
            let is_super = matches!(key, Key::Super_L | Key::Super_R);
            lone_super.set(is_super);
            let modifier = matches!(
                key,
                Key::Control_L
                    | Key::Control_R
                    | Key::Shift_L
                    | Key::Shift_R
                    | Key::Alt_L
                    | Key::Alt_R
                    | Key::Meta_L
                    | Key::Meta_R
                    | Key::ISO_Level3_Shift
                    | Key::Caps_Lock
                    | Key::Num_Lock
            );
            if !(is_super || modifier) {
                match pc_key_line(key, state) {
                    Some(line) => emit(&line),
                    None => return gtk::glib::Propagation::Proceed,
                }
            }
            gtk::glib::Propagation::Stop
        }
    });
    keys.connect_key_released({
        let control = control.clone();
        move |_, key, _, _| {
            use gtk::gdk::Key;
            if matches!(key, Key::Super_L | Key::Super_R) && lone_super.replace(false) {
                emit("combo super");
            }
            // Ctrl+Alt on their own: out of Control mode (Ctrl+Alt+anything goes there).
            if release.borrow_mut().released(key) && control.on.get() {
                control.set(false);
            }
        }
    });
    window.add_controller(keys);

    // Touchpad swipes with several fingers, in Control mode (the desktop leaves them to this
    // window then): one gesture each, by where the fingers went.
    let swipe = Rc::new(Cell::new((0_u32, 0.0_f64, 0.0_f64)));
    let gestures = gtk::EventControllerLegacy::new();
    gestures.connect_event({
        let control = control.clone();
        move |_, event| {
            use gtk::gdk::TouchpadGesturePhase as Phase;
            let Some(event) = event.downcast_ref::<gtk::gdk::TouchpadEvent>() else {
                return gtk::glib::Propagation::Proceed;
            };
            if event.event_type() != gtk::gdk::EventType::TouchpadSwipe {
                return gtk::glib::Propagation::Proceed;
            }
            let (fingers, x, y) = swipe.get();
            match event.gesture_phase() {
                Phase::Begin => swipe.set((event.n_fingers(), 0.0, 0.0)),
                Phase::Update => {
                    let (dx, dy) = event.deltas();
                    swipe.set((fingers, x + dx, y + dy));
                }
                Phase::End => {
                    if let Some(line) = gesture_line(fingers, x, y)
                        && control.on.get()
                    {
                        emit(&line);
                    }
                    swipe.set((0, 0.0, 0.0));
                }
                _ => swipe.set((0, 0.0, 0.0)),
            }
            gtk::glib::Propagation::Stop
        }
    });
    window.add_controller(gestures);
}

/// A finished touchpad swipe as a line for pairlyd: 3 or more fingers, far enough.
fn gesture_line(fingers: u32, dx: f64, dy: f64) -> Option<String> {
    if fingers < 3 || dx.hypot(dy) < SWIPE_DISTANCE {
        return None;
    }
    let direction = if dx.abs() >= dy.abs() {
        if dx < 0.0 { "left" } else { "right" }
    } else if dy < 0.0 {
        "up"
    } else {
        "down"
    };
    Some(format!("gesture {} {direction}", fingers.min(5)))
}

/// Ctrl and Alt pressed together and let go with no other key in between.
#[derive(Default)]
struct CtrlAlt {
    ctrl: bool,
    alt: bool,
    both: bool,
    other: bool,
}

impl CtrlAlt {
    fn pressed(&mut self, key: gtk::gdk::Key) {
        use gtk::gdk::Key;
        match key {
            Key::Control_L | Key::Control_R => self.ctrl = true,
            Key::Alt_L | Key::Alt_R => self.alt = true,
            _ => self.other = true,
        }
        if self.ctrl && self.alt {
            self.both = true;
        }
    }

    /// Whether this release completes a lone Ctrl+Alt.
    fn released(&mut self, key: gtk::gdk::Key) -> bool {
        use gtk::gdk::Key;
        match key {
            Key::Control_L | Key::Control_R => self.ctrl = false,
            Key::Alt_L | Key::Alt_R => self.alt = false,
            _ => return false,
        }
        if self.ctrl || self.alt {
            return false;
        }
        let lone = self.both && !self.other;
        *self = Self::default();
        lone
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gtk::gdk::Key;

    #[test]
    fn ctrl_alt_alone_releases_but_ctrl_alt_t_doesnt() {
        let mut c = CtrlAlt::default();
        c.pressed(Key::Control_L);
        c.pressed(Key::Alt_L);
        assert!(!c.released(Key::Alt_L), "Ctrl still held");
        assert!(
            c.released(Key::Control_L),
            "both let go, nothing else pressed"
        );

        // Ctrl+Alt+T: a shortcut for the other PC.
        c.pressed(Key::Control_L);
        c.pressed(Key::Alt_L);
        c.pressed(Key::t);
        assert!(!c.released(Key::t));
        assert!(!c.released(Key::Alt_L));
        assert!(!c.released(Key::Control_L));

        // Ctrl alone, or Alt alone: nothing.
        c.pressed(Key::Control_L);
        assert!(!c.released(Key::Control_L));
        c.pressed(Key::Alt_R);
        assert!(!c.released(Key::Alt_R));
    }

    #[test]
    fn swipes_become_gestures() {
        assert_eq!(
            gesture_line(4, -200.0, 10.0).as_deref(),
            Some("gesture 4 left")
        );
        assert_eq!(
            gesture_line(3, 5.0, -150.0).as_deref(),
            Some("gesture 3 up")
        );
        assert_eq!(gesture_line(2, -200.0, 0.0), None, "two fingers scroll");
        assert_eq!(gesture_line(4, -20.0, 5.0), None, "too short");
    }
}
