//! stats / hud-stats / video-timing emission, mirroring the Python worker's
//! cadence and field names (host journal mapping in src/media.rs).

use crate::{clock, protocol, state::State};
use gstreamer::prelude::*;
use serde_json::json;
use std::sync::{Arc, Mutex};

pub fn emit_stats(state: &mut State, queue: Option<&gstreamer::Element>, webrtc: Option<&gstreamer::Element>) {
    if state.capture_diagnostics {
        if let Some(webrtc) = webrtc {
            crate::transport::request_transport_stats(webrtc, &state.transport);
        }
    }
    if !state.stage_profile.is_empty() || !state.motion_profile.is_empty() {
        protocol::event("vulkan-stage-profile", json!({
            "time": clock::now_ms(),
            "width": state.width,
            "height": state.height,
            "samples": state.stage_profile.iter()
                .map(|(a, b, c, d, e)| json!([a, b, c, d, e])).collect::<Vec<_>>(),
            "motion": state.motion_profile,
        }));
        state.stage_profile.clear();
        state.motion_profile.clear();
    }
    let depth = state.queue_depth(queue);
    protocol::event("stats", json!({
        "time": clock::now_ms(),
        "width": state.width,
        "height": state.height,
        "encodedFrames": state.encoded_frames,
        "encodedBytes": state.encoded_bytes,
        "decodedFrames": state.decoded_frames,
        "renderedFrames": state.rendered_frames,
        "sinkDropped": state.sink_dropped,
        "queueInput": state.queue_in,
        "queueOutput": state.queue_out,
        "queueDepth": depth,
    }));
}

pub fn emit_video_timing(state: &mut State, pts: u64, sink_at: f64) {
    if !state.benchmark { return; }
    if sink_at - state.last_video_timing_emit < 250.0 { return; }
    let Some(timing) = state.frame_timing.get(&pts) else { return };
    state.last_video_timing_emit = sink_at;
    protocol::event("video-timing", json!({
        "receivedAt": timing.received_at,
        "sinkAt": sink_at,
        "preJitter": timing.pre_jitter,
        "captureAt": timing.capture_at,
        "srWallGapMs": timing.sr_wall_gap_ms,
    }));
}

pub fn refresh_hud(state: &Arc<Mutex<State>>, webrtc: &gstreamer::Element, queue: Option<&gstreamer::Element>) {
    let now = clock::now_ms();
    let (fps, mbps, snapshot) = {
        let mut st = state.lock().unwrap();
        let (prev_t, prev_dec, prev_bytes) = st.hud_snapshot.unwrap_or((now, st.decoded_frames, st.encoded_bytes));
        let dt = (now - prev_t).max(1e-3);
        let fps = (st.decoded_frames.saturating_sub(prev_dec)) as f64 * 1000.0 / dt;
        let mbps = (st.encoded_bytes.saturating_sub(prev_bytes)) as f64 * 8.0 / dt / 1e6;
        st.hud_snapshot = Some((now, st.decoded_frames, st.encoded_bytes));
        (fps, mbps, (st.width, st.height, st.viewport,
            st.queue_depth(queue), st.queue_drops(st.queue_depth(queue)), st.sink_dropped,
            st.hud_transport.rtt_ms, st.hud_transport.jitter_ms, st.hud_transport.rtp_lost,
            st.decoder_name.clone()))
    };
    let (width, height, viewport, depth, drops, sink_drops, rtt, jitter, lost, decoder) = snapshot;
    {
        let mut st = state.lock().unwrap();
        let ms = |value: Option<f64>| match value {
            None => "—".to_string(),
            Some(v) if v < 1.0 => format!("{v:.2} ms"),
            Some(v) => format!("{v:.0} ms"),
        };
        let loss = lost.map(|v| v.to_string()).unwrap_or_else(|| "—".to_string());
        let drops_word = if st.russian { "проп." } else { "drops" };
        st.hud_compact = format!("{}p · {:.0} FPS · {:.1} Mb/s · {} · {} {}",
            height, fps, mbps, ms(rtt), drops + sink_drops, drops_word);
        let limit = crate::LIMIT_KBPS.load(std::sync::atomic::Ordering::Relaxed);
        st.hud_metrics = vec![
            format!("Video {}×{} · {:.1} decoded FPS", width, height, fps),
            format!("Viewport {}×{} · {} · LSFG {}", viewport.0, viewport.1, decoder,
                if st.lsfg { "on" } else { "off" }),
            format!("Video {:.1} Mb/s · limit {} Mb/s", mbps, limit as f64 / 1000.0),
            format!("RTT {} · jitter {} · RTP lost {}", ms(rtt), ms(jitter), loss),
            format!("Queue {} · queue drops ~{} · sink drops {}", depth, drops, sink_drops),
        ];
    }
    protocol::event("hud-stats", json!({
        "width": width,
        "height": height,
        "viewportWidth": viewport.0,
        "viewportHeight": viewport.1,
        "decodedFps": fps,
        "videoMbps": mbps,
        "limitKbps": limit_kbps(),
        "rttMs": rtt,
        "jitterMs": jitter,
        "rtpLost": lost,
        "queueDepth": depth,
        "queueDrops": drops,
        "sinkDrops": sink_drops,
    }));
    request_hud_transport(state, webrtc);
}

fn limit_kbps() -> u32 {
    crate::LIMIT_KBPS.load(std::sync::atomic::Ordering::Relaxed)
}

/// HUD rtt/jitter/loss from get-stats (Python hud_transport_reply). Report
/// fields are keyed by candidate/pair id, not by an inner "id" field.
pub fn request_hud_transport(state: &Arc<Mutex<State>>, webrtc: &gstreamer::Element) {
    {
        let mut st = state.lock().unwrap();
        if st.hud_transport.pending { return; }
        st.hud_transport.pending = true;
    }
    let state = state.clone();
    let promise = gstreamer::Promise::with_change_func(move |reply| {
        let mut st = state.lock().unwrap();
        st.hud_transport.pending = false;
        let Ok(Some(reply)) = reply else { return };
        let records: Vec<(String, gstreamer::Structure)> = reply.iter()
            .filter_map(|(name, value)| value.get::<gstreamer::Structure>().ok().map(|s| (name.to_string(), s)))
            .collect();
        let selected = records.iter()
            .find_map(|(_, s)| s.get::<String>("selected-candidate-pair-id").ok());
        if let Some(pair) = selected.as_deref()
            .and_then(|id| records.iter().find(|(name, _)| name == id).map(|(_, s)| s))
        {
            if let Ok(rtt) = pair.get::<f64>("current-round-trip-time") {
                st.hud_transport.rtt_ms = Some(rtt * 1000.0);
            }
        }
        if let Some((_, inbound)) = records.iter().find(|(_, s)| {
            s.name().as_str() == "inbound-rtp" && s.get::<String>("kind").as_deref() == Ok("video")
        }) {
            if let Ok(jitter) = inbound.get::<f64>("jitter") {
                st.hud_transport.jitter_ms = Some(jitter * 1000.0);
            }
            if let Ok(lost) = inbound.get::<i64>("packets-lost") {
                st.hud_transport.rtp_lost = Some(lost);
            }
        }
    });
    webrtc.emit_by_name::<()>("get-stats", &[&None::<gstreamer::Pad>, &promise]);
}