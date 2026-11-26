//! Video/audio branch construction and pad probes, byte-parity with the
//! Python worker's element chains (media/gstreamer_player.py link_track).

use crate::{clock, protocol, state::{State, Timing}};
use gamestream::Codec;
use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;
use gstreamer_video::prelude::*;
use serde_json::json;
use std::sync::{Arc, Mutex};

pub struct VideoBranch {
    pub bin: gst::Bin,
    pub sink: gst::Element,
    pub queue: Option<gst::Element>,
    pub decoder: String,
    pub renderer: String,
    pub conversion: Option<String>,
}

/// The parser of a codec and its decoders, best first.
fn elements(codec: Codec) -> Result<(&'static str, [&'static str; 2]), String> {
    match codec {
        Codec::H264 => Ok(("h264parse", ["vah264dec", "avdec_h264"])),
        Codec::Hevc => Ok(("h265parse", ["vah265dec", "avdec_h265"])),
        Codec::Av1 => Err("AV1 streams are not supported".into()),
    }
}

/// The decoders to try for a codec, best first. `preferred` (ALLY_DECODER)
/// goes first when it decodes this codec; the launcher names an H.264 one.
fn decoders(codec: Codec, preferred: Option<&str>) -> Result<Vec<String>, String> {
    let mut choices: Vec<String> = elements(codec)?.1.map(String::from).to_vec();
    let family = if codec == Codec::Hevc { "h265" } else { "h264" };
    if let Some(preferred) = preferred.filter(|name| name.contains(family)) {
        choices.retain(|choice| choice != preferred);
        choices.insert(0, preferred.to_owned());
    }
    Ok(choices)
}

/// The audio branch's depayloader and sink options. Only PulseAudio's sink
/// has the buffer sizes; the fallback sink would refuse them.
fn audio_output(rtp: bool, sink: &str) -> (&'static str, &'static str) {
    match (rtp, sink) {
        (true, _) => ("rtpopusdepay ! ", "sync=true"),
        (false, "pulsesink") => ("", "sync=false buffer-time=100000 latency-time=10000"),
        (false, _) => ("", "sync=false"),
    }
}

pub fn choose_decoder(codec: Codec) -> Result<(String, String), String> {
    let choices = decoders(codec, std::env::var("ALLY_DECODER").ok().as_deref())?;
    let name = choices.iter()
        .find(|c| gst::ElementFactory::find(c).is_some())
        .ok_or(format!("GStreamer {codec:?} decoder unavailable"))?
        .clone();
    let mut opts = std::env::var("ALLY_DECODER_OPTS").unwrap_or_default();
    if !opts.is_empty() {
        let factory = gst::ElementFactory::find(&name);
        opts = opts.split_whitespace()
            .filter(|part| factory.as_ref().is_some_and(|f| f.find_property(part.split('=').next().unwrap_or("")).is_some()))
            .collect::<Vec<_>>()
            .join(" ");
    }
    Ok((name, opts))
}

