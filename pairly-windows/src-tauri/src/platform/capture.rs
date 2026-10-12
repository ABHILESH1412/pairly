//! Recording this PC's main screen for a phone to watch: Windows' desktop duplication hands
//! over each new picture of the screen (from the graphics card), which is shrunk to fit 1080p
//! (see [`output_size`]), has the pointer drawn in, and is encoded to H.264
//! (OpenH264, tuned for screen content) at up to 30 frames a second. Nothing is sent while the
//! screen doesn't change, apart from one picture a second so the phone knows it's still there.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// The video fits 1920×1080 (or 1080×1920 for an upright screen): see [`output_size`].
const MAX_SIDE: u32 = 1920;
const MAX_OTHER_SIDE: u32 = 1080;

pub enum Event {
    /// The video's size; returning false stops the recording.
    Started { width: u32, height: u32 },
    /// One H.264 access unit (Annex B); returning true asks for a key frame next.
    Frame { data: Vec<u8>, key: bool },
    /// The recording ended (the screen went away, or it failed).
    Stopped(String),
}

pub type OnEvent = Box<dyn Fn(Event) -> bool + Send>;

pub struct Recording {
    stop: Arc<AtomicBool>,
    monitor: (i32, i32, u32, u32),
}

impl Recording {
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    /// The recorded monitor on the desktop: (x, y, width, height) in pixels.
    pub fn monitor(&self) -> (i32, i32, u32, u32) {
        self.monitor
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Pick the video's size: the screen, shrunk (keeping its shape) to fit 1080p, which every
/// phone's hardware decoder takes. Bigger pictures (a 1920×1200 laptop screen is already
/// past it) need a decoder level many phones lack, and they then show nothing at all. Both
/// sides a multiple of 8, as H.264 decoders prefer.
pub fn output_size(width: u32, height: u32) -> (u32, u32) {
    let (width, height) = (width.max(8), height.max(8));
    let (max_w, max_h) = if width >= height {
        (MAX_SIDE, MAX_OTHER_SIDE)
    } else {
        (MAX_OTHER_SIDE, MAX_SIDE)
    };
    // The smaller of the two shrink ratios, never enlarging.
    let (w, h) = if u64::from(width) * u64::from(max_h) > u64::from(height) * u64::from(max_w) {
        (max_w.min(width), (u64::from(height) * u64::from(max_w.min(width)) / u64::from(width)) as u32)
    } else {
        ((u64::from(width) * u64::from(max_h.min(height)) / u64::from(height)) as u32, max_h.min(height))
    };
    ((w & !7).max(8), (h & !7).max(8))
}

/// Scale a BGRA picture (`sw` × `sh`, rows `pitch` bytes apart) to `ow` × `oh` into `out`
/// (tightly packed), averaging the pixels each output pixel covers, so text stays readable.
pub fn scale_bgra(src: &[u8], pitch: usize, sw: u32, sh: u32, out: &mut [u8], ow: u32, oh: u32) {
    let (sw, sh, ow, oh) = (sw as usize, sh as usize, ow as usize, oh as usize);
    if (sw, sh) == (ow, oh) {
        for y in 0..oh {
            out[y * ow * 4..(y + 1) * ow * 4].copy_from_slice(&src[y * pitch..y * pitch + ow * 4]);
        }
        return;
    }
    // The source columns (and rows) each output pixel covers: [start, end).
    let span = |i: usize, s: usize, o: usize| {
        let a = i * s / o;
        (a, ((i + 1) * s / o).max(a + 1).min(s))
    };
    let cols: Vec<(usize, usize)> = (0..ow).map(|x| span(x, sw, ow)).collect();
    for y in 0..oh {
        let (y0, y1) = span(y, sh, oh);
        for (x, &(x0, x1)) in cols.iter().enumerate() {
            let mut sum = [0u32; 4];
            for sy in y0..y1 {
                let row = sy * pitch;
                for sx in x0..x1 {
                    let p = row + sx * 4;
                    for c in 0..4 {
                        sum[c] += u32::from(src[p + c]);
                    }
                }
            }
            let n = ((y1 - y0) * (x1 - x0)) as u32;
            let o = (y * ow + x) * 4;
            for c in 0..4 {
                out[o + c] = (sum[c] / n) as u8;
            }
        }
    }
}

/// Draw the pointer (a white dot with a dark ring) at (x, y) in a BGRA picture.
pub fn draw_pointer(buf: &mut [u8], w: u32, h: u32, x: i32, y: i32, radius: i32) {
    for dy in -radius - 2..=radius + 2 {
        for dx in -radius - 2..=radius + 2 {
            let (px, py) = (x + dx, y + dy);
            if px < 0 || py < 0 || px >= w as i32 || py >= h as i32 {
                continue;
            }
            let d2 = dx * dx + dy * dy;
            let color = if d2 <= radius * radius {
                [255, 255, 255]
            } else if d2 <= (radius + 2) * (radius + 2) {
                [20, 20, 20]
            } else {
                continue;
            };
            let o = (py as usize * w as usize + px as usize) * 4;
            buf[o..o + 3].copy_from_slice(&color);
        }
    }
}

struct Encoder {
    inner: openh264::encoder::Encoder,
    yuv: openh264::formats::YUVBuffer,
    width: u32,
    height: u32,
}

impl Encoder {
    fn new(width: u32, height: u32) -> anyhow::Result<Self> {
        use openh264::encoder::{
            BitRate, EncoderConfig, FrameRate, IntraFramePeriod, RateControlMode, UsageType,
        };
        let config = EncoderConfig::new()
            .usage_type(UsageType::ScreenContentRealTime)
            .bitrate(BitRate::from_bps(6_000_000))
            .rate_control_mode(RateControlMode::Bitrate)
            .max_frame_rate(FrameRate::from_hz(30.0))
            // A key frame every few seconds, so a lost picture doesn't linger.
            .intra_frame_period(IntraFramePeriod::from_num_frames(150))
            .skip_frames(false);
        let inner = openh264::encoder::Encoder::with_api_config(
            openh264::OpenH264API::from_source(),
            config,
        )?;
        Ok(Self {
            inner,
            yuv: openh264::formats::YUVBuffer::new(width as usize, height as usize),
            width,
            height,
        })
    }

    /// Encode one BGRA picture; (access unit, key frame), or None if the encoder skipped it.
    fn encode(&mut self, bgra: &[u8], key: bool) -> anyhow::Result<Option<(Vec<u8>, bool)>> {
        use openh264::encoder::FrameType;
        use openh264::formats::BgraSliceU8;
        self.yuv.read_bgra8(BgraSliceU8::new(
            bgra,
            (self.width as usize, self.height as usize),
        ));
        if key {
            self.inner.force_intra_frame();
        }
        let stream = self.inner.encode(&self.yuv)?;
        let is_key = matches!(stream.frame_type(), FrameType::IDR | FrameType::I);
        let data = stream.to_vec();
        Ok((!data.is_empty()).then_some((data, is_key)))
    }
}

/// Start recording the main screen; events go to `on_event` from the recording's own thread.
pub fn start(on_event: OnEvent) -> anyhow::Result<Recording> {
    let stop = Arc::new(AtomicBool::new(false));
    let monitor = imp::main_monitor()?;
    let flag = stop.clone();
    std::thread::Builder::new()
        .name("pairly-cast".into())
        .spawn(move || {
            let reason = match imp::record(&flag, &on_event) {
                Ok(()) => "the screen sharing ended".to_owned(),
                Err(e) => {
                    tracing::warn!(error = %e, "screen recording failed");
                    format!("the screen recording failed: {e}")
                }
            };
            if !flag.load(Ordering::SeqCst) {
                on_event(Event::Stopped(reason));
            }
        })?;
    Ok(Recording { stop, monitor })
}

#[cfg(windows)]
mod imp {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use windows::Win32::Foundation::{HMODULE, POINT};
    use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
        D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
        D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
    };
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_NOT_FOUND, DXGI_ERROR_WAIT_TIMEOUT,
        DXGI_OUTDUPL_FRAME_INFO, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput, IDXGIOutput1,
        IDXGIOutputDuplication, IDXGIResource,
    };
    use windows::Win32::Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleDC,
        CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, HBITMAP, HDC, HGDIOBJ,
        ReleaseDC, SRCCOPY, SelectObject,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetCursorPos, GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN,
    };
    use windows::core::Interface;

    use super::{Encoder, Event, OnEvent, draw_pointer, output_size, scale_bgra};

    /// The longest gap between pictures sent while the screen is still.
    const KEEP_ALIVE: Duration = Duration::from_secs(1);
    /// A key frame at least this often, so a viewer that joins (or loses a picture) is never
    /// left waiting: the phone shows nothing until one arrives, and a still screen sends few
    /// frames. More often just after starting, while the phone is still setting up its decoder
    /// (frames before that are dropped).
    const KEY_EVERY: Duration = Duration::from_secs(2);
    const KEY_EVERY_AT_START: Duration = Duration::from_millis(500);
    const START: Duration = Duration::from_secs(4);
    const FRAME_GAP: Duration = Duration::from_millis(33);
    /// Screen copies without duplication: about 15 a second.
    const GDI_GAP: Duration = Duration::from_millis(66);

    /// The adapter and output showing the main screen (the one at the desktop's origin).
    fn find_output() -> anyhow::Result<(IDXGIAdapter1, IDXGIOutput)> {
        // SAFETY: plain COM factory creation.
        let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
        let mut first = None;
        for a in 0.. {
            // SAFETY: enumerating until DXGI_ERROR_NOT_FOUND.
            let adapter = match unsafe { factory.EnumAdapters1(a) } {
                Ok(a) => a,
                Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(e) => return Err(e.into()),
            };
            for o in 0.. {
                // SAFETY: as above.
                let output = match unsafe { adapter.EnumOutputs(o) } {
                    Ok(o) => o,
                    Err(_) => break,
                };
                // SAFETY: a plain getter.
                let desc = unsafe { output.GetDesc()? };
                if !desc.AttachedToDesktop.as_bool() {
                    continue;
                }
                let r = desc.DesktopCoordinates;
                if r.left == 0 && r.top == 0 {
                    return Ok((adapter, output));
                }
                if first.is_none() {
                    first = Some((adapter.clone(), output));
                }
            }
        }
        first.ok_or_else(|| anyhow::anyhow!("no screen to record"))
    }

    pub fn main_monitor() -> anyhow::Result<(i32, i32, u32, u32)> {
        if let Ok((_, output)) = find_output() {
            // SAFETY: a plain getter.
            let r = unsafe { output.GetDesc()? }.DesktopCoordinates;
            return Ok((
                r.left,
                r.top,
                (r.right - r.left).max(1) as u32,
                (r.bottom - r.top).max(1) as u32,
            ));
        }
        // SAFETY: plain getters; the main screen is at the desktop's origin.
        let (w, h) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
        anyhow::ensure!(w > 0 && h > 0, "no screen to record");
        Ok((0, 0, w as u32, h as u32))
    }

    /// Plain screen copies (GDI), where desktop duplication isn't available: laptops whose
    /// screen hangs off one graphics chip while apps run on the other, some remote sessions.
    /// Slower, so a few frames fewer a second; a change is spotted by comparing samples.
    struct Gdi {
        screen: HDC,
        memory: HDC,
        bitmap: HBITMAP,
        old: HGDIOBJ,
        bits: *const u8,
        width: u32,
        height: u32,
        origin: (i32, i32),
        last_sample: u64,
        last_grab: Instant,
    }

    impl Gdi {
        fn open() -> anyhow::Result<Self> {
            let (x, y, width, height) = main_monitor()?;
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width as i32,
                    // Negative: rows top to bottom.
                    biHeight: -(height as i32),
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            // SAFETY: the DCs and bitmap are released in Drop; `bits` lives as long as `bitmap`.
            unsafe {
                let screen = GetDC(None);
                anyhow::ensure!(!screen.is_invalid(), "can't read the screen");
                let memory = CreateCompatibleDC(Some(screen));
                let mut bits = std::ptr::null_mut();
                let bitmap =
                    match CreateDIBSection(Some(screen), &info, DIB_RGB_COLORS, &mut bits, None, 0)
                    {
                        Ok(b) => b,
                        Err(e) => {
                            let _ = DeleteDC(memory);
                            ReleaseDC(None, screen);
                            return Err(e.into());
                        }
                    };
                let old = SelectObject(memory, bitmap.into());
                Ok(Self {
                    screen,
                    memory,
                    bitmap,
                    old,
                    bits: bits.cast(),
                    width,
                    height,
                    origin: (x, y),
                    last_sample: 0,
                    last_grab: Instant::now() - GDI_GAP,
                })
            }
        }

        /// Copy the screen (at most every [`GDI_GAP`]); true if it changed.
        fn next(&mut self) -> windows::core::Result<bool> {
            let wait = GDI_GAP.saturating_sub(self.last_grab.elapsed());
            std::thread::sleep(wait);
            self.last_grab = Instant::now();
            // SAFETY: valid DCs of the right size.
            unsafe {
                BitBlt(
                    self.memory,
                    0,
                    0,
                    self.width as i32,
                    self.height as i32,
                    Some(self.screen),
                    self.origin.0,
                    self.origin.1,
                    SRCCOPY | CAPTUREBLT,
                )?;
            }
            let sample = self.with_pixels(|px, _| {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                // Every 61st pixel: cheap, and catches any change bigger than a few pixels.
                px.chunks(4 * 61)
                    .for_each(|c| c[..4.min(c.len())].hash(&mut h));
                h.finish()
            });
            let fresh = sample != self.last_sample;
            self.last_sample = sample;
            Ok(fresh)
        }

        fn with_pixels<R>(&self, f: impl FnOnce(&[u8], usize) -> R) -> R {
            let pitch = self.width as usize * 4;
            // SAFETY: the DIB section holds `width * height` 32-bit pixels while `bitmap` lives.
            let px = unsafe { std::slice::from_raw_parts(self.bits, pitch * self.height as usize) };
            f(px, pitch)
        }
    }

    impl Drop for Gdi {
        fn drop(&mut self) {
            // SAFETY: created in `open`, released once.
            unsafe {
                SelectObject(self.memory, self.old);
                let _ = DeleteObject(self.bitmap.into());
                let _ = DeleteDC(self.memory);
                ReleaseDC(None, self.screen);
            }
        }
    }

    /// Where pictures of the screen come from.
    enum Source {
        Duplication(Capture),
        Gdi(Gdi),
    }

    impl Source {
        fn open() -> anyhow::Result<Self> {
            match open() {
                Ok(c) => Ok(Self::Duplication(c)),
                Err(e) => {
                    tracing::info!(error = %e, "no desktop duplication here: copying the screen instead");
                    Ok(Self::Gdi(Gdi::open()?))
                }
            }
        }

        fn size(&self) -> (u32, u32) {
            match self {
                Self::Duplication(c) => (c.width, c.height),
                Self::Gdi(g) => (g.width, g.height),
            }
        }

        fn origin(&self) -> (i32, i32) {
            match self {
                Self::Duplication(c) => c.origin,
                Self::Gdi(g) => g.origin,
            }
        }

        fn next(&mut self) -> windows::core::Result<bool> {
            match self {
                Self::Duplication(c) => c.next(50),
                Self::Gdi(g) => g.next(),
            }
        }

        fn read<R>(&self, f: impl FnOnce(&[u8], usize) -> R) -> windows::core::Result<R> {
            match self {
                Self::Duplication(c) => c.read(f),
                Self::Gdi(g) => Ok(g.with_pixels(f)),
            }
        }
    }

    struct Capture {
        /// Kept alive with the duplication made on it.
        _device: ID3D11Device,
        context: ID3D11DeviceContext,
        dup: IDXGIOutputDuplication,
        staging: ID3D11Texture2D,
        width: u32,
        height: u32,
        origin: (i32, i32),
    }

    fn open() -> anyhow::Result<Capture> {
        let (adapter, output) = find_output()?;
        let mut device = None;
        let mut context = None;
        // SAFETY: out-parameters for a new device on the screen's adapter.
        unsafe {
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
        }
        let device: ID3D11Device = device.ok_or_else(|| anyhow::anyhow!("no Direct3D device"))?;
        let context = context.ok_or_else(|| anyhow::anyhow!("no Direct3D context"))?;
        let output1: IDXGIOutput1 = output.cast()?;
        // SAFETY: the device belongs to the output's adapter.
        let dup = unsafe { output1.DuplicateOutput(&device)? };
        // SAFETY: plain getters.
        let mode = unsafe { dup.GetDesc() }.ModeDesc;
        let origin = unsafe { output.GetDesc()? }.DesktopCoordinates;
        let desc = D3D11_TEXTURE2D_DESC {
            Width: mode.Width,
            Height: mode.Height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut staging = None;
        // SAFETY: a valid description; the out-parameter receives the texture.
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut staging))? };
        Ok(Capture {
            _device: device,
            context,
            dup,
            staging: staging.ok_or_else(|| anyhow::anyhow!("no staging texture"))?,
            width: mode.Width,
            height: mode.Height,
            origin: (origin.left, origin.top),
        })
    }

    impl Capture {
        /// Wait for the next picture of the screen; false on timeout (no change).
        fn next(&self, timeout_ms: u32) -> windows::core::Result<bool> {
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;
            // SAFETY: out-parameters; every acquired frame is released below.
            match unsafe {
                self.dup
                    .AcquireNextFrame(timeout_ms, &mut info, &mut resource)
            } {
                Ok(()) => {}
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(false),
                Err(e) => return Err(e),
            }
            let copied = resource
                .ok_or_else(|| windows::core::Error::from(DXGI_ERROR_NOT_FOUND))
                .and_then(|r| r.cast::<ID3D11Texture2D>())
                .map(|texture| {
                    // SAFETY: same size and format as the screen.
                    unsafe { self.context.CopyResource(&self.staging, &texture) }
                });
            // SAFETY: the frame acquired above.
            unsafe { self.dup.ReleaseFrame()? };
            copied?;
            // Only the pointer moved: nothing new to read (it's drawn in anyway).
            Ok(info.LastPresentTime != 0 || info.AccumulatedFrames > 0)
        }

        /// Read the staged picture (BGRA rows, `pitch` bytes apart) with `f`.
        fn read<R>(&self, f: impl FnOnce(&[u8], usize) -> R) -> windows::core::Result<R> {
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            // SAFETY: the staging texture is CPU-readable; unmapped below.
            unsafe {
                self.context
                    .Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
            }
            let pitch = mapped.RowPitch as usize;
            let len = pitch * self.height as usize;
            // SAFETY: Map gives `RowPitch * height` readable bytes until Unmap.
            let data = unsafe { std::slice::from_raw_parts(mapped.pData.cast::<u8>(), len) };
            let r = f(data, pitch);
            // SAFETY: mapped above.
            unsafe { self.context.Unmap(&self.staging, 0) };
            Ok(r)
        }
    }

    pub fn record(stop: &AtomicBool, on_event: &OnEvent) -> anyhow::Result<()> {
        let mut cap = Source::open()?;
        let (width, height) = cap.size();
        let (ow, oh) = output_size(width, height);
        let mut encoder = Encoder::new(ow, oh)?;
        if !on_event(Event::Started {
            width: ow,
            height: oh,
        }) {
            return Ok(());
        }
        let mut picture = vec![0u8; (ow * oh * 4) as usize];
        let mut frame = picture.clone();
        let mut have_picture = false;
        let mut want_key = true;
        let mut last_sent = Instant::now() - KEEP_ALIVE;
        let started = Instant::now();
        let mut last_key = Instant::now();
        let mut lost = 0;
        let mut last_cursor = (i32::MIN, i32::MIN);
        while !stop.load(Ordering::SeqCst) {
            let fresh = match cap.next() {
                Ok(fresh) => fresh,
                // Resolution change, full-screen game, the secure desktop (UAC): start again.
                Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST && lost < 20 => {
                    lost += 1;
                    std::thread::sleep(Duration::from_millis(250));
                    match Source::open() {
                        Ok(c) if c.size() == cap.size() => {
                            cap = c;
                            want_key = true;
                        }
                        Ok(_) => anyhow::bail!("the screen's size changed"),
                        Err(_) => {}
                    }
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            lost = 0;
            if fresh {
                cap.read(|src, pitch| scale_bgra(src, pitch, width, height, &mut picture, ow, oh))?;
                have_picture = true;
            }
            if !have_picture {
                continue;
            }
            let mut cursor = POINT::default();
            // SAFETY: an out-parameter.
            let pointer = unsafe { GetCursorPos(&mut cursor) }.is_ok();
            let moved = pointer && (cursor.x, cursor.y) != last_cursor;
            let key_due = last_key.elapsed()
                >= if started.elapsed() < START {
                    KEY_EVERY_AT_START
                } else {
                    KEY_EVERY
                };
            let due = fresh || moved || key_due || last_sent.elapsed() >= KEEP_ALIVE;
            if !due || last_sent.elapsed() < FRAME_GAP {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            }
            frame.copy_from_slice(&picture);
            last_cursor = (cursor.x, cursor.y);
            if pointer {
                let x = (cursor.x - cap.origin().0) * ow as i32 / width as i32;
                let y = (cursor.y - cap.origin().1) * oh as i32 / height as i32;
                draw_pointer(&mut frame, ow, oh, x, y, 5);
            }
            if key_due {
                want_key = true;
            }
            if let Some((data, key)) = encoder.encode(&frame, want_key)? {
                want_key = false;
                last_sent = Instant::now();
                if key {
                    last_key = last_sent;
                }
                if on_event(Event::Frame { data, key }) {
                    want_key = true;
                }
            }
        }
        Ok(())
    }
}

#[cfg(not(windows))]
mod imp {
    use std::sync::atomic::AtomicBool;

    use super::OnEvent;

    pub fn main_monitor() -> anyhow::Result<(i32, i32, u32, u32)> {
        anyhow::bail!("screen recording is built for Windows")
    }

    pub fn record(_stop: &AtomicBool, _on_event: &OnEvent) -> anyhow::Result<()> {
        anyhow::bail!("screen recording is built for Windows")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_fit_1080p_and_keep_their_shape() {
        assert_eq!(output_size(1920, 1080), (1920, 1080));
        assert_eq!(output_size(3840, 2160), (1920, 1080));
        // This laptop's 16:10 screen: 1200 rows is past what many phones decode.
        assert_eq!(output_size(1920, 1200), (1728, 1080));
        assert_eq!(output_size(2560, 1600), (1728, 1080));
        // Upright screens fit 1080×1920; small ones aren't enlarged.
        assert_eq!(output_size(1200, 1920), (1080, 1728));
        assert_eq!(output_size(1366, 768), (1360, 768));
        for (w, h) in [(1366, 767), (5120, 1440), (800, 600), (3000, 2000)] {
            let (ow, oh) = output_size(w, h);
            assert!(ow % 8 == 0 && oh % 8 == 0 && ow <= 1920 && oh <= 1920);
            assert!(ow.max(oh) <= 1920 && ow.min(oh) <= 1080, "{w}x{h} -> {ow}x{oh}");
        }
    }

    #[test]
    fn scaling_averages_what_each_pixel_covers() {
        // 2x2 picture, pitch 16 (padded rows), shrunk to 1x1.
        let mut src = vec![0u8; 32];
        src[0..4].copy_from_slice(&[0, 0, 0, 255]);
        src[4..8].copy_from_slice(&[100, 100, 100, 255]);
        src[16..20].copy_from_slice(&[200, 200, 200, 255]);
        src[20..24].copy_from_slice(&[100, 100, 100, 255]);
        let mut out = [0u8; 4];
        scale_bgra(&src, 16, 2, 2, &mut out, 1, 1);
        assert_eq!(out, [100, 100, 100, 255]);
        // Same size: copied as is (padding dropped).
        let mut same = [0u8; 16];
        scale_bgra(&src, 16, 2, 2, &mut same, 2, 2);
        assert_eq!(&same[8..12], &[200, 200, 200, 255]);
        // An odd ratio (3 → 2) covers every source pixel without reading past the rows.
        let src3 = vec![50u8; 3 * 3 * 4];
        let mut out2 = [0u8; 2 * 2 * 4];
        scale_bgra(&src3, 12, 3, 3, &mut out2, 2, 2);
        assert!(out2.iter().all(|&v| v == 50));
    }

    #[test]
    fn encodes_a_picture() {
        let (w, h) = (64u32, 48u32);
        let mut enc = Encoder::new(w, h).expect("encoder");
        let picture = vec![128u8; (w * h * 4) as usize];
        let (data, key) = enc
            .encode(&picture, true)
            .expect("encode")
            .expect("a frame");
        assert!(key);
        // Annex B: starts with a start code.
        assert!(data.starts_with(&[0, 0, 0, 1]) || data.starts_with(&[0, 0, 1]));
    }
}
