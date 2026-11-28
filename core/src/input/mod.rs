//! Keyboard, mouse and touch input from every frontend. The frontends only
//! capture platform events as `InputEvent`s; the core turns them into
//! GameStream packets for a PC (`PcInput` into a `Sink`) or into a gamepad
//! for an Xbox (`PadFromKeys`), and recognises trackpad gestures (`Trackpad`).

mod keys;
mod pad;
mod trackpad;

pub use keys::{char_usage, modifier};
pub use pad::{Binding, PadFromKeys, PadLayout, Source, Target};
pub use trackpad::{Gesture, Trackpad};

use keys::Keyboard;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq)]
pub enum InputEvent {
    Key { usage: Usage, down: bool },
    Text(String),
    /// Relative motion in host pixels; positive `dx` is right, positive `dy` down.
    MouseMove { dx: f32, dy: f32 },
    /// Absolute position over the video, 0…1 from its top left corner.
    MousePosition { x: f32, y: f32 },
    MouseButton { button: MouseButton, down: bool },
    /// Wheel notches: positive `dy` scrolls up, away from the user, and
    /// positive `dx` scrolls right.
    Scroll { dx: f32, dy: f32 },
    /// A finger on the video, `x` and `y` 0…1 from its top left corner.
    Touch { id: u32, phase: Phase, x: f32, y: f32, pressure: f32 },
}

/// A physical key; `vk()` gives its Windows virtual-key code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Usage {
    /// A HID usage of the keyboard page 0x07, as `GCKeyboard` reports it.
    Hid(u16),
    /// A Linux evdev `KEY_*` code.
    Evdev(u16),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum MouseButton { Left, Middle, Right, X1, X2 }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase { Began, Moved, Ended, Cancelled }

/// What a key or button press came from: the host hears a release only once
/// no device holds the key or button any more.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Device {
    Keyboard,
    Mouse,
    Touch,
    /// The on-screen key bar.
    Panel,
}

/// The devices holding each key or button down.
struct Holds<K>(BTreeMap<K, BTreeSet<Device>>);

impl<K> Default for Holds<K> {
    fn default() -> Self { Self(BTreeMap::new()) }
}

impl<K: Ord + Copy> Holds<K> {
    /// Whether the host is to hear the change: a press always, a release
    /// once no other device holds `key`.
    fn change(&mut self, key: K, device: Device, down: bool) -> bool {
        if down {
            self.0.entry(key).or_default().insert(device);
            return true;
        }
        let Some(devices) = self.0.get_mut(&key) else { return true };
        devices.remove(&device);
        if !devices.is_empty() { return false; }
        self.0.remove(&key);
        true
    }

    fn keys(&self) -> impl Iterator<Item = K> + '_ { self.0.keys().copied() }

    /// Every key held, now forgotten.
    fn take(&mut self) -> Vec<K> { std::mem::take(&mut self.0).into_keys().collect() }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Touch {
    pub id: u32,
    pub phase: Phase,
    pub x: f32,
    pub y: f32,
    pub pressure: f32,
}

/// Where PC input goes: the methods of `gamestream::Stream`.
pub trait Sink {
    /// `key` is a Windows virtual-key code, `modifiers` `gamestream::MODIFIER_*` bits.
    fn key(&self, key: u8, down: bool, modifiers: u8);
    fn text(&self, text: &str);
    fn mouse_move(&self, dx: i16, dy: i16);
    fn mouse_position(&self, x: i16, y: i16, width: i16, height: i16);
    fn mouse_button(&self, button: MouseButton, down: bool);
    /// 120 is one notch up.
    fn scroll(&self, amount: i16);
    /// 120 is one notch right.
    fn hscroll(&self, amount: i16);
    /// False if the host takes no touches.
    fn touch(&self, touch: Touch) -> bool;
    fn host_supports_touch(&self) -> bool;
}

/// The span `mouse_position` reports positions in: the host scales it to
/// its screen, so any fixed span works and a wide one keeps precision.
const POSITION_SPAN: i16 = i16::MAX;
const WHEEL_DELTA: f32 = 120.0;

/// Turns `InputEvent`s into the calls a PC host takes: virtual keys with
/// the held modifiers, whole pixels and wheel steps with the fractions
/// carried to the next event. Keys and buttons are held per `Device`.
#[derive(Default)]
pub struct PcInput {
    keyboard: Keyboard,
    buttons: Holds<MouseButton>,
    motion: (f32, f32),
    wheel: (f32, f32),
}

impl PcInput {
    pub fn send(&mut self, sink: &impl Sink, device: Device, event: InputEvent) {
        match event {
            InputEvent::Key { usage, down } => {
                if let Some((vk, modifiers)) = self.keyboard.key(device, usage, down) {
                    sink.key(vk, down, modifiers);
                }
            }
            InputEvent::Text(text) => sink.text(&text),
            InputEvent::MouseMove { dx, dy } => {
                let (dx, dy) = (whole(&mut self.motion.0, dx), whole(&mut self.motion.1, dy));
                if dx != 0 || dy != 0 { sink.mouse_move(dx, dy); }
            }
            InputEvent::MousePosition { x, y } => {
                let span = |value: f32| (unit(value) * f32::from(POSITION_SPAN)).round() as i16;
                sink.mouse_position(span(x), span(y), POSITION_SPAN, POSITION_SPAN);
            }
            InputEvent::MouseButton { button, down } => {
                if self.buttons.change(button, device, down) { sink.mouse_button(button, down); }
            }
            InputEvent::Scroll { dx, dy } => {
                let vertical = whole(&mut self.wheel.1, dy * WHEEL_DELTA);
                let horizontal = whole(&mut self.wheel.0, dx * WHEEL_DELTA);
                if vertical != 0 { sink.scroll(vertical); }
                if horizontal != 0 { sink.hscroll(horizontal); }
            }
            InputEvent::Touch { id, phase, x, y, pressure } => {
                sink.touch(Touch { id, phase, x: unit(x), y: unit(y), pressure });
            }
        }
    }