pub fn attach_video(
    pipeline: &gst::Pipeline,
    pad: &gst::Pad,
    state: Arc<Mutex<State>>,
    renderer: &str,
    window_id: Option<u64>,
    lsfg: bool,
    quit: glib::MainLoop,
    rtp: bool,
    codec: Codec,
    // A 10-bit stream, which only the worker's own presenter can show.
    deep: bool,
) -> Result<VideoBranch, String> {
    let (decoder, decoder_opts) = choose_decoder(codec)?;
    let parser = elements(codec)?.0;
    // A VA decoder leaves the frame on the GPU; these sinks take it from memory.
    let va = decoder.starts_with("va");
    let vulkan = renderer == "vulkan";
    let mut conversion: Option<String> = None;
    let video_output = if deep {
        conversion = Some("vk_p010".into());
        "video/x-raw,format=P010_10LE ! appsink max-buffers=1 drop=true".to_string()
    } else if vulkan {
        let queue = "queue name=video_queue max-size-buffers=2 max-size-bytes=0 max-size-time=0 leaky=downstream ! ";
        let download = if va { "vapostproc ! video/x-raw,format=NV12 ! " } else { "" };
        let chain = if decoder == "vulkanh264dec" {
            conversion = Some("vk_zero_copy".into());
            "vulkancolorconvert ! video/x-raw(memory:VulkanImage),format=BGRA ! vulkansink"
        } else if va
            && std::env::var("ALLY_VULKAN_CONVERSION").as_deref() != Ok("cpu")
            && gst::ElementFactory::find("vulkancolorconvert").is_some()
        {
            conversion = Some("gpu_nv12".into());
            "vulkanupload name=upload ! vulkancolorconvert ! video/x-raw(memory:VulkanImage),format=BGRA ! vulkansink"
        } else {
            conversion = Some("cpu_bgra".into());
            "videoconvert ! video/x-raw,format=BGRA ! vulkanupload name=upload ! vulkansink"
        };
        format!("{queue}{download}{chain}")
    } else if window_id.is_some() {
        let download = if va {
            "vapostproc ! video/x-raw,format=NV12 ! "
        } else if decoder == "vulkanh264dec" {
            "vulkandownload ! "
        } else {
            ""
        };
        // A PC stream's window shrinks above the on-screen keyboard; the
        // scaler follows the window and does nothing at full size.
        let scale = if rtp { "" } else { "videoscale ! " };
        format!("{download}videoconvert ! {scale}video/x-raw,format=BGRx ! ximagesink")
    } else {
        "videoconvert ! autovideosink".to_string()
    };
    let options = if !deep && (vulkan || window_id.is_some()) { "sync=false force-aspect-ratio=true" } else { "sync=false" };
    // An Xbox sends RTP; a PC stream arrives as whole frames.
    let depay = if rtp { "rtph264depay name=depay ! " } else { "" };
    let description = format!(
        "{depay}{parser} name=parser ! {decoder} {decoder_opts} name=decoder ! \
         {video_output} name=video_sink {options}");
    let bin = gst::parse::bin_from_description(&description, true)
        .map_err(|e| format!("video branch: {e}"))?;
    let sink = bin.by_name("video_sink").ok_or("video sink missing")?;
    let queue = bin.by_name("video_queue");
    let renderer_name = sink.factory().map(|f| f.name().to_string()).unwrap_or_default();
    if deep {
        present_frames(&sink, window_id.ok_or("A 10-bit stream needs the video window")?, state.clone(), quit.clone(), lsfg)?;
    } else if let Some(xid) = window_id {
        let overlay = sink.clone().dynamic_cast::<gstreamer_video::VideoOverlay>()
            .map_err(|_| "sink does not implement VideoOverlay")?;
        unsafe { overlay.set_window_handle(xid as usize) };
    }
    {
        let mut st = state.lock().unwrap();
        st.decoder_name = decoder.clone();
        st.renderer_name = renderer_name.clone();
        st.conversion = conversion.clone();
    }

    // Branch entry: first observation of each frame, by RTP timestamp for RTP.
    let depay = bin.by_name(if rtp { "depay" } else { "parser" }).ok_or("branch entry missing")?;
    let st = state.clone();
    depay.static_pad("sink").unwrap().add_probe(gst::PadProbeType::BUFFER, move |_pad, info| {
        let Some(buffer) = info.buffer() else { return gst::PadProbeReturn::Ok };
        let Some(pts) = buffer.pts().map(|t| t.nseconds()) else { return gst::PadProbeReturn::Ok };
        let now = clock::now_ms();
        let rtp_key = if rtp && buffer.size() >= 12 {
            buffer.map_readable().ok().map(|map| {
                let data = map.as_slice();
                u64::from_be_bytes([data[4], data[5], data[6], data[7], data[8], data[9], data[10], data[11]])
            })
        } else {
            None
        };
        let mut st = st.lock().unwrap();
        let first_packet = rtp_key.and_then(|key| st.ingress.get(&key).copied());
        let received_at = first_packet.unwrap_or(now);
        let mut capture_at = None;
        let mut sr_wall_gap_ms = None;
        if rtp && st.capture_diagnostics {
            if let Ok(map) = buffer.map_readable() {
                let mut transport = st.transport.lock().unwrap();
                capture_at = transport.observe_rtp(map.as_slice(), now, clock::wall_ms());
                sr_wall_gap_ms = transport.sr_wall_gap_ms();
            }
        }
        st.remember_timing(pts, Timing { received_at, post_jitter_at: now, pre_jitter: first_packet.is_some(),
            decoded_at: None, sink_at: None, capture_at, sr_wall_gap_ms });
        gst::PadProbeReturn::Ok
    });

    // parser src: encoded frame/byte counters (Python `encoded` probe).
    let parser = bin.by_name("parser").ok_or("parser missing")?;
    let st = state.clone();
    parser.static_pad("src").unwrap().add_probe(gst::PadProbeType::BUFFER, move |_pad, info| {
        if let Some(buffer) = info.buffer() {
            let mut st = st.lock().unwrap();
            st.encoded_frames += 1;
            st.encoded_bytes += buffer.size() as u64;
        }
        gst::PadProbeReturn::Ok
    });

    // decoder src: decodedAt + resolution-change video event.
    let decoder_el = bin.by_name("decoder").ok_or("decoder missing")?;
    let st = state.clone();
    let decoder_name = decoder.clone();
    let renderer_label = renderer_name.clone();
    let conversion_label = conversion.clone();
    decoder_el.static_pad("src").unwrap().add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
        let Some(buffer) = info.buffer() else { return gst::PadProbeReturn::Ok };
        let now = clock::now_ms();
        let mut st = st.lock().unwrap();
        st.decoded_frames += 1;
        if let Some(pts) = buffer.pts().map(|t| t.nseconds()) {
            if let Some(timing) = st.frame_timing.get_mut(&pts) { timing.decoded_at = Some(now); }
        }
        if let Some(caps) = pad.current_caps() {
            if let Some(s) = caps.structure(0) {
                let (Ok(w), Ok(h)) = (s.get::<i32>("width"), s.get::<i32>("height")) else {
                    return gst::PadProbeReturn::Ok;
                };
                let (w, h) = (w as u32, h as u32);
                if st.last_video_dims != Some((w, h)) {
                    st.last_video_dims = Some((w, h));
                    st.width = w;
                    st.height = h;
                    protocol::event("video", json!({
                        "width": w, "height": h,
                        "decoder": decoder_name,
                        "renderer": renderer_label,
                        "conversion": conversion_label,
                        "motionProbe": null,
                    }));
                }
            }
        }
        gst::PadProbeReturn::Ok
    });

    // queue counters (vulkan path).
    if let Some(queue) = queue.clone() {
        let st = state.clone();
        queue.static_pad("sink").unwrap().add_probe(gst::PadProbeType::BUFFER, move |_pad, info| {
            let mut st = st.lock().unwrap();
            st.queue_in += 1;
            if st.benchmark {
                if let Some(pts) = info.buffer().and_then(|b| b.pts()).map(|t| t.nseconds()) {
                    st.queue_times.insert(pts, crate::state::Stage { entered: clock::now_ms(), ..Default::default() });
                    if st.queue_times.len() > 180 {
                        let oldest = st.queue_times.keys().next().copied();
                        if let Some(key) = oldest { st.queue_times.remove(&key); }
                    }
                }
            }
            gst::PadProbeReturn::Ok
        });
        let st = state.clone();
        queue.static_pad("src").unwrap().add_probe(gst::PadProbeType::BUFFER, move |_pad, info| {
            let mut st = st.lock().unwrap();
            st.queue_out += 1;
            if st.benchmark {
                if let Some(pts) = info.buffer().and_then(|b| b.pts()).map(|t| t.nseconds()) {
                    if let Some(stage) = st.queue_times.get_mut(&pts) {
                        stage.dequeued = Some(clock::now_ms());
                    }
                }
            }
            gst::PadProbeReturn::Ok
        });
    }
    if vulkan && state.lock().unwrap().benchmark {
        if let Some(upload) = bin.by_name("upload") {
            let st = state.clone();
            upload.static_pad("sink").unwrap().add_probe(gst::PadProbeType::BUFFER, move |_pad, info| {
                let mut st = st.lock().unwrap();
                if let Some(pts) = info.buffer().and_then(|b| b.pts()).map(|t| t.nseconds()) {
                    if let Some(stage) = st.queue_times.get_mut(&pts) {
                        stage.pre_upload = Some(clock::now_ms());
                    }
                }
                gst::PadProbeReturn::Ok
            });
        }
    }

    // sink pad: rendered counter, sinkAt timing, LSFG layer check.
    let st = state.clone();
    let sink_for_probe = sink.clone();
    let has_stats = sink.factory().is_some_and(|f| f.find_property("stats").is_some());
    let quit_for_lsfg = quit.clone();
    // The worker's own presenter checks once it has made its Vulkan instance.
    let lsfg_checked = std::sync::atomic::AtomicBool::new(!lsfg || deep);
    sink.static_pad("sink").unwrap().add_probe(gst::PadProbeType::BUFFER, move |_pad, info| {
        let Some(buffer) = info.buffer() else { return gst::PadProbeReturn::Ok };
        let now = clock::now_ms();
        if !lsfg_checked.swap(true, std::sync::atomic::Ordering::Relaxed) {
            if let Err(error) = lsfg_loaded() {
                protocol::error(error);
                quit_for_lsfg.quit();
                return gst::PadProbeReturn::Drop;
            }
        }
        let mut st = st.lock().unwrap();
        st.rendered_frames += 1;
        if has_stats {
            let stats = sink_for_probe.property::<gst::Structure>("stats");
            if let Ok(dropped) = stats.get::<u64>("dropped") { st.sink_dropped = dropped; }
        }
        let pts = buffer.pts().map(|t| t.nseconds());
        if let Some(pts) = pts {
            let hud_enabled = st.hud_enabled;
            let latency = st.frame_timing.get_mut(&pts).and_then(|timing| {
                timing.sink_at = Some(now);
                (timing.post_jitter_at > 0.0).then(|| now - timing.post_jitter_at)
            });
            // The worker's own presenter measures up to its present call instead.
            if hud_enabled && !deep {
                if let Some(ms) = latency.filter(|ms| *ms >= 0.0 && *ms < 10_000.0) {
                    st.hud_video_latency.push_back(ms);
                    while st.hud_video_latency.len() > 60 { st.hud_video_latency.pop_front(); }
                }
            }
            if st.benchmark && vulkan {
                if let Some(stage) = st.queue_times.remove(&pts) {
                    if let (Some(dequeued), Some(pre_upload)) = (stage.dequeued, stage.pre_upload) {
                        st.stage_profile.push((dequeued - stage.entered, pre_upload - dequeued,
                            now - pre_upload, 0.0, false));
                    }
                }
            }
            if st.benchmark {
                if let Some(caps) = sink_for_probe.static_pad("sink").and_then(|p| p.current_caps()) {
                    crate::trial::observe_sink_frame(&mut st, buffer, &caps, now, pts);
                }
            }
            crate::hud::emit_video_timing(&mut st, pts, now);
        }
        gst::PadProbeReturn::Ok
    });

    let branch = VideoBranch { bin: bin.clone(), sink, queue, decoder, renderer: renderer_name, conversion };
    pipeline.add(&bin).map_err(|e| format!("video add: {e}"))?;
    pad.link(&bin.static_pad("sink").ok_or("branch sink pad missing")?)
        .map_err(|e| format!("video link: {e}"))?;
    bin.sync_state_with_parent().map_err(|e| format!("video state: {e}"))?;
    Ok(branch)
}

