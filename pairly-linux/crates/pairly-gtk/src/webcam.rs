//! `pairly-gtk --webcam <device>`: no window. Decodes a phone's camera (pairlyd's messages on
//! stdin, as for `--screen`) into a v4l2loopback device, so every app sees it as a camera.

use std::io::Read;

use gstreamer::prelude::*;

/// Apps expect a steady rate; the phone films at up to 30.
const FPS: i32 = 30;

struct Output {
    pipeline: gstreamer::Pipeline,
    src: gstreamer_app::AppSrc,
}

impl Output {
    /// The pipeline for a `width`×`height` picture: fixed size and rate, in YUY2, which every
    /// camera app reads.
    fn new(device: &str, width: u32, height: u32) -> Result<Self, String> {
        let launch = format!(
            "appsrc name=src is-live=true format=time do-timestamp=true \
               caps=video/x-h264,stream-format=byte-stream,alignment=au \
             ! h264parse ! decodebin3 ! videoconvert ! videoscale add-borders=true ! videorate \
             ! video/x-raw,format=YUY2,width={width},height={height},framerate={FPS}/1,\
               pixel-aspect-ratio=1/1 \
             ! v4l2sink device=\"{}\" sync=false",
            device.replace('"', "")
        );
        let pipeline = gstreamer::parse::launch(&launch)
            .map_err(|e| format!("no video decoder ({e}); install gst-plugins-good and gst-libav"))?
            .downcast::<gstreamer::Pipeline>()
            .map_err(|_| "not a pipeline".to_owned())?;
        let src = pipeline
            .by_name("src")
            .and_then(|e| e.downcast::<gstreamer_app::AppSrc>().ok())
            .ok_or("no appsrc")?;
        pipeline
            .set_state(gstreamer::State::Playing)
            .map_err(|_| format!("can't write to {device}"))?;
        Ok(Self { pipeline, src })
    }

    fn stop(self) {
        let _ = self.src.end_of_stream();
        let _ = self.pipeline.set_state(gstreamer::State::Null);
    }
}

pub fn run(device: &str) {
    if let Err(e) = gstreamer::init() {
        eprintln!("pairly-gtk --webcam: GStreamer: {e}");
        std::process::exit(1);
    }
    let mut stdin = std::io::stdin().lock();
    let mut head = [0_u8; 5];
    let mut output: Option<Output> = None;
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
                if let Some(old) = output.take() {
                    old.stop();
                }
                match Output::new(device, w.clamp(16, 3840), h.clamp(16, 2160)) {
                    Ok(o) => output = Some(o),
                    Err(e) => {
                        eprintln!("pairly-gtk --webcam: {e}");
                        std::process::exit(1);
                    }
                }
            }
            2 if !body.is_empty() => {
                let Some(out) = &output else { continue };
                let mut buffer = gstreamer::Buffer::from_mut_slice(body.split_off(1));
                if body[0] & 1 == 0
                    && let Some(b) = buffer.get_mut()
                {
                    b.set_flags(gstreamer::BufferFlags::DELTA_UNIT);
                }
                if out.src.push_buffer(buffer).is_err() {
                    break;
                }
            }
            3 => break,
            _ => {}
        }
    }
    if let Some(out) = output {
        out.stop();
    }
}
