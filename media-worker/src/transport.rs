//! Transport diagnostics: abs-capture-time RTP extension parsing, RTCP
//! sender-report clock gaps, and webrtcbin get-stats transport reporting.
//! Port of media/gstreamer_player.py:803-813, 1039-1124, 1437-1458.

use crate::{clock, protocol};
use gstreamer as gst;
use gstreamer::prelude::*;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub const ABS_CAPTURE_TIME_URI: &str = "http://www.webrtc.org/experiments/rtp-hdrext/abs-capture-time";
pub const ABS_SEND_TIME_URI: &str = "http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time";

#[derive(Default)]
pub struct Transport {
    pub capture_extension: Option<u8>,
    pub send_time_offered: bool,
    pub capture_packets: u64,
    pub capture_timestamps: u64,
    pub capture_clock_offset: bool,
    pub capture_clock_offset_max_ms: f64,
    anchors: HashMap<u32, (u32, f64)>,
    sr_samples: HashMap<u32, Vec<f64>>,
    pub video_ssrcs: Vec<u32>,
}

/// Insert b=AS and (when capturing) the abs-capture-time extmap into the
/// m=video section, Python munge parity.
pub fn munge_offer(text: &str, bitrate: u32, capture: bool) -> (String, Option<u8>) {
    let mut lines: Vec<String> = text.split("\r\n").map(str::to_string).collect();
    let Some(start) = lines.iter().position(|line| line.starts_with("m=video ")) else {
        return (text.to_string(), None);
    };
    let end = lines.iter().skip(start + 1).position(|line| line.starts_with("m="))
        .map(|offset| start + 1 + offset)
        .unwrap_or(lines.len());
    let first_attr = lines[start..end].iter().position(|line| line.starts_with("a="))
        .map(|offset| start + offset)
        .unwrap_or(end);
    lines.insert(first_attr, format!("b=AS:{bitrate}"));
    let mut extension = None;
    if capture {
        let mut used: Vec<u8> = Vec::new();
        for line in &lines[start..end + 1] {
            if let Some(rest) = line.strip_prefix("a=extmap:") {
                if let Some(id) = rest.split(|c: char| c == '/' || c.is_whitespace()).next()
                    .and_then(|id| id.parse::<u8>().ok())
                {
                    used.push(id);
                }
            }
        }
        if let Some(id) = (1..=14).rev().find(|id| !used.contains(id)) {
            lines.insert(first_attr + 1, format!("a=extmap:{id}/recvonly {ABS_CAPTURE_TIME_URI}"));
            extension = Some(id);
        }
    }
    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    let mut out = lines.join("\r\n");
    out.push_str("\r\n");
    (out, extension)
}

/// Answer inspection: which capture/send-time extensions the console agreed to.
pub fn inspect_answer(text: &str) -> (Option<u8>, bool) {
    let mut in_video = false;
    let mut capture = None;
    let mut send_time = false;
    for line in text.split("\r\n") {
        if line.starts_with("m=") {
            in_video = line.starts_with("m=video ");
            continue;
        }
        if !in_video { continue; }
        if let Some(rest) = line.strip_prefix("a=extmap:") {
            let mut parts = rest.splitn(2, |c: char| c.is_whitespace());
            let id_part = parts.next().unwrap_or("");
            let uri = parts.next().unwrap_or("");
            let id = id_part.split('/').next().and_then(|id| id.parse::<u8>().ok());
            if uri == ABS_CAPTURE_TIME_URI { capture = id; }
            if uri == ABS_SEND_TIME_URI { send_time = true; }
        }
    }
    (capture, send_time)
}

pub fn rtp_key(data: &[u8]) -> Option<u64> {
    if data.len() < 12 { return None; }
    Some(u64::from_be_bytes([data[4], data[5], data[6], data[7], data[8], data[9], data[10], data[11]]))
}

/// Parse the abs-capture-time extension value (8 or 16 bytes) from an RTP packet.
pub fn read_capture_extension(data: &[u8], extension: u8) -> Option<&[u8]> {
    if data.len() < 16 { return None; }
    let cc = (data[0] & 15) as usize;
    let mut offset = 12 + 4 * cc;
    if data[0] & 16 == 0 { return None; }
    if data.len() < offset + 4 { return None; }
    let profile = u16::from_be_bytes([data[offset], data[offset + 1]]);
    let words = u16::from_be_bytes([data[offset + 2], data[offset + 3]]) as usize;
    if words > 256 { return None; }
    offset += 4;
    let end = offset + words * 4;
    if data.len() < end { return None; }
    if profile == 0xBEDE {
        let mut pos = offset;
        while pos < end {
            let header = data[pos];
            let id = header >> 4;
            let size = (header & 15) as usize + 1;
            if id == 15 { break; }
            if id == extension && (size == 8 || size == 16) && pos + 1 + size <= end {
                return Some(&data[pos + 1..pos + 1 + size]);
            }
            pos += 1 + size;
        }
    } else if (0x1000..=0x100f).contains(&profile) {
        let mut pos = offset;
        while pos + 2 <= end {
            let id = data[pos];
            let size = data[pos + 1] as usize;
            if id == extension && (size == 8 || size == 16) && pos + 2 + size <= end {
                return Some(&data[pos + 2..pos + 2 + size]);
            }
            pos += 2 + size;
        }
    }
    None
}

