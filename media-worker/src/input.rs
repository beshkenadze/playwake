//! Input packets (38-byte Playwire packet): sent at once on every gamepad
//! change and on an 8 ms tick otherwise, with evdev sampling, the route state
//! machine, pulse overrides and external gamepad state.
//! Port of media/gstreamer_player.py:859-921 + StreamInput wiring.

use crate::{clock, protocol, route::{Route, Routed, RouteEvent, GUIDE}};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_webrtc as gst_webrtc;
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, Weak};

type ChannelState = gst_webrtc::WebRTCDataChannelState;

const BUTTONS: [(u16, u16); 10] = [
    (304, 16), (305, 32), (307, 64), (308, 128), (310, 4096),
    (311, 8192), (314, 8), (315, 4), (317, 16384), (318, 32768),
];
const AXES: [u16; 4] = [0, 1, 3, 4];
/// Packets stay at least this far apart, so a fast pad cannot flood the channel.
const MIN_GAP_MS: f64 = 1.0;

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
    last_sent: f64,
    /// Set by the handshake ack; no packet goes out before it.
    pub armed: bool,
    /// Replaced when the device is lost, so a watch on its fd can tell.
    device_alive: Arc<()>,
}

/// Where input packets go: the Xbox data channel or a PC stream.
pub trait Link {
    fn state(&self) -> ChannelState;
    fn send(&self, sequence: u32, time: f64, state: &Routed);
    /// Bytes queued and not yet sent, for the latency trial.
    fn buffered(&self) -> u64 { 0 }
}

impl Link for gst_webrtc::WebRTCDataChannel {
    fn state(&self) -> ChannelState { self.property("ready-state") }

    fn send(&self, sequence: u32, time: f64, state: &Routed) {
        let packet = input_packet(sequence, time, state.buttons, state.axes, state.lt, state.rt);
        self.emit_by_name::<()>("send-data", &[&gst::glib::Bytes::from(&packet)]);
    }

    fn buffered(&self) -> u64 { self.property("buffered-amount") }
}

impl Link for gamestream::Stream {
    fn state(&self) -> ChannelState {
        if self.ended() { ChannelState::Closed } else { ChannelState::Open }
    }

    fn send(&self, _sequence: u32, _time: f64, state: &Routed) {
        self.gamepad(&pc_gamepad(state));
    }
}

/// The routed Xbox state in the PC protocol's buttons, ranges and stick signs.
pub fn pc_gamepad(state: &Routed) -> gamestream::Gamepad {
    use gamestream::*;
    const BUTTONS: [(u16, u32); 15] = [
        (2, BUTTON_GUIDE), (4, BUTTON_START), (8, BUTTON_BACK), (16, BUTTON_A), (32, BUTTON_B),
        (64, BUTTON_X), (128, BUTTON_Y), (256, BUTTON_UP), (512, BUTTON_DOWN), (1024, BUTTON_LEFT),
        (2048, BUTTON_RIGHT), (4096, BUTTON_LEFT_BUMPER), (8192, BUTTON_RIGHT_BUMPER),
        (16384, BUTTON_LEFT_STICK), (32768, BUTTON_RIGHT_STICK),
    ];
    let [left_x, left_y, right_x, right_y] = state.axes;
    Gamepad {
        buttons: BUTTONS.iter().filter(|(xbox, _)| state.buttons & xbox != 0).map(|(_, pc)| pc).sum(),
        left_trigger: (state.lt >> 8) as u8,
        right_trigger: (state.rt >> 8) as u8,
        // evdev sticks point down for positive Y; the host expects up.
        left_stick: (left_x, left_y.saturating_neg()),
        right_stick: (right_x, right_y.saturating_neg()),
    }
}

/// What a send attempt did.
pub enum Tick {
    Sent(Vec<RouteEvent>),
    /// Too close to the last packet: the caller retries after `MIN_GAP_MS`.
    Deferred,
    Opening,
    Closed,
}

#[derive(Debug, PartialEq)]
enum Gate { Send, Deferred, Opening, Closed }

