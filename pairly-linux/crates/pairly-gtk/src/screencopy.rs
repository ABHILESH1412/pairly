//! Grabbing a monitor straight from the compositor with wlr-screencopy (Hyprland, Sway, river,
//! …). It needs no portal, so the desktop doesn't ask "what to share" every time: the phone was
//! paired, and the PC shows a notification while it watches.
//!
//! Frames are copied into a shared-memory buffer and read back as packed rows.

use std::fs::File;
use std::os::fd::AsFd;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;

use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, WEnum, delegate_noop};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};

#[derive(Default)]
struct Output {
    name: String,
    position: (i32, i32),
    size: (i32, i32),
}

/// What the compositor said about the frame being captured.
#[derive(Default)]
struct Frame {
    /// The shared-memory layout it wants: format, width, height, stride.
    shm: Option<(wl_shm::Format, u32, u32, u32)>,
    buffer_done: bool,
    y_invert: bool,
    ready: bool,
    failed: bool,
}

#[derive(Default)]
struct State {
    outputs: Vec<Output>,
    frame: Frame,
}

impl Dispatch<wl_output::WlOutput, usize> for State {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(output) = state.outputs.get_mut(*index) else {
            return;
        };
        match event {
            wl_output::Event::Geometry { x, y, .. } => output.position = (x, y),
            wl_output::Event::Mode {
                flags: WEnum::Value(flags),
                width,
                height,
                ..
            } if flags.contains(wl_output::Mode::Current) => output.size = (width, height),
            wl_output::Event::Name { name } => output.name = name,
            _ => {}
        }
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwlr_screencopy_frame_v1::Event;
        let frame = &mut state.frame;
        match event {
            Event::Buffer {
                format: WEnum::Value(format),
                width,
                height,
                stride,
            } if frame.shm.is_none() && gst_format(format).is_some() => {
                frame.shm = Some((format, width, height, stride));
            }
            Event::Flags {
                flags: WEnum::Value(flags),
            } => frame.y_invert = flags.contains(zwlr_screencopy_frame_v1::Flags::YInvert),
            Event::BufferDone => frame.buffer_done = true,
            Event::Ready { .. } => frame.ready = true,
            Event::Failed => frame.failed = true,
            _ => {}
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(State: ZwlrScreencopyManagerV1);
delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: wl_shm_pool::WlShmPool);
delegate_noop!(State: ignore wl_buffer::WlBuffer);

/// The GStreamer name of a shared-memory format (little-endian, so the bytes are reversed).
fn gst_format(format: wl_shm::Format) -> Option<&'static str> {
    match format {
        wl_shm::Format::Xrgb8888 => Some("BGRx"),
        wl_shm::Format::Argb8888 => Some("BGRA"),
        wl_shm::Format::Xbgr8888 => Some("RGBx"),
        wl_shm::Format::Abgr8888 => Some("RGBA"),
        _ => None,
    }
}

/// The monitor that has focus, on Hyprland (`hyprctl activeworkspace` names it).
fn focused_monitor() -> Option<String> {
    std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE")?;
    let out = std::process::Command::new("hyprctl")
        .arg("activeworkspace")
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let rest = text.split(" on monitor ").nth(1)?;
    Some(rest.split(':').next()?.trim().to_owned())
}

/// One monitor, captured frame after frame.
pub struct Capture {
    _conn: Connection,
    queue: EventQueue<State>,
    state: State,
    qh: QueueHandle<State>,
    manager: ZwlrScreencopyManagerV1,
    output: wl_output::WlOutput,
    file: File,
    buffer: wl_buffer::WlBuffer,
    stride: u32,
    /// The pixel format, as GStreamer names it.
    pub format: &'static str,
    pub width: u32,
    pub height: u32,
    /// The image comes upside down.
    pub y_invert: bool,
    /// Where the monitor is in the desktop, and its size.
    pub position: (i32, i32),
    pub size: (i32, i32),
}

impl Capture {
    /// Connect and capture the focused monitor (or the first one) once, to learn its format.
    pub fn open() -> Result<Self, String> {
        let conn = Connection::connect_to_env().map_err(|e| e.to_string())?;
        let (globals, mut queue) =
            registry_queue_init::<State>(&conn).map_err(|e| e.to_string())?;
        let qh = queue.handle();
        let manager: ZwlrScreencopyManagerV1 = globals
            .bind(&qh, 3..=3, ())
            .map_err(|_| "the desktop has no direct screen capture")?;
        let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).map_err(|e| e.to_string())?;

