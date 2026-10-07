//! `pairly-gtk --cast <token-file>`: records this PC's screen for a phone to watch.
//!
//! On wlroots-style desktops (Hyprland, Sway, …) the focused monitor is captured directly
//! (see [`crate::screencopy`]), with no question asked. Elsewhere the screen comes from the
//! desktop's ScreenCast portal: the first time, the desktop asks which screen to share; ticking
//! "remember" there gives a restore token, kept in `<token-file>`, so later sessions start
//! without asking. The video is encoded to H.264 (on the GPU when it can) and written on stdout
//! as messages `[kind u8][length u32 BE][payload]`:
//! - 1 started: `[width u32][height u32][x i32][y i32][w u32][h u32]`, the video size, then
//!   where the shared monitor is in the desktop (for placing the pointer);
//! - 2 frame: `[flags u8][H.264 access unit]` (flag 1: key frame);
//! - 3 stopped: the reason, UTF-8.
//!
//! It ends when stdin closes (or says `stop`), or the desktop ends the sharing. A `key` line
//! on stdin asks for a key frame now (the phone's link fell behind and skipped frames).

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::PathBuf;
use std::sync::Mutex;

use futures_util::StreamExt;
use gstreamer::prelude::*;
use zbus::Connection;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

const STARTED: u8 = 1;
const FRAME: u8 = 2;
const STOPPED: u8 = 3;

/// The video's longer side at most (phones decode 1080p easily).
const MAX_WIDTH: u32 = 1920;

static OUT: Mutex<()> = Mutex::new(());

fn send(kind: u8, payload: &[u8]) {
    let _guard = OUT.lock();
    let mut out = std::io::stdout().lock();
    let len = u32::try_from(payload.len()).unwrap_or(0).to_be_bytes();
    let ok = out
        .write_all(&[kind])
        .and_then(|()| out.write_all(&len))
        .and_then(|()| out.write_all(payload))
        .and_then(|()| out.flush());
    if ok.is_err() {
        // pairlyd went away.
        std::process::exit(0);
    }
}

fn stop(reason: &str) -> ! {
    send(STOPPED, reason.as_bytes());
    std::process::exit(0);
}

pub fn run(token_file: &str) {
    if let Err(e) = gstreamer::init() {
        stop(&format!("GStreamer: {e}"));
    }
    match crate::screencopy::Capture::open() {
        Ok(capture) => direct(capture),
        Err(why) => {
            eprintln!("pairly-gtk: no direct capture ({why}), asking the desktop");
            portal(token_file);
        }
    }
}

/// Capture the monitor ourselves, about 30 times a second.
fn direct(mut capture: crate::screencopy::Capture) {
    let (w, h) = (capture.width, capture.height);
    let flip = if capture.y_invert {
        "videoflip method=vertical-flip ! "
    } else {
        ""
    };
    let description = format!("appsrc name=src ! {flip}{}", encode(w, h));
    let pipeline = match launch(&description) {
        Ok(p) => p,
        Err(e) => stop(&e),
    };
    let Some(src) = pipeline
        .by_name("src")
        .and_then(|e| e.downcast::<gstreamer_app::AppSrc>().ok())
    else {
        stop("no appsrc");
    };
    src.set_caps(Some(
        &gstreamer::Caps::builder("video/x-raw")
            .field("format", capture.format)
            .field("width", i32::try_from(w).unwrap_or(0))
            .field("height", i32::try_from(h).unwrap_or(0))
            .field("framerate", gstreamer::Fraction::new(30, 1))
            .build(),
    ));
    src.set_is_live(true);
    src.set_format(gstreamer::Format::Time);
    src.set_do_timestamp(true);
    attach(&pipeline, w, h, capture.position, capture.size);
    play(&pipeline);

    let frame_time = std::time::Duration::from_millis(33);
    let len = w as usize * h as usize * 4;
    loop {
        let begun = std::time::Instant::now();
        let mut pixels = vec![0_u8; len];
        if let Err(e) = capture.grab(&mut pixels) {
            stop(&e);
        }
        if src
            .push_buffer(gstreamer::Buffer::from_mut_slice(pixels))
            .is_err()
        {
            stop("the screen recording stopped");
        }
        if let Some(rest) = frame_time.checked_sub(begun.elapsed()) {
            std::thread::sleep(rest);
        }
    }
}

/// Record through the ScreenCast portal.
fn portal(token_file: &str) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => stop(&format!("can't start: {e}")),
    };
    let stream = match runtime.block_on(open_screencast(PathBuf::from(token_file))) {
        Ok(s) => s,
        Err(e) => stop(&e),
    };
    let (w, h) = stream.size;
    let (w, h) = (u32::try_from(w).unwrap_or(0), u32::try_from(h).unwrap_or(0));
    let description = format!(
        "pipewiresrc fd={} path={} do-timestamp=true keepalive-time=500 always-copy=true ! {}",
        stream.fd.as_raw_fd(),
        stream.node,
        encode(w, h),
    );
    let pipeline = match launch(&description) {
        Ok(p) => p,
        Err(e) => stop(&e),
    };
    attach(&pipeline, w, h, stream.position, stream.size);
    play(&pipeline);
    // The bus thread ends the process; `stream` (the portal session) lives until then.
    loop {
        std::thread::park();
    }
}

