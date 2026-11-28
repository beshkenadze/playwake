//! Input packets (38-byte Playwire packet): sent at once on every gamepad
//! change and on an 8 ms tick otherwise, with evdev sampling, the route state
//! machine, pulse overrides and external gamepad state.
//! Port of media/gstreamer_player.py:859-921 + StreamInput wiring.

use crate::{clock, protocol, route::{Route, Routed, RouteEvent, GUIDE}, x11::{self, PointerEvent}};
use playwake_core::input::{Device, InputEvent, MouseButton, PcInput, Sink, Usage};
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
        self.gamepad(&playwake_core::pc::gamepad(state));
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

/// How a PC stream takes the mouse.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MouseMode {
    /// How far the mouse moved, with the cursor hidden and held in the stream.
    #[default]
    Game,
    /// Where the cursor is over the picture.
    Desktop,
}

/// The mouse of a PC stream: pointer events of the video window as core
/// input, sent only while the stream has the mouse.
#[derive(Default)]
pub struct Mouse {
    pub mode: MouseMode,
    active: bool,
    pc: PcInput,
    /// When an absolute device last put the pointer somewhere.
    jumped_at: Option<u32>,
    /// X buttons a tap on the touchscreen holds, which game mode keeps local.
    tapped: std::collections::BTreeSet<u32>,
}

/// How soon after an absolute jump a press counts as the tap that made it:
/// Gamescope moves the pointer to the finger and presses at once.
const TAP_MS: u32 = 250;

impl Mouse {
    /// Sends `events` from a `window`-sized video window showing a
    /// `picture`-sized video, and starts or stops taking the mouse. Events
    /// read as the mouse is taken or let go stay local; letting go releases
    /// every held button.
    pub fn update(&mut self, sink: &impl Sink, active: bool, events: impl IntoIterator<Item = PointerEvent>,
        window: (u32, u32), picture: (u32, u32))
    {
        if self.active && active {
            for event in events { self.send(sink, event, window, picture); }
        } else if self.active {
            self.pc.release_all(sink);
            self.forget_taps();
        }
        self.active = active;
    }

    /// Switches between the game and desktop modes, letting go of held buttons.
    pub fn toggle_mode(&mut self, sink: &impl Sink) {
        self.pc.release_all(sink);
        self.forget_taps();
        self.mode = match self.mode { MouseMode::Game => MouseMode::Desktop, MouseMode::Desktop => MouseMode::Game };
    }

    fn send(&mut self, sink: &impl Sink, event: PointerEvent, window: (u32, u32), picture: (u32, u32)) {
        let event = match (event, self.mode) {
            (PointerEvent::Moved { dx, dy }, MouseMode::Game) => {
                // The mouse moved: a press now is its own, not a tap's.
                self.jumped_at = None;
                Some(InputEvent::MouseMove { dx: dx as f32, dy: dy as f32 })
            }
            (PointerEvent::At { x, y }, MouseMode::Desktop) => over_picture((x, y), window, picture)
                .map(|(x, y)| InputEvent::MousePosition { x, y }),
            (PointerEvent::Jumped { time }, _) => {
                self.jumped_at = Some(time);
                None
            }
            // A tap clicks where the finger is, which game mode does not track.
            (PointerEvent::Button { button, down, time }, MouseMode::Game) if self.tap(button, down, time) => None,
            (PointerEvent::Button { button, down, .. }, _) => button_event(button, down),
            _ => None,
        };
        if let Some(event) = event { self.pc.send(sink, Device::Mouse, event); }
    }
}

impl Mouse {
    /// Whether a button change belongs to a tap on the touchscreen.
    fn tap(&mut self, button: u32, down: bool, time: u32) -> bool {
        if !down { return self.tapped.remove(&button); }
        // One jump makes one tap.
        let tap = self.jumped_at.take().is_some_and(|at| time.wrapping_sub(at) <= TAP_MS);
        if tap { self.tapped.insert(button); } else { self.tapped.remove(&button); }
        tap
    }

