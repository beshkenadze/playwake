//! The LSFG preview: a made-up scene shaped like a game, which goes through
//! the same encode, decode and video window as a stream. It has what frame
//! generation gets wrong: a scrolling background, objects that move at
//! different speeds, scrolling text and a running clock.
use crate::{Slot, pipeline};
use gamestream::Codec;
use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;

/// H.264 encoders in the order to try; the GPU's first.
const ENCODERS: [(&str, &str); 4] = [
    ("vah264enc", "vah264enc bitrate=20000"),
    ("vah264lpenc", "vah264lpenc bitrate=20000"),
    ("x264enc", "x264enc tune=zerolatency speed-preset=ultrafast bitrate=20000"),
    ("openh264enc", "openh264enc bitrate=20000000"),
];

/// How far the scrolling text moves each second, as a share of the width.
const TICKER_SPEED: f64 = 0.12;

pub fn start(slot: &Slot, pipeline: &gst::Pipeline, width: u32, height: u32, fps: u32, russian: bool,
    quit: glib::MainLoop) -> Result<(), String>
{
    let encoder = ENCODERS.iter().find(|(name, _)| gst::ElementFactory::find(name).is_some())
        .map(|(_, description)| *description).ok_or("No H.264 encoder for the LSFG preview")?;
    let ticker = if russian {
        "Текст: здесь LSFG ошибается первым · 0123456789"
    } else {
        "Text: LSFG errs here first · 0123456789"
    };
    // Everything stays in NV12, which the encoder takes, and each overlay
    // runs on its own thread: in one thread the CPU cannot keep the rate.
    // The balls are text too: a glyph moves and blends cheaply.
    let queue = "queue max-size-buffers=2";
    let ball = |name: &str, size: u32, color: &str| format!(
        "textoverlay auto-resize=false name={name} text=\"●\" font-desc=\"Sans {size}\" color={color} \
         halignment=position valignment=position");
    let description = format!(
        "videotestsrc is-live=true pattern=smpte horizontal-speed=4 \
           ! video/x-raw,format=NV12,width={width},height={height},framerate={fps}/1 \
         ! {queue} ! {} ! {queue} ! {} \
         ! {queue} ! textoverlay auto-resize=false wrap-mode=none name=ticker text=\"{ticker}\" font-desc=\"Sans Bold 34\" \
           halignment=position xpos=1 valignment=position ypos=0.8 shaded-background=true \
         ! {queue} ! timeoverlay auto-resize=false font-desc=\"Monospace Bold 26\" valignment=top halignment=right shaded-background=true \
         ! {queue} ! {encoder} name=encoder",
        ball("ball1", 140, "0xff7cdb8c"), ball("ball2", 110, "0xffff8a3d"));
    // Only the encoder's output leaves the bin. A text overlay whose text
    // input looks linked would wait for text forever.
    let scene = gst::parse::bin_from_description(&description, false).map_err(|e| format!("LSFG preview scene: {e}"))?;
    let output = scene.by_name("encoder").and_then(|encoder| encoder.static_pad("src")).ok_or("encoder pad missing")?;
    scene.add_pad(&gst::GhostPad::with_target(&output).map_err(|e| format!("LSFG preview scene: {e}"))?)
        .map_err(|e| format!("LSFG preview scene: {e}"))?;
    let element = |name: &str| scene.by_name(name).ok_or(format!("LSFG preview scene: {name} missing"));
    let (ticker, first, second) = (element("ticker")?, element("ball1")?, element("ball2")?);
    // Each frame moves the text and the balls by the frame's time.
    let moving = ticker.clone();
    ticker.static_pad("video_sink").ok_or("scrolling text pad missing")?
        .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let Some(time) = info.buffer().and_then(|buffer| buffer.pts()) {
                let seconds = time.nseconds() as f64 / 1e9;
                moving.set_property("xpos", ticker_position(seconds));
                let ((x1, y1), (x2, y2)) = ball_positions(seconds);
                first.set_property("xpos", x1);
                first.set_property("ypos", y1);
                second.set_property("xpos", x2);
                second.set_property("ypos", y2);
            }
            gst::PadProbeReturn::Ok
        });
    pipeline.add(&scene).map_err(|e| format!("LSFG preview scene: {e}"))?;
    let pad = scene.static_pad("src").ok_or("LSFG preview scene pad missing")?;
    {
        let mut guard = slot.lock().unwrap();
        let w = guard.as_mut().ok_or("worker stopped")?;
        let branch = pipeline::attach_video(&w.pipeline, &pad, w.state.clone(), &w.renderer, w.window, w.lsfg,
            quit, false, Codec::H264, false)?;
        w.video = Some(branch);
    }
    pipeline.set_state(gst::State::Playing).map_err(|e| format!("pipeline playing: {e}"))?;
    Ok(())
}

/// Where the moving text is, as a share of the width. GStreamer keeps text
/// inside the frame, so the line, shorter than the frame, goes from one side
/// to the other and back instead of scrolling off.
fn ticker_position(seconds: f64) -> f64 {
    (2.0 * (seconds * TICKER_SPEED).fract() - 1.0).abs()
}

/// Where the two balls are, as shares of the frame: one sweeps from side to
/// side, the other circles faster.
fn ball_positions(seconds: f64) -> ((f64, f64), (f64, f64)) {
    let sweep = 1.0 - (2.0 * (seconds * 0.25).fract() - 1.0).abs();
    let angle = seconds * 1.3;
    ((0.05 + 0.8 * sweep, 0.2), (0.45 + 0.3 * angle.cos(), 0.45 + 0.25 * angle.sin()))
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_text_goes_from_one_side_to_the_other_and_back() {
        let period = 1.0 / super::TICKER_SPEED;
        assert_eq!(super::ticker_position(0.0), 1.0);
        assert!(super::ticker_position(period / 2.0).abs() < 1e-9);
        assert!(super::ticker_position(period / 4.0) < super::ticker_position(0.0));
        assert!(super::ticker_position(period * 3.0 / 4.0) > super::ticker_position(period / 2.0));
    }

    #[test]
    fn the_balls_stay_inside_the_frame() {
        for step in 0..400 {
            let ((x1, y1), (x2, y2)) = super::ball_positions(step as f64 * 0.05);
            for value in [x1, y1, x2, y2] { assert!((0.0..=1.0).contains(&value)); }
        }
    }
}
