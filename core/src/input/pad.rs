//! Keyboard and mouse as a gamepad, for the Xbox, which takes nothing else:
//! keys and mouse buttons press pad buttons or push the left stick, and
//! mouse motion pushes the right stick.

use super::{InputEvent, MouseButton};
use crate::route::Routed;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// A key, by its Windows virtual-key code (see `Usage::vk`), or a mouse button.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Source {
    Key(u8),
    Mouse(MouseButton),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Target {
    A, B, X, Y,
    View, Menu,
    LeftBumper, RightBumper,
    LeftTrigger, RightTrigger,
    LeftThumb, RightThumb,
    DpadUp, DpadDown, DpadLeft, DpadRight,
    /// The left stick, pushed all the way.
    StickUp, StickDown, StickLeft, StickRight,
}

impl Target {
    /// The button's bit in `Routed::buttons`, or 0 for the triggers and stick.
    fn bit(self) -> u16 {
        match self {
            Target::Menu => 4,
            Target::View => 8,
            Target::A => 16,
            Target::B => 32,
            Target::X => 64,
            Target::Y => 128,
            Target::DpadUp => 256,
            Target::DpadDown => 512,
            Target::DpadLeft => 1024,
            Target::DpadRight => 2048,
            Target::LeftBumper => 4096,
            Target::RightBumper => 8192,
            Target::LeftThumb => 16384,
            Target::RightThumb => 32768,
            _ => 0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Binding {
    pub source: Source,
    pub target: Target,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PadLayout {
    pub bindings: Vec<Binding>,
    /// Right-stick deflection, 0…1, per pixel a millisecond of mouse travel.
    pub sensitivity: f32,
    /// The least right-stick deflection while the mouse moves, 0…1, to get
    /// past the game's own dead zone.
    pub dead_zone: f32,
    /// Once the mouse stops, the right stick falls to about a third in this
    /// many milliseconds.
    pub decay_ms: f32,
}

impl Default for PadLayout {
    /// Close to Better xCloud's: WASD walk, the mouse looks, the left button
    /// fires and the right one aims.
    fn default() -> Self {
        use Target::*;
        let key = |vk: u8, target| Binding { source: Source::Key(vk), target };
        let mouse = |button, target| Binding { source: Source::Mouse(button), target };
        Self {
            bindings: vec![
                key(b'W', StickUp), key(b'S', StickDown), key(b'A', StickLeft), key(b'D', StickRight),
                key(b' ', A), key(b'E', A),
                key(0xA2, B), key(0x08, B),       // left Ctrl, Backspace
                key(b'R', X),
                key(b'V', Y),
                key(b'Q', RightBumper),
                key(b'C', LeftBumper), key(b'G', LeftBumper),
                key(0xA0, LeftThumb),             // left Shift
                key(b'F', RightThumb),
                key(0x09, View),                  // Tab
                key(0x1B, Menu), key(0x0D, Menu), // Escape, Enter
                key(b'1', DpadUp), key(b'2', DpadRight), key(b'3', DpadDown), key(b'4', DpadLeft),
                key(0x26, DpadUp), key(0x27, DpadRight), key(0x28, DpadDown), key(0x25, DpadLeft),
                mouse(MouseButton::Left, RightTrigger),
                mouse(MouseButton::Right, LeftTrigger),
                mouse(MouseButton::Middle, RightThumb),
            ],
            sensitivity: 0.5,
            dead_zone: 0.2,
            decay_ms: 50.0,
        }
    }
}

/// Keyboard and mouse events in, a gamepad state out each input tick.
pub struct PadFromKeys {
    layout: PadLayout,
    held: HashSet<Source>,
    /// Pressed since the last state, so a press and release between two
    /// ticks still shows in one.
    pressed: HashSet<Source>,
    motion: (f32, f32),
    /// The right stick before the dead zone, -1…1 each way.
    look: (f32, f32),
    last: Option<f64>,
}

impl PadFromKeys {
    pub fn new(layout: PadLayout) -> Self {
        Self { layout, held: HashSet::new(), pressed: HashSet::new(), motion: (0.0, 0.0), look: (0.0, 0.0), last: None }
    }

    pub fn event(&mut self, event: &InputEvent) {
        let (source, down) = match *event {
            InputEvent::Key { usage, down } => match usage.vk() {
                Some(vk) => (Source::Key(vk), down),
                None => return,
            },
            InputEvent::MouseButton { button, down } => (Source::Mouse(button), down),
            InputEvent::MouseMove { dx, dy } => {
                if dx.is_finite() && dy.is_finite() {
                    self.motion = (self.motion.0 + dx, self.motion.1 + dy);
                }
                return;
            }
            _ => return,
        };
        if down {
            self.held.insert(source);
            self.pressed.insert(source);
        } else {
            self.held.remove(&source);
        }
    }

    /// Lets go of everything, as when the stream loses focus.
    pub fn release_all(&mut self) {
        self.held.clear();
        self.pressed.clear();
        self.motion = (0.0, 0.0);
        self.look = (0.0, 0.0);
    }

    /// The pad at `now`, in milliseconds. Call it once each input tick: the
    /// right stick follows the mouse travel since the last call.
    pub fn state(&mut self, now: f64) -> Routed {
        let mut routed = Routed { axes: [0; 4], buttons: 0, lt: 0, rt: 0 };
        let (mut up, mut south, mut left, mut right) = (false, false, false, false);
        let pressed = std::mem::take(&mut self.pressed);
        let down = |source| self.held.contains(source) || pressed.contains(source);
        for binding in self.layout.bindings.iter().filter(|binding| down(&binding.source)) {
            match binding.target {
                Target::LeftTrigger => routed.lt = u16::MAX,
                Target::RightTrigger => routed.rt = u16::MAX,
                Target::StickUp => up = true,
                Target::StickDown => south = true,
                Target::StickLeft => left = true,
                Target::StickRight => right = true,
                target => routed.buttons |= target.bit(),
            }
        }
        let walk = (f32::from(right) - f32::from(left), f32::from(south) - f32::from(up));
        let walk = scaled(walk, length(walk).min(1.0));
        self.look(now);
        let reach = length(self.look);
        let reach = if reach > 0.0 { self.layout.dead_zone + (1.0 - self.layout.dead_zone) * reach } else { 0.0 };
        let look = scaled(self.look, reach);
        routed.axes = [axis(walk.0), axis(walk.1), axis(look.0), axis(look.1)];
        routed
    }

    fn look(&mut self, now: f64) {
        let Some(last) = self.last.replace(now) else { return };
        let elapsed = now - last;
        if elapsed.is_nan() || elapsed <= 0.0 {
            self.last = Some(last.max(now));
            return;
        }
        let motion = std::mem::take(&mut self.motion);
        if motion != (0.0, 0.0) {
            let speed = self.layout.sensitivity / elapsed as f32;
            let look = (motion.0 * speed, motion.1 * speed);
            self.look = scaled(look, length(look).min(1.0));
        } else {
            let decay_ms = self.layout.decay_ms;
            let left = if decay_ms > 0.0 { (-elapsed as f32 / decay_ms).exp() } else { 0.0 };
            self.look = (self.look.0 * left, self.look.1 * left);
            if length(self.look) < 0.01 { self.look = (0.0, 0.0); }
        }
    }
}

fn length(vector: (f32, f32)) -> f32 {
    vector.0.hypot(vector.1)
}

/// `vector` with its length set to `reach`, the zero vector staying zero.
fn scaled(vector: (f32, f32), reach: f32) -> (f32, f32) {
    let length = length(vector);
    if length <= 0.0 { return (0.0, 0.0); }
    (vector.0 / length * reach, vector.1 / length * reach)
}

fn axis(value: f32) -> i16 {
    (value.clamp(-1.0, 1.0) * f32::from(i16::MAX)).round() as i16
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::Usage;

    fn key(pad: &mut PadFromKeys, hid: u16, down: bool) {
        pad.event(&InputEvent::Key { usage: Usage::Hid(hid), down });
    }

    fn mouse(pad: &mut PadFromKeys, button: MouseButton, down: bool) {
        pad.event(&InputEvent::MouseButton { button, down });
    }

    fn look(pad: &mut PadFromKeys, dx: f32, dy: f32) {
        pad.event(&InputEvent::MouseMove { dx, dy });
    }

    const W: u16 = 0x1A;
    const A: u16 = 0x04;
    const D: u16 = 0x07;
    const SPACE: u16 = 0x2C;
    const ENTER: u16 = 0x28;
    const ESCAPE: u16 = 0x29;
    const LEFT_SHIFT: u16 = 0xE1;

    #[test]
    fn wasd_walks_the_left_stick_with_even_diagonals() {
        let mut pad = PadFromKeys::new(PadLayout::default());
        key(&mut pad, W, true);
        assert_eq!(pad.state(0.0).axes, [0, -32767, 0, 0], "up is negative, as Gamepad.swift reports it");
        key(&mut pad, D, true);
        assert_eq!(pad.state(8.0).axes, [23170, -23170, 0, 0]);
        key(&mut pad, A, true);
        assert_eq!(pad.state(16.0).axes, [0, -32767, 0, 0], "left and right cancel");
        key(&mut pad, W, false);
        key(&mut pad, D, false);
        assert_eq!(pad.state(24.0).axes, [-32767, 0, 0, 0]);
        key(&mut pad, A, false);
        assert_eq!(pad.state(32.0).axes, [0; 4]);
    }

    #[test]
    fn keys_and_mouse_buttons_press_and_release_pad_buttons() {
        let mut pad = PadFromKeys::new(PadLayout::default());
        key(&mut pad, SPACE, true);
        key(&mut pad, LEFT_SHIFT, true);
        key(&mut pad, ESCAPE, true);
        mouse(&mut pad, MouseButton::Left, true);
        mouse(&mut pad, MouseButton::Right, true);
        let state = pad.state(0.0);
        assert_eq!(state.buttons, 16 | 16384 | 4);
        assert_eq!((state.lt, state.rt), (u16::MAX, u16::MAX));

        key(&mut pad, SPACE, false);
        key(&mut pad, LEFT_SHIFT, false);
        mouse(&mut pad, MouseButton::Left, false);
        let state = pad.state(8.0);
        assert_eq!(state.buttons, 4);
        assert_eq!((state.lt, state.rt), (u16::MAX, 0));
    }

    #[test]
    fn a_button_bound_to_two_keys_stays_down_until_both_go_up() {
        let mut pad = PadFromKeys::new(PadLayout::default());
        key(&mut pad, ESCAPE, true);
        key(&mut pad, ENTER, true);
        key(&mut pad, ESCAPE, false);
        assert_eq!(pad.state(0.0).buttons, 4);
        key(&mut pad, ENTER, true);
        assert_eq!(pad.state(8.0).buttons, 4);
        key(&mut pad, ENTER, false);
        assert_eq!(pad.state(16.0).buttons, 0, "a repeated key-down does not count twice");
    }

    #[test]
    fn mouse_travel_pushes_the_right_stick_past_the_dead_zone() {
        let layout = PadLayout { sensitivity: 0.5, dead_zone: 0.2, decay_ms: 50.0, ..PadLayout::default() };
        let mut pad = PadFromKeys::new(layout);
        pad.state(0.0);
        look(&mut pad, 4.0, 0.0);
        // 4 px in 10 ms: 0.4 px/ms × 0.5 = 0.2, past the dead zone 0.2 + 0.8 × 0.2.
        assert_eq!(pad.state(10.0).axes, [0, 0, axis(0.36), 0]);
        look(&mut pad, 0.0, 1000.0);
        assert_eq!(pad.state(20.0).axes, [0, 0, 0, 32767], "down is positive, and the stick tops out");
        look(&mut pad, -1.0, 0.0);
        assert_eq!(pad.state(30.0).axes, [0, 0, axis(-(0.2 + 0.8 * 0.05)), 0]);
    }

    #[test]
    fn the_right_stick_decays_once_the_mouse_stops() {
        let mut pad = PadFromKeys::new(PadLayout { decay_ms: 50.0, ..PadLayout::default() });
        pad.state(0.0);
        look(&mut pad, 100.0, 0.0);
        assert_eq!(pad.state(10.0).axes[2], 32767);
        let mut previous = 32767;
        for tick in 2..=6 {
            let now = f64::from(tick) * 10.0;
            let value = pad.state(now).axes[2];
            assert!(value < previous && value > 0, "tick {tick}: {value} after {previous}");
            previous = value;
        }
        let reach = (-50.0f32 / 50.0).exp();
        assert_eq!(previous, axis(0.2 + 0.8 * reach));
        assert_eq!(pad.state(400.0).axes[2], 0, "settles to rest");
    }

    #[test]
    fn time_standing_still_or_going_back_changes_nothing() {
        let mut pad = PadFromKeys::new(PadLayout::default());
        pad.state(100.0);
        look(&mut pad, 4.0, 0.0);
        assert_eq!(pad.state(100.0).axes[2], 0, "no time has passed: the travel waits");
        assert_eq!(pad.state(50.0).axes[2], 0, "nor when the clock goes back");
        assert_eq!(pad.state(f64::NAN).axes[2], 0, "nor when it is no time at all");
        assert_eq!(pad.state(110.0).axes[2], axis(0.36), "the travel counts on the next real tick");
    }

    #[test]
    fn a_click_between_two_ticks_shows_in_one() {
        let mut pad = PadFromKeys::new(PadLayout::default());
        mouse(&mut pad, MouseButton::Left, true);
        mouse(&mut pad, MouseButton::Left, false);
        key(&mut pad, SPACE, true);
        key(&mut pad, SPACE, false);
        let state = pad.state(0.0);
        assert_eq!((state.rt, state.buttons), (u16::MAX, 16));
        let state = pad.state(8.0);
        assert_eq!((state.rt, state.buttons), (0, 0));
    }

    #[test]
    fn a_decay_of_zero_or_less_stops_the_stick_at_once() {
        for decay_ms in [0.0, -50.0] {
            let mut pad = PadFromKeys::new(PadLayout { decay_ms, ..PadLayout::default() });
            pad.state(0.0);
            look(&mut pad, 1.0, 0.0);
            assert!(pad.state(10.0).axes[2] > 0);
            assert_eq!(pad.state(20.0).axes[2], 0, "decay_ms {decay_ms}");
        }
    }

    #[test]
    fn release_all_lets_go() {
        let mut pad = PadFromKeys::new(PadLayout::default());
        pad.state(0.0);
        key(&mut pad, W, true);
        mouse(&mut pad, MouseButton::Left, true);
        look(&mut pad, 100.0, 0.0);
        pad.state(10.0);
        look(&mut pad, 100.0, 0.0);
        pad.release_all();
        assert_eq!(pad.state(20.0), Routed { axes: [0; 4], buttons: 0, lt: 0, rt: 0 });
    }

    #[test]
    fn a_custom_layout_round_trips_through_json() {
        let layout = PadLayout {
            bindings: vec![
                Binding { source: Source::Key(b'J'), target: Target::Y },
                Binding { source: Source::Mouse(MouseButton::X1), target: Target::DpadLeft },
            ],
            sensitivity: 1.5,
            dead_zone: 0.1,
            decay_ms: 30.0,
        };
        let json = serde_json::to_string(&layout).unwrap();
        assert_eq!(serde_json::from_str::<PadLayout>(&json).unwrap(), layout);

        let mut pad = PadFromKeys::new(serde_json::from_str(&json).unwrap());
        key(&mut pad, 0x0D, true); // J
        mouse(&mut pad, MouseButton::X1, true);
        key(&mut pad, SPACE, true);
        assert_eq!(pad.state(0.0).buttons, 128 | 1024);
    }

    #[test]
    fn missing_fields_take_the_defaults() {
        let layout: PadLayout = serde_json::from_str(r#"{"sensitivity": 2.0}"#).unwrap();
        assert_eq!(layout.sensitivity, 2.0);
        assert_eq!(layout.bindings, PadLayout::default().bindings);
        let layout: PadLayout = serde_json::from_str(
            r#"{"bindings": [{"source": {"Key": 87}, "target": "StickUp"}]}"#).unwrap();
        assert_eq!(layout.bindings, vec![Binding { source: Source::Key(b'W'), target: Target::StickUp }]);
    }
}