        let mut state = State::default();
        let outputs: Vec<wl_output::WlOutput> = globals.contents().with_list(|list| {
            list.iter()
                .filter(|g| g.interface == "wl_output")
                .enumerate()
                .map(|(i, g)| globals.registry().bind(g.name, g.version.min(4), &qh, i))
                .collect()
        });
        state.outputs = outputs.iter().map(|_| Output::default()).collect();
        queue.roundtrip(&mut state).map_err(|e| e.to_string())?;
        let wanted = focused_monitor();
        let index = wanted
            .and_then(|name| state.outputs.iter().position(|o| o.name == name))
            .unwrap_or(0);
        let output = outputs.get(index).cloned().ok_or("no monitor")?;
        let (position, size) = (state.outputs[index].position, state.outputs[index].size);

        // A first capture tells the buffer it needs.
        let frame = manager.capture_output(1, &output, &qh, ());
        while !(state.frame.buffer_done || state.frame.failed) {
            queue
                .blocking_dispatch(&mut state)
                .map_err(|e| e.to_string())?;
        }
        let (format, width, height, stride) = match (state.frame.failed, state.frame.shm) {
            (false, Some(layout)) => layout,
            _ => {
                frame.destroy();
                return Err("the screen can't be captured in a usable format".into());
            }
        };
        let file = shm_file(u64::from(stride) * u64::from(height))?;
        let len = i32::try_from(stride * height).map_err(|e| e.to_string())?;
        let pool = shm.create_pool(file.as_fd(), len, &qh, ());
        let buffer = pool.create_buffer(
            0,
            i32::try_from(width).map_err(|e| e.to_string())?,
            i32::try_from(height).map_err(|e| e.to_string())?,
            i32::try_from(stride).map_err(|e| e.to_string())?,
            format,
            &qh,
            (),
        );
        pool.destroy();

        let mut capture = Self {
            _conn: conn,
            queue,
            state,
            qh,
            manager,
            output,
            file,
            buffer,
            stride,
            format: gst_format(format).ok_or("unusable format")?,
            width,
            height,
            y_invert: false,
            position,
            size: if size == (0, 0) {
                (width as i32, height as i32)
            } else {
                size
            },
        };
        capture.finish(&frame)?;
        capture.y_invert = capture.state.frame.y_invert;
        Ok(capture)
    }

    /// Copy the next frame into `out` (packed rows, `width * height * 4` bytes).
    pub fn grab(&mut self, out: &mut [u8]) -> Result<(), String> {
        self.state.frame = Frame::default();
        let frame = self.manager.capture_output(1, &self.output, &self.qh, ());
        while !(self.state.frame.buffer_done || self.state.frame.failed) {
            self.queue
                .blocking_dispatch(&mut self.state)
                .map_err(|e| e.to_string())?;
        }
        match self.state.frame.shm {
            Some((_, w, h, s)) if (w, h, s) == (self.width, self.height, self.stride) => {}
            _ => {
                frame.destroy();
                return Err("the screen's resolution changed".into());
            }
        }
        self.finish(&frame)?;
        let row = self.width as usize * 4;
        if self.stride as usize == row {
            self.file.read_exact_at(out, 0).map_err(|e| e.to_string())?;
        } else {
            for (y, line) in out.chunks_exact_mut(row).enumerate() {
                self.file
                    .read_exact_at(line, y as u64 * u64::from(self.stride))
                    .map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    }

    /// Copy into the shared buffer and wait until it's there.
    fn finish(&mut self, frame: &ZwlrScreencopyFrameV1) -> Result<(), String> {
        frame.copy(&self.buffer);
        while !(self.state.frame.ready || self.state.frame.failed) {
            self.queue
                .blocking_dispatch(&mut self.state)
                .map_err(|e| e.to_string())?;
        }
        frame.destroy();
        if self.state.frame.failed {
            return Err("the screen couldn't be captured".into());
        }
        Ok(())
    }
}

/// A file in memory (`$XDG_RUNTIME_DIR`), shared with the compositor through its descriptor.
fn shm_file(len: u64) -> Result<File, String> {
    let dir = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").ok_or("no XDG_RUNTIME_DIR")?);
    let path = dir.join(format!("pairly-cast-{}", std::process::id()));
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(&path);
    file.set_len(len).map_err(|e| e.to_string())?;
    Ok(file)
}