/// Start the pipeline; stdin closing (or "stop"), errors and the end of the stream end it.
fn play(pipeline: &gstreamer::Pipeline) {
    let sink = pipeline.by_name("out");
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            match line.as_deref().map(str::trim) {
                Ok("stop") | Err(_) => break,
                Ok("key") => {
                    // Travels up from the sink to the encoder.
                    let event = gstreamer::event::CustomUpstream::new(
                        gstreamer::Structure::builder("GstForceKeyUnit")
                            .field("all-headers", true)
                            .build(),
                    );
                    if let Some(sink) = &sink {
                        sink.send_event(event);
                    }
                }
                Ok(_) => {}
            }
        }
        stop("stopped on the phone");
    });
    if pipeline.set_state(gstreamer::State::Playing).is_err() {
        stop("the screen recording didn't start");
    }
    let bus = pipeline.bus().expect("a pipeline has a bus");
    std::thread::spawn(move || {
        for msg in bus.iter_timed(gstreamer::ClockTime::NONE) {
            match msg.view() {
                gstreamer::MessageView::Eos(_) => stop("the PC stopped sharing its screen"),
                gstreamer::MessageView::Error(e) => {
                    stop(&format!("screen recording failed: {}", e.error()))
                }
                _ => {}
            }
        }
    });
}

/// The shared monitor, as the portal gave it.
struct Stream {
    /// The portal ends the session when this connection closes: keep it while recording.
    _conn: Connection,
    fd: OwnedFd,
    node: u32,
    position: (i32, i32),
    size: (i32, i32),
}

/// The video size for a `w`×`h` screen: at most [`MAX_WIDTH`] wide, even sides.
fn video_size(w: u32, h: u32) -> (u32, u32) {
    let scale = (f64::from(MAX_WIDTH) / f64::from(w.max(1))).min(1.0);
    (
        ((f64::from(w) * scale) as u32 / 2) * 2,
        ((f64::from(h) * scale) as u32 / 2) * 2,
    )
}

/// The pipeline's tail: scale, encode to H.264 and hand each frame to the appsink `out`.
fn encode(w: u32, h: u32) -> String {
    // The GPU encoder if there is one, else software. 5 Mbit/s keeps text sharp at 1080p and
    // fits ordinary Wi-Fi; a key frame every 2 s (or on request) lets the phone recover.
    let encoder = if gstreamer::ElementFactory::find("vah264enc").is_some() {
        "vah264enc rate-control=cbr bitrate=5000 key-int-max=60 b-frames=0"
    } else {
        "x264enc tune=zerolatency speed-preset=ultrafast bitrate=5000 key-int-max=60"
    };
    let (vw, vh) = video_size(w, h);
    format!(
        "videoconvert ! videoscale ! videorate \
         ! video/x-raw,format=NV12,width={vw},height={vh},framerate=30/1 \
         ! {encoder} ! h264parse config-interval=-1 \
         ! video/x-h264,stream-format=byte-stream,alignment=au \
         ! appsink name=out emit-signals=true sync=false max-buffers=4 drop=true"
    )
}

fn launch(description: &str) -> Result<gstreamer::Pipeline, String> {
    gstreamer::parse::launch(description)
        .map_err(|e| format!("no screen encoder: {e}"))?
        .downcast::<gstreamer::Pipeline>()
        .map_err(|_| "not a pipeline".to_owned())
}

/// Say the stream started, then send each encoded frame.
fn attach(pipeline: &gstreamer::Pipeline, w: u32, h: u32, position: (i32, i32), size: (i32, i32)) {
    let Some(sink) = pipeline
        .by_name("out")
        .and_then(|e| e.downcast::<gstreamer_app::AppSink>().ok())
    else {
        stop("no appsink");
    };
    let (vw, vh) = video_size(w, h);
    let mut started = Vec::with_capacity(24);
    started.extend_from_slice(&vw.to_be_bytes());
    started.extend_from_slice(&vh.to_be_bytes());
    started.extend_from_slice(&position.0.to_be_bytes());
    started.extend_from_slice(&position.1.to_be_bytes());
    started.extend_from_slice(&u32::try_from(size.0).unwrap_or(0).to_be_bytes());
    started.extend_from_slice(&u32::try_from(size.1).unwrap_or(0).to_be_bytes());
    send(STARTED, &started);

    sink.set_callbacks(
        gstreamer_app::AppSinkCallbacks::builder()
            .new_sample(|sink| {
                let sample = sink.pull_sample().map_err(|_| gstreamer::FlowError::Eos)?;
                if let Some(buffer) = sample.buffer()
                    && let Ok(map) = buffer.map_readable()
                {
                    let key = !buffer.flags().contains(gstreamer::BufferFlags::DELTA_UNIT);
                    let mut payload = Vec::with_capacity(1 + map.len());
                    payload.push(u8::from(key));
                    payload.extend_from_slice(&map);
                    send(FRAME, &payload);
                }
                Ok(gstreamer::FlowSuccess::Ok)
            })
            .build(),
    );
}