/// Holds the newest value for a taker that can be slower than the giver.
struct Latest<T> {
    slot: Mutex<Option<T>>,
    given: std::sync::Condvar,
}

impl<T> Latest<T> {
    fn new() -> Self {
        Self { slot: Mutex::new(None), given: std::sync::Condvar::new() }
    }

    /// Puts `value` in place of one that was not taken. Returns whether
    /// there was one.
    fn put(&self, value: T) -> bool {
        let replaced = self.slot.lock().unwrap().replace(value).is_some();
        self.given.notify_one();
        replaced
    }

    /// Takes the value, waiting up to `limit` for one.
    fn take(&self, limit: std::time::Duration) -> Option<T> {
        let (mut slot, _) = self.given.wait_timeout_while(self.slot.lock().unwrap(), limit, |slot| slot.is_none()).unwrap();
        slot.take()
    }
}

/// Shows the frames that reach `sink` in the window `xid` with the worker's
/// own presenter, on a thread of its own. Showing a frame can wait for the
/// screen. Frames that come in a burst after a network stall must not each
/// take that wait, or the picture stays behind for good: the newest frame
/// replaces one that is not shown yet. A frame that cannot be shown ends the
/// worker.
/// Whether Vulkan loaded the lsfg-vk layer into the worker.
fn lsfg_loaded() -> Result<(), String> {
    if std::fs::read_to_string("/proc/self/maps").is_ok_and(|maps| maps.contains("liblsfg-vk-layer.so")) {
        protocol::event("lsfg-ready", json!({}));
        Ok(())
    } else {
        Err("The lsfg-vk 2.x layer was not loaded by Vulkan".into())
    }
}

