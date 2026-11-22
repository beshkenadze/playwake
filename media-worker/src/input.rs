//! 8 ms input packet loop (38-byte Playwire packet) with evdev sampling,
//! the route state machine, pulse overrides and external gamepad state.
//! Port of media/gstreamer_player.py:859-921 + StreamInput wiring.

use crate::{clock, protocol, route::{Route, RouteEvent, GUIDE}};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_webrtc as gst_webrtc;
use serde_json::json;
use std::collections::HashMap;

const BUTTONS: [(u16, u16); 10] = [
    (304, 16), (305, 32), (307, 64), (308, 128), (310, 4096),
    (311, 8192), (314, 8), (315, 4), (317, 16384), (318, 32768),
];
const AXES: [u16; 4] = [0, 1, 3, 4];

pub struct Input {
    pub sequence: u32,
    pub start_time: f64,
    pub external: Option<([i16; 4], u16, u16, u16)>,
    pub override_: Option<(f64, u16, f64)>,
    pub trial_actuated_at: Option<f64>,
    pub route: Route,
    pub device: Option<evdev::Device>,
    held: HashMap<(u16, u16), i32>,
    axis_info: HashMap<u16, evdev::AbsInfo>,
    audit: bool,
    last_audit: Option<(u16, bool, bool)>,
    pub last_trial_timing: Option<serde_json::Value>,
}

impl Input {
    pub fn new(start_time: f64) -> Self {
        Self {
            sequence: 0,
            start_time,
            external: None,
            override_: None,
            trial_actuated_at: None,
            route: Route::default(),
            device: None,
            held: HashMap::new(),
            axis_info: HashMap::new(),
            audit: std::env::var("ALLY_INPUT_AUDIT").as_deref() == Ok("1"),
            last_audit: None,
            last_trial_timing: None,
        }
    }

    pub fn connect_gamepad(&mut self) {
        let preferred = std::env::var("GREENLIGHT_GAMEPAD").ok();
        let candidates: Vec<std::path::PathBuf> = match &preferred {
            Some(value) => vec![std::path::PathBuf::from(value)],
            None => evdev::enumerate().map(|(path, _)| path).collect(),
        };
        for path in candidates {
            let Ok(device) = evdev::Device::open(&path) else { continue };
            let keys = device.supported_keys();
            let abs = device.supported_absolute_axes();
            let has_key = keys.is_some_and(|k| k.contains(evdev::KeyCode::BTN_SOUTH));
            let has_abs = abs.is_some_and(|a| a.contains(evdev::AbsoluteAxisCode::ABS_X));
            if !has_key || !has_abs { continue; }
            if device.set_nonblocking(true).is_err() { continue; }
            self.axis_info.clear();
            if let Ok(infos) = device.get_absinfo() {
                for (code, info) in infos {
                    self.axis_info.insert(code.0, info);
                }
            }
            self.held.clear();
            self.device = Some(device);
            return;
        }
    }

    pub fn set_gamepad(&mut self, axes: &[i16], buttons: u16, lt: u16, rt: u16) {
        let mut a = [0i16; 4];
        for (slot, value) in a.iter_mut().zip(axes.iter()) {
            *slot = (*value).clamp(-32767, 32767);
        }
        self.external = Some((a, buttons & 0xffff, lt.min(65535), rt.min(65535)));
    }

    pub fn pulse(&mut self, axis: f64, buttons: u16, duration: u64, trial: bool) {
        let now = clock::now_ms();
        self.override_ = Some((axis, buttons, now + duration as f64));
        if trial { self.trial_actuated_at = Some(now); }
    }

    fn sample(&mut self) -> ([i16; 4], u16, u16, u16) {
        let mut fetched: Vec<evdev::InputEvent> = Vec::new();
        let mut lost = false;
        if let Some(device) = self.device.as_mut() {
            match device.fetch_events() {
                Ok(events) => fetched.extend(events),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => lost = true,
            }
        }
        if lost {
            self.device = None;
            self.held.clear();
            self.axis_info.clear();
        }
        for event in fetched {
            let kind = match event.event_type() {
                evdev::EventType::KEY => 1,
                evdev::EventType::ABSOLUTE => 3,
                _ => continue,
            };
            self.held.insert((kind, event.code()), event.value());
        }
        let value = |code: u16| -> i32 {
            let Some(info) = self.axis_info.get(&code) else { return 0 };
            if info.maximum() <= info.minimum() { return 0; }
            let raw = self.held.get(&(3, code)).copied().unwrap_or(info.value());
            let scaled = (2.0 * (raw - info.minimum()) as f64
                / (info.maximum() - info.minimum()) as f64 - 1.0) * 32767.0;
            scaled.round().clamp(-32767.0, 32767.0) as i32
        };
        let mut axes = [0i16; 4];
        for (slot, code) in axes.iter_mut().zip(AXES) {
            *slot = value(code) as i16;
        }
        let mut buttons: u16 = BUTTONS.iter()
            .filter(|(code, _)| self.held.get(&(1, *code)).copied().unwrap_or(0) != 0)
            .map(|(_, mask)| mask)
            .sum();
        match self.held.get(&(3, 16)).copied().unwrap_or(0) {
            -1 => buttons |= 1024,
            1 => buttons |= 2048,
            _ => {}
        }
        match self.held.get(&(3, 17)).copied().unwrap_or(0) {
            -1 => buttons |= 256,
            1 => buttons |= 512,
            _ => {}
        }
        let lt = if self.axis_info.contains_key(&2) { (value(2) + 32767) as u16 } else { 0 };
        let rt = if self.axis_info.contains_key(&5) { (value(5) + 32767) as u16 } else { 0 };
        (axes, buttons, lt, rt)
    }