/// Ask the ScreenCast portal for one monitor (with the cursor), remembering the choice.
async fn open_screencast(token_file: PathBuf) -> Result<Stream, String> {
    let conn = Connection::session().await.map_err(|e| e.to_string())?;
    let portal = zbus::Proxy::new(
        &conn,
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.ScreenCast",
    )
    .await
    .map_err(|e| format!("no screen-sharing portal: {e}"))?;

    let session: OwnedObjectPath = {
        let results = request(&conn, "CreateSession", |token| {
            let options: HashMap<&str, Value<'_>> = HashMap::from([
                ("handle_token", Value::from(token)),
                ("session_handle_token", Value::from("pairly_cast")),
            ]);
            let portal = &portal;
            async move { portal.call_method("CreateSession", &(options,)).await }
        })
        .await?;
        let path: String = results
            .get("session_handle")
            .and_then(|v| String::try_from(v.clone()).ok())
            .ok_or("the portal made no session")?;
        OwnedObjectPath::try_from(path).map_err(|e| e.to_string())?
    };

    let restore = std::fs::read_to_string(&token_file).unwrap_or_default();
    request(&conn, "SelectSources", |token| {
        let mut options: HashMap<&str, Value<'_>> = HashMap::from([
            ("handle_token", Value::from(token)),
            ("types", Value::from(1_u32)), // monitors
            ("multiple", Value::from(false)),
            ("cursor_mode", Value::from(2_u32)), // the pointer drawn into the video
            ("persist_mode", Value::from(2_u32)), // remember until revoked
        ]);
        if !restore.trim().is_empty() {
            options.insert("restore_token", Value::from(restore.trim()));
        }
        let (portal, session) = (&portal, &session);
        async move {
            portal
                .call_method("SelectSources", &(session, options))
                .await
        }
    })
    .await?;

    let results = request(&conn, "Start", |token| {
        let options: HashMap<&str, Value<'_>> =
            HashMap::from([("handle_token", Value::from(token))]);
        let (portal, session) = (&portal, &session);
        async move { portal.call_method("Start", &(session, "", options)).await }
    })
    .await?;
    if let Some(token) = results
        .get("restore_token")
        .and_then(|v| String::try_from(v.clone()).ok())
    {
        let _ = std::fs::write(&token_file, token);
    }
    type Streams = Vec<(u32, HashMap<String, OwnedValue>)>;
    let streams: Streams = results
        .get("streams")
        .and_then(|v| Streams::try_from(v.clone()).ok())
        .ok_or("no screen was chosen")?;
    let (node, props) = streams.into_iter().next().ok_or("no screen was chosen")?;
    let pair = |key: &str| {
        props
            .get(key)
            .and_then(|v| <(i32, i32)>::try_from(v.clone()).ok())
    };
    let size = pair("size").ok_or("the portal didn't say the screen's size")?;
    let position = pair("position").unwrap_or((0, 0));

    let reply = portal
        .call_method(
            "OpenPipeWireRemote",
            &(&session, HashMap::<&str, Value<'_>>::new()),
        )
        .await
        .map_err(|e| e.to_string())?;
    let fd: zbus::zvariant::OwnedFd = reply.body().deserialize().map_err(|e| e.to_string())?;
    Ok(Stream {
        _conn: conn,
        fd: fd.into(),
        node,
        position,
        size,
    })
}

/// Call a portal method that answers through a Request object, and wait for the answer.
async fn request<F, Fut>(
    conn: &Connection,
    what: &str,
    call: F,
) -> Result<HashMap<String, OwnedValue>, String>
where
    F: FnOnce(&'static str) -> Fut,
    Fut: std::future::Future<Output = zbus::Result<zbus::Message>>,
{
    // The Request's path is known in advance; listen before calling, so the answer can't be
    // missed.
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let token: &'static str = Box::leak(format!("pairly{n}").into_boxed_str());
    let sender = conn
        .unique_name()
        .ok_or("no bus name")?
        .trim_start_matches(':')
        .replace('.', "_");
    let path = format!("/org/freedesktop/portal/desktop/request/{sender}/{token}");
    let request = zbus::Proxy::new(
        conn,
        "org.freedesktop.portal.Desktop",
        ObjectPath::try_from(path).map_err(|e| e.to_string())?,
        "org.freedesktop.portal.Request",
    )
    .await
    .map_err(|e| e.to_string())?;
    let mut responses = request
        .receive_signal("Response")
        .await
        .map_err(|e| e.to_string())?;
    call(token).await.map_err(|e| format!("{what}: {e}"))?;
    let msg = responses.next().await.ok_or("the portal went away")?;
    let (code, results): (u32, HashMap<String, OwnedValue>) =
        msg.body().deserialize().map_err(|e| e.to_string())?;
    match code {
        0 => Ok(results),
        1 => Err("screen sharing was declined on the PC".into()),
        _ => Err(format!("the portal refused {what}")),
    }
}