fn present_frames(sink: &gst::Element, xid: u64, state: Arc<Mutex<State>>, quit: glib::MainLoop, lsfg: bool) -> Result<(), String> {
    let sink = sink.clone().downcast::<gstreamer_app::AppSink>().map_err(|_| "the 10-bit sink is not an appsink")?;
    let latest = Arc::new(Latest::new());
    let (decoded, stale) = (latest.clone(), state.clone());
    sink.set_callbacks(gstreamer_app::AppSinkCallbacks::builder().new_sample(move |sink| {
        let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
        if decoded.put((sample, sink.current_running_time())) { stale.lock().unwrap().sink_dropped += 1; }
        Ok(gst::FlowSuccess::Ok)
    }).build());
    // The thread lasts as long as the worker, which plays one stream; it must
    // not be what keeps the sink alive.
    let sink = sink.downgrade();
    std::thread::Builder::new().name("present".into()).spawn(move || {
        let mut presenter = None::<crate::present::Presenter>;
        let mut peak_read = std::time::Instant::now();
        loop {
            // The wait for the screen comes first, so the frame taken after it is the newest.
            if let Some(Err(error)) = presenter.as_mut().map(|presenter| presenter.ready()) {
                protocol::error(error);
                quit.quit();
                break;
            }
            let sample = latest.take(std::time::Duration::from_millis(500));
            let Some(sink) = sink.upgrade() else { break };
            let Some((sample, decoded_at)) = sample else { continue };
            let taken_at = sink.current_running_time();
            let mut steps = [0.0; 2];
            let mut show = || -> Result<(), String> {
                let info = gstreamer_video::VideoInfo::from_caps(sample.caps().ok_or("frame without a format")?)
                    .map_err(|error| format!("frame format: {error}"))?;
                let frame = gstreamer_video::VideoFrameRef::from_buffer_ref_readable(sample.buffer().ok_or("empty frame")?, &info)
                    .map_err(|error| format!("frame: {error}"))?;
                let (display, layer) = { let state = state.lock().unwrap(); (state.hdr_display, state.overlay_layer.clone()) };
                if presenter.is_none() {
                    presenter = Some(crate::present::Presenter::new(xid)?);
                    // Vulkan loads the layer with the presenter, not before.
                    if lsfg { lsfg_loaded()?; }
                }
                presenter.as_mut().unwrap().show(&frame, display, layer)?;
                steps = presenter.as_ref().unwrap().steps;
                // Once a second is enough for the stats.
                if peak_read.elapsed() >= std::time::Duration::from_secs(1) {
                    peak_read = std::time::Instant::now();
                    state.lock().unwrap().hdr_peak_nits = crate::present::hdr_peak(&frame);
                }
                Ok(())
            };
            if let Err(error) = show() {
                protocol::error(error);
                quit.quit();
                break;
            }
            // A frame is stamped with the pipeline's time when it arrives, so
            // the time since then is how long it took to reach the screen.
            let arrived = sample.buffer().and_then(|buffer| buffer.pts());
            if let (Some(arrived), Some(decoded), Some(taken), Some(now)) = (arrived, decoded_at, taken_at, sink.current_running_time()) {
                let between = |from: gst::ClockTime, to: gst::ClockTime| (to.nseconds() as f64 - from.nseconds() as f64) / 1e6;
                let ms = between(arrived, now);
                let mut state = state.lock().unwrap();
                if state.hud_enabled && (0.0..10_000.0).contains(&ms) {
                    state.hud_video_latency.push_back(ms);
                    while state.hud_video_latency.len() > 60 { state.hud_video_latency.pop_front(); }
                    state.present_stages.push_back([between(arrived, decoded), between(decoded, taken), steps[0], steps[1]]);
                    while state.present_stages.len() > 60 { state.present_stages.pop_front(); }
                }
            }
        }
    }).map_err(|error| format!("presenter thread: {error}"))?;
    Ok(())
}