pub fn ntp_unix_ms(ntp: &[u8]) -> f64 {
    let sec = u32::from_be_bytes([ntp[0], ntp[1], ntp[2], ntp[3]]);
    let frac = u32::from_be_bytes([ntp[4], ntp[5], ntp[6], ntp[7]]);
    (sec as f64 - 2_208_988_800.0) * 1000.0 + frac as f64 * 1000.0 / 4_294_967_296.0
}

impl Transport {
    /// Record an RTP packet's capture extension; returns captureAt in the
    /// worker monotonic domain when an anchor exists.
    pub fn observe_rtp(&mut self, data: &[u8], now_mono: f64, now_wall: f64) -> Option<f64> {
        self.capture_packets += 1;
        let extension = self.capture_extension?;
        let value = read_capture_extension(data, extension)?;
        let key = rtp_key(data)?;
        let ssrc = u32::from_be_bytes([data[8], data[9], data[10], data[11]]);
        let rtp_ts = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
        if !self.video_ssrcs.contains(&ssrc) { self.video_ssrcs.push(ssrc); }
        let offset_ms = if value.len() == 16 {
            let raw = i64::from_be_bytes([value[8], value[9], value[10], value[11], value[12], value[13], value[14], value[15]]);
            let ms = raw as f64 * 1000.0 / 4_294_967_296.0;
            self.capture_clock_offset = true;
            self.capture_clock_offset_max_ms = self.capture_clock_offset_max_ms.max(ms.abs());
            ms
        } else {
            0.0
        };
        self.capture_timestamps += 1;
        let anchor = self.anchors.entry(ssrc).or_insert((rtp_ts, now_wall - offset_ms));
        let ticks = ((rtp_ts.wrapping_sub(anchor.0) as i32) as f64) / 90.0;
        if ticks.abs() > 2000.0 { return None; }
        Some(anchor.1 + ticks - (now_wall - now_mono))
    }

    /// Compound RTCP sender reports -> wall-clock gap samples per SSRC.
    pub fn observe_rtcp(&mut self, data: &[u8], now_wall: f64) {
        let mut pos = 0;
        while pos + 4 <= data.len() {
            let flags = data[pos];
            let pt = data[pos + 1];
            let words = u16::from_be_bytes([data[pos + 2], data[pos + 3]]) as usize;
            let size = (words + 1) * 4;
            if flags >> 6 != 2 || pos + size > data.len() { break; }
            if pt == 200 && size >= 28 {
                let ssrc = u32::from_be_bytes([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]]);
                let ntp = &data[pos + 8..pos + 16];
                self.sr_samples.entry(ssrc).or_default().push(now_wall - ntp_unix_ms(ntp));
            }
            pos += size;
        }
    }

    pub fn sr_wall_gap_ms(&self) -> Option<f64> {
        let mut gaps: Vec<f64> = self.video_ssrcs.iter()
            .filter_map(|ssrc| self.sr_samples.get(ssrc))
            .flat_map(|samples| samples.iter().rev().take(21).copied())
            .collect();
        if gaps.is_empty() { return None; }
        gaps.sort_by(|a, b| a.partial_cmp(b).unwrap());
        Some(gaps[gaps.len() / 2])
    }

    pub fn latest_sr_gap_ms(&self) -> Option<f64> {
        self.video_ssrcs.iter()
            .filter_map(|ssrc| self.sr_samples.get(ssrc))
            .filter_map(|samples| samples.last().copied())
            .next()
    }
}