    /// One 8 ms tick. Returns (continue_timer, events_to_emit).
    pub fn tick(&mut self, channel: &gst_webrtc::WebRTCDataChannel, now: f64) -> (bool, Vec<RouteEvent>) {
        if channel.property::<gst_webrtc::WebRTCDataChannelState>("ready-state")
            != gst_webrtc::WebRTCDataChannelState::Open
        {
            return (false, Vec::new());
        }
        let (mut axes, mut buttons, lt, rt) = match self.external {
            Some(sample) => sample,
            None => self.sample(),
        };
        if let Some((axis, virtual_buttons, expires)) = self.override_ {
            if now < expires {
                axes[2] = (axis.clamp(-1.0, 1.0) * 32767.0).round() as i16;
                buttons = virtual_buttons;
            } else {
                self.override_ = None;
            }
        }
        let (routed, events) = self.route.route(axes, buttons, lt, rt, now);
        if self.audit {
            let sample = (routed.buttons, routed.axes.iter().any(|a| *a != 0), lt != 0 || rt != 0);
            if self.last_audit != Some(sample) {
                self.last_audit = Some(sample);
                protocol::event("stream-input-audit", json!({
                    "buttons": sample.0, "axisActive": sample.1, "triggerActive": sample.2,
                }));
            }
        }
        self.sequence = self.sequence.wrapping_add(1);
        let packet = input_packet(self.sequence, now - self.start_time, routed.buttons, routed.axes, routed.lt, routed.rt);
        let trial = self.trial_actuated_at.take_if(|_| routed.axes[2] != 0);
        let before = if trial.is_some() { channel.property::<u64>("buffered-amount") } else { 0 };
        channel.emit_by_name::<()>("send-data", &[&gst::glib::Bytes::from(&packet)]);
        if let Some(actuated_at) = trial {
            let after = channel.property::<u64>("buffered-amount");
            self.last_trial_timing = Some(json!({
                "actuatedAt": actuated_at,
                "polledAt": now,
                "sentAt": clock::now_ms(),
                "queuedBeforeBytes": before,
                "queuedAfterBytes": after,
            }));
            protocol::event("input-timing", json!({
                "actuatedAt": actuated_at,
                "polledAt": now,
                "sentAt": clock::now_ms(),
                "queuedBeforeBytes": before,
                "queuedAfterBytes": after,
            }));
        }
        (true, events)
    }
}

pub fn emit_route_events(events: Vec<RouteEvent>) {
    for event in events {
        match event {
            RouteEvent::OverlayKey(key) => protocol::event("stream-overlay-key", json!({ "key": key })),
            RouteEvent::Overlay(open) => protocol::event("stream-overlay", json!({ "open": open })),
            RouteEvent::GuidePulse(pressed) => protocol::event("xbox-guide-pulse", json!({ "pressed": pressed })),
            RouteEvent::Rearmed => protocol::event("stream-input-rearmed", json!({})),
            RouteEvent::Focus(active) => protocol::event("stream-input-focus", json!({ "active": active })),
        }
    }
}

/// struct.pack('<HIdBBHhhhhHHII', 2, seq, time, 1, 0, buttons, lx, -ly, rx, -ry, lt, rt, 0, 0)
pub fn input_packet(sequence: u32, time: f64, buttons: u16, axes: [i16; 4], lt: u16, rt: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(38);
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&sequence.to_le_bytes());
    out.extend_from_slice(&time.to_le_bytes());
    out.push(1);
    out.push(0);
    out.extend_from_slice(&buttons.to_le_bytes());
    out.extend_from_slice(&axes[0].to_le_bytes());
    out.extend_from_slice(&(-axes[1]).to_le_bytes());
    out.extend_from_slice(&axes[2].to_le_bytes());
    out.extend_from_slice(&(-axes[3]).to_le_bytes());
    out.extend_from_slice(&lt.to_le_bytes());
    out.extend_from_slice(&rt.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packet_layout_matches_python_struct() {
        let bytes = input_packet(7, 12.5, 16, [100, -200, 300, -400], 1000, 2000);
        assert_eq!(bytes.len(), 38);
        assert_eq!(&bytes[0..2], &2u16.to_le_bytes());
        assert_eq!(&bytes[2..6], &7u32.to_le_bytes());
        assert_eq!(&bytes[6..14], &12.5f64.to_le_bytes());
        assert_eq!(&bytes[14..16], &[1, 0]);
        assert_eq!(&bytes[16..18], &16u16.to_le_bytes());
        assert_eq!(&bytes[18..20], &100i16.to_le_bytes());
        assert_eq!(&bytes[20..22], &200i16.to_le_bytes());
        assert_eq!(&bytes[22..24], &300i16.to_le_bytes());
        assert_eq!(&bytes[24..26], &400i16.to_le_bytes());
        assert_eq!(&bytes[26..28], &1000u16.to_le_bytes());
        assert_eq!(&bytes[28..30], &2000u16.to_le_bytes());
        assert_eq!(&bytes[30..38], &[0; 8]);
    }

    #[test]
    fn guide_constant_matches_protocol() {
        assert_eq!(GUIDE, 2);
    }
}