/// Reports a pipeline error and ends the worker. The watch lasts as long as
/// the returned guard.
pub fn watch_errors(pipeline: &gst::Pipeline, quit: glib::MainLoop) -> Result<gst::bus::BusWatchGuard, String> {
    pipeline.bus().ok_or("pipeline bus missing")?.add_watch_local(move |_bus, message| {
        if let gst::MessageView::Error(error) = message.view() {
            protocol::error(error.error().to_string());
            quit.quit();
        }
        glib::ControlFlow::Continue
    }).map_err(|error| format!("bus watch: {error}"))
}

pub fn attach_audio(pipeline: &gst::Pipeline, pad: &gst::Pad, volume: u32, rtp: bool) -> Result<(), String> {
    let sink = if gst::ElementFactory::find("pulsesink").is_some() { "pulsesink" } else { "autoaudiosink" };
    // RTP from an Xbox has passed a jitter buffer and plays by its clock. A PC
    // sends bare packets stamped on arrival: played by the clock, a sound
    // server that takes 90 ms at a time (more over Bluetooth) finds most of
    // them late and drops them. They are played as they come instead, and a
    // backlog over 100 ms is dropped so the delay cannot grow.
    let (depay, output) = audio_output(rtp, sink);
    let backlog = if rtp { "" } else { "queue max-size-buffers=0 max-size-bytes=0 max-size-time=100000000 leaky=downstream ! " };
    let description = format!(
        "{depay}opusdec ! audioconvert ! audioresample ! volume volume={} ! {backlog}{sink} {output}",
        volume as f64 / 100.0);
    let bin = gst::parse::bin_from_description(&description, true)
        .map_err(|e| format!("audio branch: {e}"))?;
    pipeline.add(&bin).map_err(|e| format!("audio add: {e}"))?;
    pad.link(&bin.static_pad("sink").ok_or("audio sink pad missing")?)
        .map_err(|e| format!("audio link: {e}"))?;
    bin.sync_state_with_parent().map_err(|e| format!("audio state: {e}"))?;
    Ok(())
}