    /// Drops the taps in progress: their releases may come in another mode
    /// or not at all, and must not swallow a later click's release.
    fn forget_taps(&mut self) {
        self.jumped_at = None;
        self.tapped.clear();
    }
}

/// The keyboard of a PC stream. A keyboard's key goes to the PC as that key,
/// held until it is let go, while the stream has the keys; what Steam's
/// on-screen keyboard types goes as text, as do keys pressed while it is up.
#[derive(Default)]
pub struct Keys {
    /// ALLY_MOUSE_DEBUG: log every key.
    debug: bool,
    active: bool,
    pc: PcInput,
    /// X keycodes sent down as keys and not let go yet.
    held: std::collections::BTreeSet<u32>,
}

impl Keys {
    pub fn new(debug: bool) -> Self { Self { debug, ..Self::default() } }

    /// Sends `keys`, as keys while `active`: the stream has the keys and
    /// Steam's on-screen keyboard is down. Becoming inactive lets go of every
    /// key held first.
    pub fn send(&mut self, sink: &impl Sink, keys: &[x11::Key], active: bool) {
        if self.active && !active {
            self.held.clear();
            self.pc.release_all(sink);
        }
        self.active = active;
        for key in keys {
            if self.debug { eprintln!("pw-key {key:?} own {:?} active {}", x11::own_key(key), self.active); }
            let usage = |code| InputEvent::Key { usage: Usage::Evdev(code), down: key.down };
            if !key.down {
                if self.held.remove(&key.keycode) {
                    self.pc.send(sink, Device::Keyboard, usage((key.keycode - 8) as u16));
                }
            } else if let Some(code) = x11::own_key(key).filter(|_| self.active) {
                self.held.insert(key.keycode);
                self.pc.send(sink, Device::Keyboard, usage(code));
            } else if let Some(code) = x11::virtual_key(key.keysym) {
                sink.key(code, true, 0);
                sink.key(code, false, 0);
            } else if !key.text.is_empty() {
                self.pc.send(sink, Device::Keyboard, InputEvent::Text(key.text.clone()));
            }
        }
    }
}

/// The core event of an X button; the wheel scrolls one notch on a press.
fn button_event(button: u32, down: bool) -> Option<InputEvent> {
    let press = |button| Some(InputEvent::MouseButton { button, down });
    let scroll = |dx, dy| down.then_some(InputEvent::Scroll { dx, dy });
    match button {
        1 => press(MouseButton::Left),
        2 => press(MouseButton::Middle),
        3 => press(MouseButton::Right),
        8 => press(MouseButton::X1),
        9 => press(MouseButton::X2),
        4 => scroll(0.0, 1.0),
        5 => scroll(0.0, -1.0),
        6 => scroll(-1.0, 0.0),
        7 => scroll(1.0, 0.0),
        _ => None,
    }
}

/// Where a point of the window is on the picture, 0…1 from its top left
/// corner: the picture is as large as fits and centred, as it is shown.
fn over_picture((x, y): (f64, f64), window: (u32, u32), picture: (u32, u32)) -> Option<(f32, f32)> {
    if picture.0 == 0 || picture.1 == 0 || window.0 == 0 || window.1 == 0 { return None; }
    let (left, top, width, height) = crate::present::fit(picture, window);
    Some(((x as f32 - left) / width, (y as f32 - top) / height))
}