fn gate(armed: bool, state: ChannelState, now: f64, last_sent: f64) -> Gate {
    // Also ends the timers of a worker that a new, unarmed one replaced.
    if !armed { return Gate::Closed; }
    match state {
        ChannelState::Open if now - last_sent >= MIN_GAP_MS => Gate::Send,
        ChannelState::Open => Gate::Deferred,
        ChannelState::Connecting => Gate::Opening,
        _ => Gate::Closed,
    }
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
            last_sent: f64::NEG_INFINITY,
            armed: false,
            device_alive: Arc::new(()),
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

    /// The gamepad fd to watch for changes. The token dies when that device
    /// is lost or this input is dropped, which closes the fd.
    pub fn device_watch(&self) -> Option<(std::os::fd::RawFd, Weak<()>)> {
        use std::os::fd::AsRawFd;
        let device = self.device.as_ref()?;
        Some((device.as_raw_fd(), Arc::downgrade(&self.device_alive)))
    }

    fn lose_device(&mut self) {
        self.device = None;
        self.device_alive = Arc::new(());
        self.held.clear();
        self.axis_info.clear();
    }

    /// Reads the pending evdev events into `held`.
    fn drain(&mut self) {
        let mut fetched: Vec<evdev::InputEvent> = Vec::new();
        let mut lost = false;
        if let Some(device) = self.device.as_mut() {
            match device.fetch_events() {
                Ok(events) => fetched.extend(events),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => lost = true,
            }
        }
        if lost { self.lose_device(); }
        for event in fetched {
            let kind = match event.event_type() {
                evdev::EventType::KEY => 1,
                evdev::EventType::ABSOLUTE => 3,
                _ => continue,
            };
            self.held.insert((kind, event.code()), event.value());
        }
    }

    fn sample(&self) -> ([i16; 4], u16, u16, u16) {
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

    /// Sends the pad state. Runs on the 8 ms tick and at once on every gamepad
    /// change, so input does not wait for the tick.
    pub fn tick(&mut self, channel: &dyn Link, now: f64) -> Tick {
        let gate = gate(self.armed, channel.state(), now, self.last_sent);
        if gate == Gate::Closed { return Tick::Closed; }
        // The watched fd is level-triggered, so read it even when not sending.
        self.drain();
        match gate {
            Gate::Deferred => return Tick::Deferred,
            Gate::Opening => return Tick::Opening,
            Gate::Send | Gate::Closed => {}
        }
        self.last_sent = now;
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
        let trial = self.trial_actuated_at.take_if(|_| routed.axes[2] != 0);
        let before = if trial.is_some() { channel.buffered() } else { 0 };
        channel.send(self.sequence, now - self.start_time, &routed);
        if let Some(actuated_at) = trial {
            let after = channel.buffered();
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
        Tick::Sent(events)
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
    fn sends_stay_a_millisecond_apart_and_ask_for_a_retry() {
        assert_eq!(gate(true, ChannelState::Open, 0.0, Input::new(0.0).last_sent), Gate::Send);
        assert_eq!(gate(true, ChannelState::Open, 100.9, 100.0), Gate::Deferred);
        assert_eq!(gate(true, ChannelState::Open, 101.0, 100.0), Gate::Send);
    }

    #[test]
    fn an_opening_channel_keeps_the_input_sources_alive() {
        assert_eq!(gate(true, ChannelState::Connecting, 0.0, f64::NEG_INFINITY), Gate::Opening);
        assert_eq!(gate(true, ChannelState::Closing, 0.0, f64::NEG_INFINITY), Gate::Closed);
        assert_eq!(gate(true, ChannelState::Closed, 0.0, f64::NEG_INFINITY), Gate::Closed);
    }

    #[test]
    fn an_unarmed_input_sends_nothing_and_ends_its_callers() {
        assert!(!Input::new(0.0).armed);
        assert_eq!(gate(false, ChannelState::Open, 0.0, f64::NEG_INFINITY), Gate::Closed);
    }

    #[test]
    fn the_device_token_dies_with_the_device_and_with_the_input() {
        let mut input = Input::new(0.0);
        let lost = Arc::downgrade(&input.device_alive);
        input.lose_device();
        assert_eq!(lost.strong_count(), 0);
        let dropped = Arc::downgrade(&input.device_alive);
        drop(input);
        assert_eq!(dropped.strong_count(), 0);
    }

    #[test]
    fn maps_the_xbox_state_to_the_pc_gamepad() {
        let state = Routed { axes: [100, -200, i16::MIN, i16::MAX], buttons: 16 | 8 | 256 | 2 | 32768, lt: 65534, rt: 300 };
        let pad = pc_gamepad(&state);
        assert_eq!(pad.buttons, gamestream::BUTTON_A | gamestream::BUTTON_BACK | gamestream::BUTTON_UP
            | gamestream::BUTTON_GUIDE | gamestream::BUTTON_RIGHT_STICK);
        assert_eq!((pad.left_trigger, pad.right_trigger), (255, 1));
        assert_eq!(pad.left_stick, (100, 200));
        assert_eq!(pad.right_stick, (i16::MIN, -i16::MAX));
        // Pushing a stick fully down must not wrap around to up.
        let down = Routed { axes: [0, i16::MIN, 0, 0], ..Routed { axes: [0; 4], buttons: 0, lt: 0, rt: 0 } };
        assert_eq!(pc_gamepad(&down).left_stick, (0, i16::MAX));
    }

    #[test]
    fn guide_constant_matches_protocol() {
        assert_eq!(GUIDE, 2);
    }
}