pub fn wire_rtcp(webrtc: &gst::Element, state: Arc<Mutex<State>>) {
    let Some(rtpbin) = webrtc.clone().dynamic_cast::<gst::Bin>().ok().and_then(|bin| bin.by_name("rtpbin")) else { return };
    let st = state.clone();
    rtpbin.connect_pad_added(move |_bin, pad| {
        if !pad.name().starts_with("recv_rtcp_sink_") { return; }
        let st = st.clone();
        pad.add_probe(gst::PadProbeType::BUFFER, move |_pad, info| {
            let Some(buffer) = info.buffer() else { return gst::PadProbeReturn::Ok };
            if let Ok(map) = buffer.map_readable() {
                st.lock().unwrap().transport.lock().unwrap()
                    .observe_rtcp(map.as_slice(), clock::wall_ms());
            }
            gst::PadProbeReturn::Ok
        });
    });
}

pub fn wire_ingress(webrtc: &gst::Element, state: Arc<Mutex<State>>) {
    let Some(rtpbin) = webrtc.clone().dynamic_cast::<gst::Bin>().ok().and_then(|bin| bin.by_name("rtpbin")) else { return };
    let st = state.clone();
    rtpbin.connect("new-jitterbuffer", false, move |values| {
        let Ok(jitter) = values[1].get::<gst::Element>() else { return None };
        let st = st.clone();
        if let Some(pad) = jitter.static_pad("sink") {
            pad.add_probe(gst::PadProbeType::BUFFER, move |_pad, info| {
                let Some(buffer) = info.buffer() else { return gst::PadProbeReturn::Ok };
                if buffer.size() >= 12 {
                    if let Ok(map) = buffer.map_readable() {
                        let data = map.as_slice();
                        let key = u64::from_be_bytes([data[4], data[5], data[6], data[7], data[8], data[9], data[10], data[11]]);
                        st.lock().unwrap().remember_ingress(key, clock::now_ms());
                    }
                }
                gst::PadProbeReturn::Ok
            });
        }
        None
    });
}
#[cfg(test)]
mod tests {
    #[test]
    fn the_fallback_audio_sink_gets_no_pulseaudio_options() {
        assert!(super::audio_output(false, "pulsesink").1.contains("buffer-time"));
        assert_eq!(super::audio_output(false, "autoaudiosink").1, "sync=false");
        assert_eq!(super::audio_output(true, "autoaudiosink"), ("rtpopusdepay ! ", "sync=true"));
    }

    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn a_slow_taker_gets_the_newest_value_only() {
        let latest = Latest::new();
        assert_eq!(latest.take(Duration::ZERO), None::<u32>);
        // Three frames come in a burst before the first is shown: two are late.
        assert_eq!([latest.put(1), latest.put(2), latest.put(3)], [false, true, true]);
        assert_eq!(latest.take(Duration::ZERO), Some(3));
        assert_eq!(latest.take(Duration::ZERO), None);
        // A taker that waits is woken by the next value.
        let latest = Arc::new(latest);
        let giver = latest.clone();
        let given = std::thread::spawn(move || { std::thread::sleep(Duration::from_millis(30)); giver.put(4) });
        let waiting = std::time::Instant::now();
        assert_eq!(latest.take(Duration::from_secs(5)), Some(4));
        // Woken, not timed out.
        assert!(waiting.elapsed() < Duration::from_secs(2));
        assert!(!given.join().unwrap());
    }

