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

pub fn refresh_hud(state: &Arc<Mutex<State>>, webrtc: Option<&gstreamer::Element>, queue: Option<&gstreamer::Element>) {
    let now = clock::now_ms();
    let snapshot = {
        let mut st = state.lock().unwrap();
        match st.hud_snapshot {
            Some((prev_t, prev_dec, prev_bytes)) => {
                let dt = (now - prev_t).max(1e-3);
                let fps = (st.decoded_frames.saturating_sub(prev_dec)) as f64 * 1000.0 / dt;
                let mbps = (st.encoded_bytes.saturating_sub(prev_bytes)) as f64 * 8.0 / dt / 1e3;
                st.hud_snapshot = Some((now, st.decoded_frames, st.encoded_bytes));
                let depth = st.queue_depth(queue);
                let client_ms = median(st.hud_video_latency.iter().copied());
                Some((fps, mbps, st.encoded_bytes, st.encoded_frames, (st.width, st.height, st.viewport,
                    depth, st.queue_drops(depth), st.sink_dropped,
                    st.hud_transport.rtt_ms, st.hud_transport.jitter_ms, st.hud_transport.rtp_lost,
                    st.decoder_name.clone(), client_ms)))
            }
            None => {
                // First sample after the HUD was enabled: remember the
                // counters so the next tick measures a real interval.
                st.hud_snapshot = Some((now, st.decoded_frames, st.encoded_bytes));
                None
            }
        }
    };
    let Some((fps, mbps, bytes, frames,
        (width, height, viewport, depth, drops, sink_drops, rtt, jitter, lost, decoder, client_ms))) = snapshot else {
        request_hud_transport(state, webrtc);
        return;
    };
    {
        let mut st = state.lock().unwrap();
        let ms = |value: Option<f64>| match value {
            None => "—".to_string(),
            Some(v) if v < 1.0 => format!("{v:.2} ms"),
            Some(v) => format!("{v:.0} ms"),
        };
        let loss = lost.map(|v| v.to_string()).unwrap_or_else(|| "—".to_string());
        let russian = st.russian;
        let drops_word = if russian { "проп." } else { "drops" };
        let video_label = if russian { "Видео" } else { "Video" };
        let decode_label = if russian { "Декодер" } else { "Decode" };
        let stream_size = match st.scaled_to {
            Some(((_, screen_height), _)) => format!("{height}p→{screen_height}p"),
            None => format!("{height}p"),
        };
        st.hud_compact_rate = format!("{} · {} {:.0} FPS · {} {}",
            stream_size, decode_label, fps, drops_word, drops + sink_drops);
        st.hud_compact = format!("{:.1} Mb/s · RTT {} · {} {}",
            mbps, ms(rtt), video_label, ms(client_ms));
        let limit = crate::LIMIT_KBPS.load(std::sync::atomic::Ordering::Relaxed);
        // The worker's own presenter measures up to the present call.
        let latency_line = match (st.present_stages.is_empty(), russian) {
            (true, true) => format!("Видео: после джиттер-буфера → вход видеовывода {}", ms(client_ms)),
            (true, false) => format!("Video: after jitter buffer → sink input {}", ms(client_ms)),
            (false, true) => format!("Видео: приход кадра → передан экрану {}", ms(client_ms)),
            (false, false) => format!("Video: frame arrival → given to the screen {}", ms(client_ms)),
        };
        let hdr = st.hdr_peak_nits.map(|nits| if russian { format!("HDR10 · пик кадра ~{nits:.0} нит") }
            else { format!("HDR10 · frame peak ~{nits:.0} nits") });
        st.hud_metrics = vec![
            format!("Video {}×{} · {:.1} decoded FPS", width, height, fps),
            format!("Viewport {}×{} · {} · LSFG {}", viewport.0, viewport.1, decoder,
                if st.lsfg { "on" } else { "off" }),
            format!("Video {:.1} Mb/s · limit {} Mb/s", mbps, limit as f64 / 1000.0),
            format!("RTT {} · jitter {} · RTP lost {}", ms(rtt), ms(jitter), loss),
            latency_line,
            format!("Queue {} · queue drops ~{} · sink drops {}", depth, drops, sink_drops),
        ];
        st.hud_metrics.extend(hdr);
        if let Some(((screen_width, screen_height), filter)) = st.scaled_to {
            st.hud_metrics.push(if russian {
                format!("Поток {width}×{height} → экран {screen_width}×{screen_height} · SteamOS {filter}")
            } else {
                format!("Stream {width}×{height} → screen {screen_width}×{screen_height} · SteamOS {filter}")
            });
        }
        if !st.present_stages.is_empty() {
            let stage = |index: usize| median(st.present_stages.iter().map(|stages| stages[index])).unwrap_or(0.0);
            let line = format!("Decode {:.1} · wait {:.1} · copy {:.1} · present {:.1} ms",
                stage(0), stage(1), stage(2), stage(3));
            st.hud_metrics.push(line);
        }
        if let Some(title) = st.preview.clone() {
            st.hud_compact_rate = title;
            st.hud_compact = if russian {
                format!("{height}p · источник {fps:.0} FPS · A сравнить · B назад")
            } else {
                format!("{height}p · source {fps:.0} FPS · A compare · B back")
            };
            st.hud_metrics.clear();
        }
    }
    protocol::event("hud-stats", json!({
        "width": width,
        "height": height,
        "viewportWidth": viewport.0,
        "viewportHeight": viewport.1,
        "decodedFps": fps,
        "videoMbps": mbps,
        "encodedBytes": bytes,
        "encodedFrames": frames,
        "limitKbps": limit_kbps(),
        "rttMs": rtt,
        "jitterMs": jitter,
        "rtpLost": lost,
        "videoClientMs": client_ms,
        "queueDepth": depth,
        "queueDrops": drops,
        "sinkDrops": sink_drops,
    }));
    request_hud_transport(state, webrtc);
}

