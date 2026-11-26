//! A stream from a PC: starts the app on the Sunshine host and feeds the
//! frames and sound it sends into the branches an Xbox stream uses.

use crate::{Slot, pipeline, protocol};
use gamestream::{Codec, Event};
use playwake_core::pc::{StreamRequest, connect, video_format};
use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;
use gstreamer_app::AppSrc;
use std::sync::Arc;
use std::time::Duration;

/// A live source that stamps each buffer with its arrival time. A sink that
/// plays by the clock plays each buffer `delay` later, which is the room it
/// has for packets that arrive late.
fn source(caps: &str, delay: Duration) -> Result<AppSrc, String> {
    let caps: gst::Caps = caps.parse().map_err(|_| "invalid stream format")?;
    let delay = delay.as_nanos() as i64;
    Ok(AppSrc::builder().caps(&caps).is_live(true).format(gst::Format::Time).do_timestamp(true)
        .min_latency(delay).max_latency(delay + MOST_BUFFERED.as_nanos() as i64).build())
}

/// What the sources report as the most they hold back; sinks need some.
const MOST_BUFFERED: Duration = Duration::from_millis(200);

pub fn start(slot: &Slot, pipeline: &gst::Pipeline, pc: StreamRequest, width: u32, height: u32,
    bitrate_kbps: u32, jitter: Duration, quit: glib::MainLoop) -> Result<(), String>
{
    // HDR when it was asked for, the display shows it and the GPU decodes it.
    let hdr = pc.hdr && crate::x11::hdr_output() && gst::ElementFactory::find("vah265dec").is_some();
    // ALLY_PC_CODEC names the codec of an SDR stream instead.
    let (codec, hdr) = video_format(std::env::var("ALLY_PC_CODEC").ok().as_deref(), hdr);
    let format = if codec == Codec::Hevc { "h265" } else { "h264" };
    // Video is shown as it arrives; sound waits out the network's jitter.
    let video = source(&format!("video/x-{format},stream-format=byte-stream,alignment=au"), Duration::ZERO)?;
    let audio = source("audio/x-opus,channel-mapping-family=0,channels=2,rate=48000", jitter)?;
    pipeline.add_many([video.upcast_ref::<gst::Element>(), audio.upcast_ref()]).map_err(|e| format!("sources: {e}"))?;
    {
        let mut guard = slot.lock().unwrap();
        let w = guard.as_mut().ok_or("worker stopped")?;
        let pad = video.static_pad("src").ok_or("video source pad missing")?;
        let branch = pipeline::attach_video(&w.pipeline, &pad, w.state.clone(), &w.renderer, w.window, w.lsfg,
            w.quit.clone(), false, codec, hdr)?;
        w.video = Some(branch);
        if let Some(overlay) = w.overlay.as_mut() { overlay.offscreen = hdr; }
        if let Some(x11) = w.x11.as_mut() && x11.adopt_started.is_none() {
            x11.adopt_started = Some(crate::clock::now_ms());
        }
        let pad = audio.static_pad("src").ok_or("audio source pad missing")?;
        pipeline::attach_audio(&w.pipeline, &pad, w.volume, false)?;
    }
    // Everything is in place before the pipeline plays. A live source added
    // to a pipeline that already plays can stay paused and deliver nothing.
    pipeline.set_state(gst::State::Playing).map_err(|e| format!("pipeline playing: {e}"))?;

    let slot = slot.clone();
    std::thread::spawn(move || {
        let run = || -> Result<(), String> {
            // Full range only where the worker's own presenter converts the picture.
            let (stream, events) = connect(&pc, width, height, bitrate_kbps, codec, hdr, hdr)?;
            let stream = Arc::new(stream);
            {
                let mut guard = slot.lock().unwrap();
                let Some(w) = guard.as_mut() else { stream.stop(); return Ok(()) };
                w.pc = Some(stream.clone());
                crate::arm_input(&slot, w);
            }
            for event in events {
                match event {
                    Event::Video(frame) => {
                        video.push_buffer(gst::Buffer::from_slice(frame.data))
                            .map_err(|error| format!("The player stopped taking video: {error}"))?;
                    }
                    Event::Audio(packet) => { let _ = audio.push_buffer(gst::Buffer::from_slice(packet)); }
                    Event::Rumble(..) => {}
                    Event::Hdr(display) => {
                        eprintln!("pw-hdr host display: {display:?}");
                        if let Some(w) = slot.lock().unwrap().as_ref() { w.state.lock().unwrap().hdr_display = display; }
                    }
                    Event::Ended(result) => return result,
                }
            }
            Ok(())
        };
        if let Err(error) = run() { protocol::error(error); }
        glib::MainContext::default().invoke(move || quit.quit());
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn sources_deliver_their_data_once_the_built_pipeline_plays() {
        gst::init().unwrap();
        let pipeline = gst::Pipeline::new();
        // The order `start` uses: a source, its branch with a sink, then play.
        let source = source("audio/x-opus,channel-mapping-family=0,channels=2,rate=48000", Duration::from_millis(20)).unwrap();
        pipeline.add(&source).unwrap();
        let sink = gst::ElementFactory::make("fakesink").property("sync", true).build().unwrap();
        pipeline.add(&sink).unwrap();
        source.link(&sink).unwrap();
        sink.sync_state_with_parent().unwrap();
        let (arrived, arrivals) = mpsc::channel();
        sink.static_pad("sink").unwrap().add_probe(gst::PadProbeType::BUFFER, move |_, _| {
            let _ = arrived.send(());
            gst::PadProbeReturn::Ok
        });
        pipeline.set_state(gst::State::Playing).unwrap();

        // Data that comes seconds later, as it does after the PC starts the app.
        std::thread::sleep(Duration::from_millis(300));
        for _ in 0..3 { source.push_buffer(gst::Buffer::from_slice([0u8; 8])).unwrap(); }
        let delivered = (0..3).all(|_| arrivals.recv_timeout(Duration::from_secs(3)).is_ok());
        pipeline.set_state(gst::State::Null).unwrap();
        assert!(delivered);
    }
}