    #[test]
    fn a_preferred_decoder_goes_first_only_for_its_own_codec() {
        assert_eq!(decoders(Codec::H264, Some("avdec_h264")).unwrap(), ["avdec_h264", "vah264dec"]);
        assert_eq!(decoders(Codec::H264, Some("vulkanh264dec")).unwrap(), ["vulkanh264dec", "vah264dec", "avdec_h264"]);
        // The launcher's H.264 decoder cannot take an HEVC stream.
        assert_eq!(decoders(Codec::Hevc, Some("avdec_h264")).unwrap(), ["vah265dec", "avdec_h265"]);
        assert_eq!(decoders(Codec::Hevc, None).unwrap(), ["vah265dec", "avdec_h265"]);
        assert!(decoders(Codec::Av1, None).is_err());
    }

    /// Posts a pipeline error and runs the loop until it quits or `limit` ends.
    /// Returns whether the error ended the loop.
    fn error_quits(pipeline: &gst::Pipeline, main_loop: &glib::MainLoop, limit: Duration) -> bool {
        let timed_out = Arc::new(AtomicBool::new(false));
        let (flag, quit) = (timed_out.clone(), main_loop.clone());
        let timeout = glib::timeout_add_local_once(limit, move || {
            flag.store(true, Ordering::Relaxed);
            quit.quit();
        });
        let error = gst::message::Error::builder(gst::CoreError::Failed, "test error").src(pipeline).build();
        pipeline.bus().unwrap().post(error).unwrap();
        main_loop.run();
        let ended = !timed_out.load(Ordering::Relaxed);
        if ended { timeout.remove(); }
        ended
    }

    #[test]
    fn a_pipeline_error_ends_the_worker_while_the_watch_is_kept() {
        gst::init().unwrap();
        let pipeline = gst::Pipeline::new();
        let main_loop = glib::MainLoop::new(None, false);
        let watch = watch_errors(&pipeline, main_loop.clone()).unwrap();
        assert!(error_quits(&pipeline, &main_loop, Duration::from_secs(5)));
        // Without the guard nothing reports the error.
        drop(watch);
        assert!(!error_quits(&pipeline, &main_loop, Duration::from_millis(200)));
    }
}
