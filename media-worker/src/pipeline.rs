//! Video/audio branch construction and pad probes, byte-parity with the
//! Python worker's element chains (media/gstreamer_player.py link_track).

use crate::{clock, protocol, state::{State, Timing}};
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

pub fn choose_decoder() -> Result<(String, String), String> {
    let mut choices: Vec<String> = vec!["vah264dec".into(), "avdec_h264".into()];
    if let Some(override_name) = std::env::var("ALLY_DECODER").ok().filter(|v| !v.is_empty()) {
        choices.retain(|c| *c != override_name);
        choices.insert(0, override_name);
    }
    let name = choices.iter()
        .find(|c| gst::ElementFactory::find(c).is_some())
        .ok_or("GStreamer H.264 decoder unavailable")?
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
) -> Result<VideoBranch, String> {
    let (decoder, decoder_opts) = choose_decoder()?;
    let vulkan = renderer == "vulkan";
    let mut conversion: Option<String> = None;
    let video_output = if vulkan {
        let queue = "queue name=video_queue max-size-buffers=2 max-size-bytes=0 max-size-time=0 leaky=downstream ! ";
        let download = if decoder == "vah264dec" { "vapostproc ! video/x-raw,format=NV12 ! " } else { "" };
        let chain = if decoder == "vulkanh264dec" {
            conversion = Some("vk_zero_copy".into());
            "vulkancolorconvert ! video/x-raw(memory:VulkanImage),format=BGRA ! vulkansink"
        } else if decoder == "vah264dec"
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
        let download = if decoder == "vah264dec" {
            "vapostproc ! video/x-raw,format=NV12 ! "
        } else if decoder == "vulkanh264dec" {
            "vulkandownload ! "
        } else {
            ""
        };
        format!("{download}videoconvert ! video/x-raw,format=BGRx ! ximagesink")
    } else {
        "videoconvert ! autovideosink".to_string()
    };
    let options = if vulkan || window_id.is_some() { "sync=false force-aspect-ratio=true" } else { "sync=false" };
    let description = format!(
        "rtph264depay name=depay ! h264parse name=parser ! {decoder} {decoder_opts} name=decoder ! \
         {video_output} name=video_sink {options}");
    let bin = gst::parse::bin_from_description(&description, true)
        .map_err(|e| format!("video branch: {e}"))?;
    let sink = bin.by_name("video_sink").ok_or("video sink missing")?;
    let queue = bin.by_name("video_queue");
    let renderer_name = sink.factory().map(|f| f.name().to_string()).unwrap_or_default();
    if let Some(xid) = window_id {
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

    // depay sink: first network observation of each frame's RTP timestamp.
    let depay = bin.by_name("depay").ok_or("depay missing")?;
    let st = state.clone();
    depay.static_pad("sink").unwrap().add_probe(gst::PadProbeType::BUFFER, move |_pad, info| {
        let Some(buffer) = info.buffer() else { return gst::PadProbeReturn::Ok };
        let Some(pts) = buffer.pts().map(|t| t.nseconds()) else { return gst::PadProbeReturn::Ok };
        let now = clock::now_ms();
        let rtp_key = if buffer.size() >= 12 {
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
        if st.capture_diagnostics {
            if let Ok(map) = buffer.map_readable() {
                let mut transport = st.transport.lock().unwrap();
                capture_at = transport.observe_rtp(map.as_slice(), now, clock::wall_ms());
                sr_wall_gap_ms = transport.sr_wall_gap_ms();
            }
        }
        st.remember_timing(pts, Timing { received_at, pre_jitter: first_packet.is_some(),
            decoded_at: None, sink_at: None, capture_at, sr_wall_gap_ms });
        gst::PadProbeReturn::Ok
    });

    // parser src: encoded frame/byte counters.
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
    let lsfg_checked = std::sync::atomic::AtomicBool::new(!lsfg);
    sink.static_pad("sink").unwrap().add_probe(gst::PadProbeType::BUFFER, move |_pad, info| {
        let Some(buffer) = info.buffer() else { return gst::PadProbeReturn::Ok };
        let now = clock::now_ms();
        if !lsfg_checked.swap(true, std::sync::atomic::Ordering::Relaxed) {
            let loaded = std::fs::read_to_string("/proc/self/maps")
                .is_ok_and(|maps| maps.contains("liblsfg-vk-layer.so"));
            if loaded {
                protocol::event("lsfg-ready", json!({}));
            } else {
                protocol::error("The lsfg-vk 2.x layer was not loaded by Vulkan");
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
            if let Some(timing) = st.frame_timing.get_mut(&pts) { timing.sink_at = Some(now); }
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

pub fn attach_audio(pipeline: &gst::Pipeline, pad: &gst::Pad, volume: u32) -> Result<(), String> {
    let sink = if gst::ElementFactory::find("pulsesink").is_some() { "pulsesink" } else { "autoaudiosink" };
    let description = format!(
        "rtpopusdepay ! opusdec ! audioconvert ! audioresample ! volume volume={} ! {sink} sync=true",
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