fn median(samples: impl Iterator<Item = f64>) -> Option<f64> {
    let mut values: Vec<f64> = samples.filter(|v| v.is_finite()).collect();
    if values.is_empty() { return None; }
    values.sort_by(|a, b| a.total_cmp(b));
    let mid = values.len() / 2;
    Some(if values.len() % 2 == 0 { (values[mid - 1] + values[mid]) / 2.0 } else { values[mid] })
}

fn limit_kbps() -> u32 {
    crate::LIMIT_KBPS.load(std::sync::atomic::Ordering::Relaxed)
}

/// HUD rtt/jitter/loss from get-stats (Python hud_transport_reply). Report
/// fields are keyed by candidate/pair id, not by an inner "id" field.
pub fn request_hud_transport(state: &Arc<Mutex<State>>, webrtc: Option<&gstreamer::Element>) {
    let Some(webrtc) = webrtc else { return };
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
        let pair = selected.as_deref()
            .and_then(|id| records.iter().find(|(name, _)| name == id).map(|(_, s)| s))
            .or_else(|| records.iter().find(|(_, s)| s.get::<bool>("selected") == Ok(true)).map(|(_, s)| s));
        if let Some(pair) = pair {
            if let Some(rtt) = number(pair, "current-round-trip-time") {
                st.hud_transport.rtt_ms = Some(rtt * 1000.0);
            }
        }
        if let Some((_, inbound)) = records.iter().find(|(_, s)| {
            s.name().as_str() == "inbound-rtp" && s.get::<String>("kind").as_deref() == Ok("video")
        }) {
            if let Some(jitter) = number(inbound, "jitter") {
                st.hud_transport.jitter_ms = Some(jitter * 1000.0);
            }
            if let Some(lost) = signed(inbound, "packets-lost") {
                st.hud_transport.rtp_lost = Some(lost);
            }
        }
    });
    webrtc.emit_by_name::<()>("get-stats", &[&None::<gstreamer::Pad>, &promise]);
}

fn number(structure: &gstreamer::Structure, key: &str) -> Option<f64> {
    structure.get::<f64>(key).ok()
        .or_else(|| structure.get::<u64>(key).ok().map(|v| v as f64))
        .or_else(|| structure.get::<i64>(key).ok().map(|v| v as f64))
        .or_else(|| structure.get::<u32>(key).ok().map(|v| v as f64))
        .or_else(|| structure.get::<i32>(key).ok().map(|v| v as f64))
}

fn signed(structure: &gstreamer::Structure, key: &str) -> Option<i64> {
    structure.get::<i64>(key).ok()
        .or_else(|| structure.get::<i32>(key).ok().map(|v| v as i64))
        .or_else(|| structure.get::<u64>(key).ok().map(|v| v as i64))
        .or_else(|| structure.get::<u32>(key).ok().map(|v| v as i64))
        .or_else(|| structure.get::<f64>(key).ok().map(|v| v as i64))
}

#[cfg(test)]
mod tests {
    #[test]
    fn median_handles_empty_odd_even_and_invalid() {
        assert_eq!(super::median(std::iter::empty()), None);
        assert_eq!(super::median([30.0, 10.0, 20.0].into_iter()), Some(20.0));
        assert_eq!(super::median([40.0, 10.0, 30.0, 20.0].into_iter()), Some(25.0));
        assert_eq!(super::median([f64::NAN, 12.0, f64::INFINITY].into_iter()), Some(12.0));
    }
}