pub fn emit_route_events(events: Vec<RouteEvent>) {
    for event in events {
        match event {
            RouteEvent::OverlayKey(key) => protocol::event("stream-overlay-key", json!({ "key": key })),
            RouteEvent::Overlay(open) => protocol::event("stream-overlay", json!({ "open": open })),
            RouteEvent::GuidePulse(pressed) => protocol::event("xbox-guide-pulse", json!({ "pressed": pressed })),
            RouteEvent::Rearmed => protocol::event("stream-input-rearmed", json!({})),
            RouteEvent::Focus(active) => protocol::event("stream-input-focus", json!({ "active": active })),
            RouteEvent::Quit => protocol::event("stream-quit", json!({})),
            RouteEvent::ToggleStats => protocol::event("stream-stats", json!({})),
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

    #[derive(Default)]
    struct Recorder(std::cell::RefCell<Vec<String>>);

    impl Recorder {
        fn take(&self) -> Vec<String> { std::mem::take(&mut self.0.borrow_mut()) }
        fn push(&self, call: String) { self.0.borrow_mut().push(call) }
    }

    impl Sink for Recorder {
        fn key(&self, key: u8, down: bool, modifiers: u8) { self.push(format!("key {key:#x} {down} {modifiers}")) }
        fn text(&self, text: &str) { self.push(format!("text {text}")) }
        fn mouse_move(&self, dx: i16, dy: i16) { self.push(format!("move {dx} {dy}")) }
        fn mouse_position(&self, x: i16, y: i16, _: i16, _: i16) { self.push(format!("at {x} {y}")) }
        fn mouse_button(&self, button: MouseButton, down: bool) { self.push(format!("{button:?} {down}")) }
        fn scroll(&self, amount: i16) { self.push(format!("scroll {amount}")) }
        fn hscroll(&self, amount: i16) { self.push(format!("hscroll {amount}")) }
        fn touch(&self, _: playwake_core::input::Touch) -> bool { false }
        fn host_supports_touch(&self) -> bool { false }
    }

    const SCREEN: (u32, u32) = (1280, 800);

    fn active_mouse(sink: &Recorder) -> Mouse {
        let mut mouse = Mouse::default();
        mouse.update(sink, true, [], SCREEN, SCREEN);
        mouse
    }

    #[test]
    fn game_mode_sends_raw_motion_and_ignores_positions() {
        let sink = Recorder::default();
        let mut mouse = active_mouse(&sink);
        mouse.update(&sink, true, [PointerEvent::Moved { dx: 2.5, dy: -1.0 }], SCREEN, SCREEN);
        mouse.update(&sink, true, [PointerEvent::At { x: 10.0, y: 10.0 }], SCREEN, SCREEN);
        mouse.update(&sink, true, [PointerEvent::Moved { dx: 0.5, dy: 0.0 }], SCREEN, SCREEN);
        assert_eq!(sink.take(), ["move 2 -1", "move 1 0"]);
    }

    #[test]
    fn desktop_mode_sends_positions_over_the_letterboxed_picture() {
        let sink = Recorder::default();
        let mut mouse = active_mouse(&sink);
        mouse.toggle_mode(&sink);
        assert_eq!(mouse.mode, MouseMode::Desktop);
        // 1920x1080 on a 1280x800 window: bars of 40 pixels above and below.
        let picture = (1920, 1080);
        mouse.update(&sink, true, [PointerEvent::Moved { dx: 5.0, dy: 5.0 }], SCREEN, picture);
        mouse.update(&sink, true, [PointerEvent::At { x: 640.0, y: 400.0 }], SCREEN, picture);
        mouse.update(&sink, true, [PointerEvent::At { x: 0.0, y: 40.0 }], SCREEN, picture);
        mouse.update(&sink, true, [PointerEvent::At { x: 1280.0, y: 760.0 }], SCREEN, picture);
        // On a bar the position stays at the picture's edge.
        mouse.update(&sink, true, [PointerEvent::At { x: 1280.0, y: 5.0 }], SCREEN, picture);
        assert_eq!(sink.take(), ["at 16384 16384", "at 0 0", "at 32767 32767", "at 32767 0"]);
    }

    #[test]
    fn a_window_shrunk_above_the_keyboard_maps_to_the_whole_picture() {
        let sink = Recorder::default();
        let mut mouse = active_mouse(&sink);
        mouse.toggle_mode(&sink);
        mouse.update(&sink, true, [PointerEvent::At { x: 320.0, y: 400.0 }], (640, 400), SCREEN);
        assert_eq!(sink.take(), ["at 16384 32767"]);
    }

    #[test]
    fn buttons_and_wheel_map_to_the_pc_mouse() {
        let sink = Recorder::default();
        let mut mouse = active_mouse(&sink);
        for button in 1..=9 {
            mouse.update(&sink, true, [PointerEvent::Button { button, down: true, time: 0 }], SCREEN, SCREEN);
            mouse.update(&sink, true, [PointerEvent::Button { button, down: false, time: 0 }], SCREEN, SCREEN);
        }
        assert_eq!(sink.take(), [
            "Left true", "Left false", "Middle true", "Middle false", "Right true", "Right false",
            "scroll 120", "scroll -120", "hscroll -120", "hscroll 120",
            "X1 true", "X1 false", "X2 true", "X2 false",
        ]);
    }

    #[test]
    fn losing_the_mouse_lets_go_of_held_buttons_and_drops_later_events() {
        let sink = Recorder::default();
        let mut mouse = active_mouse(&sink);
        mouse.update(&sink, true, [PointerEvent::Button { button: 1, down: true, time: 0 }], SCREEN, SCREEN);
        mouse.update(&sink, true, [PointerEvent::Button { button: 3, down: true, time: 0 }], SCREEN, SCREEN);
        sink.take();
        // A click read as the menu opens stays local.
        mouse.update(&sink, false, [PointerEvent::Button { button: 2, down: true, time: 0 }], SCREEN, SCREEN);
        assert_eq!(sink.take(), ["Left false", "Right false"]);
        mouse.update(&sink, false, [PointerEvent::Moved { dx: 4.0, dy: 4.0 }], SCREEN, SCREEN);
        // Motion made while the menu was open stays local as the mouse is taken again.
        mouse.update(&sink, true, [PointerEvent::Moved { dx: 4.0, dy: 4.0 }], SCREEN, SCREEN);
        mouse.update(&sink, false, [], SCREEN, SCREEN);
        assert_eq!(sink.take(), Vec::<String>::new());
    }

    #[test]
    fn a_tap_on_the_screen_does_not_click_in_game_mode() {
        let sink = Recorder::default();
        let mut mouse = active_mouse(&sink);
        let left = |down, time| PointerEvent::Button { button: 1, down, time };
        // Gamescope puts the pointer at the finger, then presses and releases.
        mouse.update(&sink, true, [PointerEvent::Jumped { time: 1000 }, PointerEvent::At { x: 639.0, y: 319.0 },
            left(true, 1000), left(false, 1080)], SCREEN, SCREEN);
        assert_eq!(sink.take(), Vec::<String>::new());
        // The mouse's own click a moment later goes to the PC.
        mouse.update(&sink, true, [left(true, 1400), left(false, 1450)], SCREEN, SCREEN);
        assert_eq!(sink.take(), ["Left true", "Left false"]);
    }

    #[test]
    fn a_tap_never_swallows_a_later_click_of_the_mouse() {
        let sink = Recorder::default();
        let mut mouse = active_mouse(&sink);
        let left = |down, time| PointerEvent::Button { button: 1, down, time };
        // A tap, then the mouse moves and clicks within the tap's window.
        mouse.update(&sink, true, [PointerEvent::Jumped { time: 1000 }, left(true, 1000), left(false, 1050),
            PointerEvent::Moved { dx: 1.0, dy: 0.0 }, left(true, 1100), left(false, 1150)], SCREEN, SCREEN);
        // A second press after one jump is the mouse's too.
        mouse.update(&sink, true, [PointerEvent::Jumped { time: 2000 }, left(true, 2000), left(false, 2050),
            left(true, 2100), left(false, 2150)], SCREEN, SCREEN);
        assert_eq!(sink.take(), ["move 1 0", "Left true", "Left false", "Left true", "Left false"]);
        // A tap whose release comes in desktop mode leaves nothing behind
        // to swallow the release of the next click in game mode.
        mouse.update(&sink, true, [PointerEvent::Jumped { time: 3000 }, left(true, 3000)], SCREEN, SCREEN);
        mouse.toggle_mode(&sink);
        mouse.update(&sink, true, [left(false, 3050)], SCREEN, SCREEN);
        mouse.toggle_mode(&sink);
        sink.take();
        mouse.update(&sink, true, [left(true, 9000), left(false, 9050)], SCREEN, SCREEN);
        assert_eq!(sink.take(), ["Left true", "Left false"]);
    }

    #[test]
    fn a_tap_on_the_screen_clicks_where_the_finger_is_in_desktop_mode() {
        let sink = Recorder::default();
        let mut mouse = active_mouse(&sink);
        mouse.toggle_mode(&sink);
        mouse.update(&sink, true, [PointerEvent::Jumped { time: 1000 }, PointerEvent::At { x: 640.0, y: 400.0 },
            PointerEvent::Button { button: 1, down: true, time: 1000 }], SCREEN, SCREEN);
        assert_eq!(sink.take(), ["at 16384 16384", "Left true"]);
    }

    #[test]
    fn switching_modes_lets_go_of_held_buttons() {
        let sink = Recorder::default();
        let mut mouse = active_mouse(&sink);
        mouse.update(&sink, true, [PointerEvent::Button { button: 1, down: true, time: 0 }], SCREEN, SCREEN);
        sink.take();
        mouse.toggle_mode(&sink);
        mouse.toggle_mode(&sink);
        assert_eq!((sink.take(), mouse.mode), (vec!["Left false".to_string()], MouseMode::Game));
    }

    #[test]
    fn guide_constant_matches_protocol() {
        assert_eq!(GUIDE, 2);
    }

    fn key(keycode: u32, keysym: u64, text: &str, down: bool) -> x11::Key {
        x11::Key { keycode, keysym, text: text.into(), shortcut: false, down }
    }


    #[test]
    fn keyboard_keys_go_down_and_up_with_their_modifiers() {
        let sink = Recorder::default();
        let mut keys = Keys::default();
        // Shift, A typed as "A" and held through a repeat, then both let go.
        keys.send(&sink, &[key(50, 0xffe1, "", true), key(38, 0x41, "A", true), key(38, 0x41, "A", true),
            key(38, 0x41, "", false), key(50, 0xffe1, "", false)], true);
        assert_eq!(sink.take(), ["key 0xa0 true 1", "key 0x41 true 1", "key 0x41 true 1", "key 0x41 false 1", "key 0xa0 false 0"]);
    }

    #[test]
    fn what_the_on_screen_keyboard_puts_on_another_key_goes_as_text() {
        let sink = Recorder::default();
        let mut keys = Keys::default();
        // Gamescope types я with KEY_1 under a keymap of its own.
        keys.send(&sink, &[key(10, 0x0100_044f, "я", true), key(10, 0x0100_044f, "", false)], true);
        assert_eq!(sink.take(), ["text я"]);
    }

    #[test]
    fn while_the_on_screen_keyboard_is_up_keys_go_as_text_and_named_keys() {
        let sink = Recorder::default();
        let mut keys = Keys::default();
        keys.send(&sink, &[key(50, 0xffe1, "", true), key(38, 0x61, "a", true), key(38, 0x61, "", false),
            key(22, 0xff08, "\u{8}", true), key(22, 0xff08, "", false), key(50, 0xffe1, "", false)], false);
        assert_eq!(sink.take(), ["text a", "key 0x8 true 0", "key 0x8 false 0"]);
    }

    #[test]
    fn losing_the_keys_lets_go_of_the_held_ones_and_skips_their_releases() {
        let sink = Recorder::default();
        let mut keys = Keys::default();
        keys.send(&sink, &[key(37, 0xffe3, "", true), key(54, 0x63, "", true)], true);
        sink.take();
        // The on-screen keyboard came up: the held keys go up before what was typed on it.
        keys.send(&sink, &[key(38, 0x61, "a", true), key(54, 0x63, "", false), key(37, 0xffe3, "", false)], false);
        assert_eq!(sink.take(), ["key 0x43 false 0", "key 0xa2 false 0", "text a"]);
        keys.send(&sink, &[], false);
        assert_eq!(sink.take(), Vec::<String>::new());
    }
}