    /// Lets go of every key and mouse button still held, as when the stream
    /// loses focus.
    pub fn release_all(&mut self, sink: &impl Sink) {
        for vk in self.keyboard.release_all() { sink.key(vk, false, 0); }
        for button in self.buttons.take() { sink.mouse_button(button, false); }
        self.motion = (0.0, 0.0);
        self.wheel = (0.0, 0.0);
    }
}

/// `value` within 0…1 of the video; NaN at its start.
fn unit(value: f32) -> f32 {
    if value.is_nan() { 0.0 } else { value.clamp(0.0, 1.0) }
}

/// The whole part of `rest + value` for an i16 field, the fraction kept in `rest`.
fn whole(rest: &mut f32, value: f32) -> i16 {
    let total = *rest + value;
    if !total.is_finite() {
        *rest = 0.0;
        return 0;
    }
    let whole = total.trunc();
    *rest = total - whole;
    whole.clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16
}

/// A PC stream takes core input as GameStream packets.
impl Sink for gamestream::Stream {
    fn key(&self, key: u8, down: bool, modifiers: u8) { gamestream::Stream::key(self, key, down, modifiers) }
    fn text(&self, text: &str) { gamestream::Stream::text(self, text) }
    fn mouse_move(&self, dx: i16, dy: i16) { gamestream::Stream::mouse_move(self, dx, dy) }
    fn mouse_position(&self, x: i16, y: i16, width: i16, height: i16) {
        gamestream::Stream::mouse_position(self, x, y, width, height)
    }
    fn mouse_button(&self, button: MouseButton, down: bool) {
        let button = match button {
            MouseButton::Left => gamestream::MouseButton::Left,
            MouseButton::Middle => gamestream::MouseButton::Middle,
            MouseButton::Right => gamestream::MouseButton::Right,
            MouseButton::X1 => gamestream::MouseButton::X1,
            MouseButton::X2 => gamestream::MouseButton::X2,
        };
        gamestream::Stream::mouse_button(self, button, down)
    }
    fn scroll(&self, amount: i16) { gamestream::Stream::scroll(self, amount) }
    fn hscroll(&self, amount: i16) { gamestream::Stream::hscroll(self, amount) }
    fn touch(&self, touch: Touch) -> bool {
        let phase = match touch.phase {
            Phase::Began => gamestream::TouchPhase::Down,
            Phase::Moved => gamestream::TouchPhase::Move,
            Phase::Ended => gamestream::TouchPhase::Up,
            Phase::Cancelled => gamestream::TouchPhase::Cancel,
        };
        gamestream::Stream::touch(self, gamestream::Touch {
            id: touch.id, phase, x: touch.x, y: touch.y, pressure: touch.pressure,
        })
    }
    fn host_supports_touch(&self) -> bool { gamestream::Stream::host_supports_touch(self) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gamestream::{MODIFIER_CTRL, MODIFIER_SHIFT};
    use std::cell::RefCell;

    #[derive(Debug, PartialEq)]
    enum Call {
        Key(u8, bool, u8),
        Text(String),
        Move(i16, i16),
        Position(i16, i16, i16, i16),
        Button(MouseButton, bool),
        Scroll(i16),
        HScroll(i16),
        Touch(Touch),
    }

    #[derive(Default)]
    struct Recorder {
        calls: RefCell<Vec<Call>>,
        touch: bool,
    }

    impl Recorder {
        fn take(&self) -> Vec<Call> { std::mem::take(&mut self.calls.borrow_mut()) }
    }

    impl Sink for Recorder {
        fn key(&self, key: u8, down: bool, modifiers: u8) { self.calls.borrow_mut().push(Call::Key(key, down, modifiers)) }
        fn text(&self, text: &str) { self.calls.borrow_mut().push(Call::Text(text.into())) }
        fn mouse_move(&self, dx: i16, dy: i16) { self.calls.borrow_mut().push(Call::Move(dx, dy)) }
        fn mouse_position(&self, x: i16, y: i16, width: i16, height: i16) {
            self.calls.borrow_mut().push(Call::Position(x, y, width, height))
        }
        fn mouse_button(&self, button: MouseButton, down: bool) { self.calls.borrow_mut().push(Call::Button(button, down)) }
        fn scroll(&self, amount: i16) { self.calls.borrow_mut().push(Call::Scroll(amount)) }
        fn hscroll(&self, amount: i16) { self.calls.borrow_mut().push(Call::HScroll(amount)) }
        fn touch(&self, touch: Touch) -> bool {
            self.calls.borrow_mut().push(Call::Touch(touch));
            self.touch
        }
        fn host_supports_touch(&self) -> bool { self.touch }
    }

    #[test]
    fn keys_carry_virtual_codes_and_held_modifiers() {
        let (sink, mut pc) = (Recorder::default(), PcInput::default());
        pc.send(&sink, Device::Keyboard, InputEvent::Key { usage: Usage::Hid(0xE0), down: true });
        pc.send(&sink, Device::Keyboard, InputEvent::Key { usage: Usage::Evdev(42), down: true });
        pc.send(&sink, Device::Keyboard, InputEvent::Key { usage: Usage::Hid(0x3A), down: true });
        pc.send(&sink, Device::Keyboard, InputEvent::Key { usage: Usage::Hid(0x3A), down: false });
        pc.send(&sink, Device::Keyboard, InputEvent::Key { usage: Usage::Hid(0x00), down: true });
        assert_eq!(sink.take(), vec![
            Call::Key(0xA2, true, MODIFIER_CTRL),
            Call::Key(0xA0, true, MODIFIER_CTRL | MODIFIER_SHIFT),
            Call::Key(0x70, true, MODIFIER_CTRL | MODIFIER_SHIFT),
            Call::Key(0x70, false, MODIFIER_CTRL | MODIFIER_SHIFT),
        ]);
    }

    #[test]
    fn text_passes_through() {
        let (sink, mut pc) = (Recorder::default(), PcInput::default());
        pc.send(&sink, Device::Keyboard, InputEvent::Text("héllo".into()));
        assert_eq!(sink.take(), vec![Call::Text("héllo".into())]);
    }

    #[test]
    fn fractional_motion_adds_up_to_whole_pixels() {
        let (sink, mut pc) = (Recorder::default(), PcInput::default());
        pc.send(&sink, Device::Mouse, InputEvent::MouseMove { dx: 0.6, dy: -0.4 });
        pc.send(&sink, Device::Mouse, InputEvent::MouseMove { dx: 0.6, dy: -0.4 });
        pc.send(&sink, Device::Mouse, InputEvent::MouseMove { dx: 2.0, dy: -0.4 });
        pc.send(&sink, Device::Mouse, InputEvent::MouseMove { dx: 1e9, dy: f32::NAN });
        assert_eq!(sink.take(), vec![Call::Move(1, 0), Call::Move(2, -1), Call::Move(i16::MAX, 0)]);
    }

    #[test]
    fn positions_span_the_video() {
        let (sink, mut pc) = (Recorder::default(), PcInput::default());
        pc.send(&sink, Device::Mouse, InputEvent::MousePosition { x: 0.5, y: 1.5 });
        pc.send(&sink, Device::Mouse, InputEvent::MousePosition { x: -1.0, y: 0.0 });
        assert_eq!(sink.take(), vec![
            Call::Position(16384, i16::MAX, i16::MAX, i16::MAX),
            Call::Position(0, 0, i16::MAX, i16::MAX),
        ]);
    }

    #[test]
    fn wheel_notches_scale_to_wheel_delta() {
        let (sink, mut pc) = (Recorder::default(), PcInput::default());
        pc.send(&sink, Device::Mouse, InputEvent::Scroll { dx: 0.0, dy: 1.0 });
        pc.send(&sink, Device::Mouse, InputEvent::Scroll { dx: -0.5, dy: 0.0 });
        pc.send(&sink, Device::Mouse, InputEvent::Scroll { dx: 0.0, dy: 0.004 });
        pc.send(&sink, Device::Mouse, InputEvent::Scroll { dx: 0.0, dy: 0.005 });
        assert_eq!(sink.take(), vec![Call::Scroll(120), Call::HScroll(-60), Call::Scroll(1)]);
    }

    #[test]
    fn touches_pass_through_within_the_video() {
        let (sink, mut pc) = (Recorder::default(), PcInput::default());
        pc.send(&sink, Device::Touch, InputEvent::Touch { id: 3, phase: Phase::Began, x: 0.25, y: 0.75, pressure: 0.5 });
        pc.send(&sink, Device::Touch, InputEvent::Touch { id: 3, phase: Phase::Moved, x: -0.5, y: 1.5, pressure: 0.5 });
        pc.send(&sink, Device::Touch, InputEvent::Touch { id: 3, phase: Phase::Ended, x: f32::NAN, y: 0.5, pressure: 0.5 });
        assert_eq!(sink.take(), vec![
            Call::Touch(Touch { id: 3, phase: Phase::Began, x: 0.25, y: 0.75, pressure: 0.5 }),
            Call::Touch(Touch { id: 3, phase: Phase::Moved, x: 0.0, y: 1.0, pressure: 0.5 }),
            Call::Touch(Touch { id: 3, phase: Phase::Ended, x: 0.0, y: 0.5, pressure: 0.5 }),
        ]);
    }

    #[test]
    fn a_held_key_stays_down_when_the_panel_lets_go_of_it() {
        let (sink, mut pc) = (Recorder::default(), PcInput::default());
        let ctrl = |device, down| (device, InputEvent::Key { usage: Usage::Hid(0xE0), down });
        let delete = |down| (Device::Panel, InputEvent::Key { usage: Usage::Hid(0x4C), down });
        // The physical Ctrl is held while the panel sends Ctrl+Delete.
        for (device, event) in [ctrl(Device::Keyboard, true), ctrl(Device::Panel, true), delete(true), delete(false),
                                ctrl(Device::Panel, false)] {
            pc.send(&sink, device, event);
        }
        assert_eq!(sink.take(), vec![
            Call::Key(0xA2, true, MODIFIER_CTRL),
            Call::Key(0xA2, true, MODIFIER_CTRL),
            Call::Key(0x2E, true, MODIFIER_CTRL),
            Call::Key(0x2E, false, MODIFIER_CTRL),
        ]);
        let (device, event) = ctrl(Device::Keyboard, false);
        pc.send(&sink, device, event);
        assert_eq!(sink.take(), vec![Call::Key(0xA2, false, 0)]);
    }

    #[test]
    fn a_button_two_devices_hold_goes_up_with_the_last() {
        let (sink, mut pc) = (Recorder::default(), PcInput::default());
        let left = |down| InputEvent::MouseButton { button: MouseButton::Left, down };
        pc.send(&sink, Device::Mouse, left(true));
        pc.send(&sink, Device::Touch, left(true));
        pc.send(&sink, Device::Touch, left(false));
        assert_eq!(sink.take(), vec![Call::Button(MouseButton::Left, true), Call::Button(MouseButton::Left, true)]);
        pc.send(&sink, Device::Mouse, left(false));
        assert_eq!(sink.take(), vec![Call::Button(MouseButton::Left, false)]);
    }

    #[test]
    fn release_all_lets_go_of_keys_and_buttons() {
        let (sink, mut pc) = (Recorder::default(), PcInput::default());
        pc.send(&sink, Device::Keyboard, InputEvent::Key { usage: Usage::Hid(0xE1), down: true });
        pc.send(&sink, Device::Keyboard, InputEvent::Key { usage: Usage::Hid(0x1A), down: true });
        pc.send(&sink, Device::Mouse, InputEvent::MouseButton { button: MouseButton::Right, down: true });
        pc.send(&sink, Device::Touch, InputEvent::MouseButton { button: MouseButton::Right, down: true });
        pc.send(&sink, Device::Mouse, InputEvent::MouseButton { button: MouseButton::Left, down: true });
        pc.send(&sink, Device::Mouse, InputEvent::MouseButton { button: MouseButton::Left, down: false });
        pc.send(&sink, Device::Mouse, InputEvent::MouseMove { dx: 0.9, dy: 0.0 });
        sink.take();
        pc.release_all(&sink);
        assert_eq!(sink.take(), vec![
            Call::Key(0x57, false, 0),
            Call::Key(0xA0, false, 0),
            Call::Button(MouseButton::Right, false),
        ]);
        pc.release_all(&sink);
        pc.send(&sink, Device::Mouse, InputEvent::MouseMove { dx: 0.9, dy: 0.0 });
        assert_eq!(sink.take(), vec![]);
    }
}