/// get-stats based transport report (selected pair, candidate type, RTT).
pub fn request_transport_stats(webrtc: &gst::Element, transport: &Arc<Mutex<Transport>>) {
    let transport = transport.clone();
    let promise = gst::Promise::with_change_func(move |reply| {
        let Ok(Some(reply)) = reply else { return };
        // get-stats report fields are keyed by candidate/pair id; the inner
        // structures do not carry an "id" field (Python parse_transport).
        let records: Vec<(String, gst::Structure)> = reply.iter()
            .filter_map(|(name, value)| value.get::<gst::Structure>().ok().map(|s| (name.to_string(), s)))
            .collect();
        let selected_pair_id = records.iter()
            .find_map(|(_, s)| s.get::<String>("selected-candidate-pair-id").ok());
        let pair = selected_pair_id.as_deref()
            .and_then(|id| records.iter().find(|(name, _)| name == id).map(|(_, s)| s));
        let rtt = pair.and_then(|p| p.get::<f64>("current-round-trip-time").ok()).map(|v| v * 1000.0);
        let remote_id = pair.and_then(|p| p.get::<String>("remote-candidate-id").ok());
        let remote = remote_id.as_deref()
            .and_then(|id| records.iter().find(|(name, _)| name == id).map(|(_, s)| s));
        let candidate_type = remote.and_then(|r| r.get::<String>("candidate-type").ok());
        let address = remote.and_then(|r| r.get::<String>("address").ok());
        let port = remote.and_then(|r| r.get::<u32>("port").ok());
        let tr = transport.lock().unwrap();
        protocol::event("transport-stats", json!({
            "selectedPair": pair.is_some(),
            "candidateType": candidate_type,
            "rttMs": rtt,
            "videoRtpPackets": tr.capture_packets,
            "captureTimestamps": tr.capture_timestamps,
            "captureClockOffset": tr.capture_clock_offset,
            "captureClockOffsetMaxMs": tr.capture_clock_offset_max_ms,
            "videoSenderReports": tr.video_ssrcs.iter()
                .map(|ssrc| tr.sr_samples.get(ssrc).map(|s| s.len()).unwrap_or(0)).sum::<usize>(),
            "latestSenderReportGapMs": tr.latest_sr_gap_ms(),
            "address": address,
            "port": port,
        }));
    });
    webrtc.emit_by_name::<()>("get-stats", &[&None::<gst::Pad>, &promise]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn munge_inserts_bitrate_and_extmap() {
        let offer = "v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=mid:audio0\r\nm=video 0 UDP/TLS/RTP/SAVPF 103\r\na=mid:video1\r\na=extmap:3 urn:ietf:params:rtp-hdrext:sdes:mid\r\n";
        let (out, ext) = munge_offer(offer, 20000, true);
        assert!(out.contains("b=AS:20000\r\n"));
        assert_eq!(ext, Some(14));
        assert!(out.contains("a=extmap:14/recvonly "));
        assert!(out.contains(ABS_CAPTURE_TIME_URI));
    }

    #[test]
    fn munge_without_capture_has_no_extmap() {
        let offer = "v=0\r\nm=video 0 UDP/TLS/RTP/SAVPF 103\r\na=mid:video1\r\n";
        let (out, ext) = munge_offer(offer, 1000, false);
        assert_eq!(ext, None);
        assert!(!out.contains("extmap:14"));
    }

    #[test]
    fn answer_inspection_finds_extensions() {
        let answer = "v=0\r\nm=video 0 UDP/TLS/RTP/SAVPF 103\r\na=extmap:14/recvonly http://www.webrtc.org/experiments/rtp-hdrext/abs-capture-time\r\na=extmap:5 http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=extmap:14 http://www.webrtc.org/experiments/rtp-hdrext/abs-capture-time\r\n";
        let (capture, send) = inspect_answer(answer);
        assert_eq!(capture, Some(14));
        assert!(send);
    }

    #[test]
    fn ntp_conversion_known_value() {
        // 2020-01-01T00:00:00Z = NTP 3786825600
        let ntp = 3_786_825_600u32.to_be_bytes();
        let mut bytes = [0u8; 8];
        bytes[..4].copy_from_slice(&ntp);
        let ms = ntp_unix_ms(&bytes);
        assert!((ms - 1_577_836_800_000.0).abs() < 1.0, "{ms}");
    }

    #[test]
    fn capture_extension_one_byte_header() {
        let mut packet = vec![0u8; 64];
        packet[0] = 0x90; // version 2, extension bit
        packet[12] = 0xBE; packet[13] = 0xDE;
        packet[14] = 0; packet[15] = 3; // 3 words
        packet[16] = (7 << 4) | 7; // id 7, size 8
        for i in 0..8 { packet[17 + i] = i as u8; }
        let value = read_capture_extension(&packet, 7).expect("extension found");
        assert_eq!(value.len(), 8);
        assert_eq!(value[0], 0);
    }

    #[test]
    fn rtcp_sender_report_gap() {
        let mut tr = Transport::default();
        tr.video_ssrcs.push(42);
        let mut sr = vec![0u8; 28];
        sr[0] = 0x80; sr[1] = 200; sr[2] = 0; sr[3] = 6;
        sr[4..8].copy_from_slice(&42u32.to_be_bytes());
        let ntp = 3_786_825_600u32.to_be_bytes();
        sr[8..12].copy_from_slice(&ntp);
        tr.observe_rtcp(&sr, 1_577_836_800_500.0);
        assert!((tr.sr_wall_gap_ms().unwrap() - 500.0).abs() < 1.0);
    }
}