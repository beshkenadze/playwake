//! C ABI for keyboard, mouse and touch input to the current stream. With a
//! PC stream the core sends it as GameStream packets (`PcInput`); without
//! one it becomes a gamepad (`PadFromKeys`) that the Xbox stream mixes into
//! its controller with `pw_input_pad` before the stream-input route.
//! While the route's menu is open (`menu`) the gate is shut: input calls
//! change nothing. Every call is safe from any thread.

use crate::bridge::{lock, read_string};
use crate::input_route::PwPad;
use crate::pc_bridge;
use playwake_core::gamestream;
use playwake_core::input::{
    char_usage, Device, Gesture, InputEvent, MouseButton, PadFromKeys, PadLayout, PcInput, Phase, Sink, Touch,
    Trackpad, Usage,
};
use std::collections::BTreeMap;
use std::os::raw::c_char;
use std::sync::{Arc, LazyLock, Mutex};

/// Mouse: relative motion, for a captured pointer.
pub const PW_INPUT_GAME: u32 = 0;
/// Mouse: absolute position over the video.
pub const PW_INPUT_DESKTOP: u32 = 1;
/// Touch: the screen is a laptop trackpad (`Trackpad` gestures).
pub const PW_INPUT_TRACKPAD: u32 = 2;
/// Touch: a finger touches the video where it is.
pub const PW_INPUT_DIRECT: u32 = 3;

pub const PW_INPUT_BUTTON_LEFT: u32 = 0;
pub const PW_INPUT_BUTTON_RIGHT: u32 = 1;
pub const PW_INPUT_BUTTON_MIDDLE: u32 = 2;
pub const PW_INPUT_BUTTON_X1: u32 = 3;
pub const PW_INPUT_BUTTON_X2: u32 = 4;

pub const PW_INPUT_PHASE_BEGAN: u32 = 0;
pub const PW_INPUT_PHASE_MOVED: u32 = 1;
pub const PW_INPUT_PHASE_ENDED: u32 = 2;
pub const PW_INPUT_PHASE_CANCELLED: u32 = 3;

/// Where a key or button comes from: the host hears it let go only once no
/// source holds it. Touch is the core's own, from `pw_input_touch`.
pub const PW_INPUT_SOURCE_KEYBOARD: u32 = 0;
pub const PW_INPUT_SOURCE_MOUSE: u32 = 1;
/// The on-screen key bar and keyboard.
pub const PW_INPUT_SOURCE_PANEL: u32 = 2;

/// Set in `pw_input_char_usage`'s result: the character takes Shift.
pub const PW_INPUT_CHAR_SHIFT: u32 = 1 << 16;

/// Returned by `pw_input_touch`: three fingers asked for the on-screen keyboard.
pub const PW_INPUT_SHOW_KEYBOARD: u32 = 1 << 0;

/// The HID usage page of keyboard keys.
const KEYBOARD_PAGE: u16 = 0x07;

/// The PC stream input goes to, with its session.
type Pc<'a, S> = Option<(u64, &'a S)>;

struct Input {
    /// The route of the current stream, by address; only its menu shuts the gate.
    route: usize,
    /// The route's menu is open: input waits, nothing is held.
    paused: bool,
    desktop: bool,
    direct: bool,
    /// The PC session `pc` holds keys and buttons for; `None` without one.
    session: Option<u64>,
    pc: PcInput,
    pad: PadFromKeys,
    trackpad: Trackpad,
    /// Direct touch on a host without touch: the finger holding the left button.
    finger: Option<u32>,
    /// Direct touches the host has down, with their last point.
    touches: BTreeMap<u32, (f32, f32)>,
}

impl Default for Input {
    fn default() -> Self {
        Self {
            route: 0,
            paused: false,
            desktop: false,
            direct: false,
            session: None,
            pc: PcInput::default(),
            pad: PadFromKeys::new(PadLayout::default()),
            trackpad: Trackpad::new(0.0, 0.0),
            finger: None,
            touches: BTreeMap::new(),
        }
    }
}

impl Input {
    /// The PC sink, after forgetting what was held for another stream: its
    /// key-ups never reached this one, so nothing is sent for them.
    fn sync<'s, S: Sink>(&mut self, pc: Pc<'s, S>) -> Option<&'s S> {
        let session = pc.map(|(session, _)| session);
        if session != self.session {
            self.session = session;
            self.pc = PcInput::default();
            self.pad.release_all();
            self.trackpad.reset();
            self.finger = None;
            self.touches.clear();
        }
        pc.map(|(_, sink)| sink)
    }

    fn send<S: Sink>(&mut self, pc: Pc<S>, device: Device, event: InputEvent) {
        if self.paused { return; }
        match self.sync(pc) {
            Some(sink) => self.pc.send(sink, device, event),
            None => self.pad.event(&event),
        }
    }

    /// Shuts the gate as the route's menu opens, letting go of everything
    /// in the same step, and opens it as the menu closes.
    fn menu<S: Sink>(&mut self, pc: Pc<S>, open: bool) {
        if open && !self.paused { self.release_all(pc); }
        self.paused = open;
    }

    fn mode<S: Sink>(&mut self, pc: Pc<S>, mode: u32) {
        let (desktop, direct) = match mode {
            PW_INPUT_GAME => (false, self.direct),
            PW_INPUT_DESKTOP => (true, self.direct),
            PW_INPUT_TRACKPAD => (self.desktop, false),
            PW_INPUT_DIRECT => (self.desktop, true),
            _ => return,
        };
        if (desktop, direct) != (self.desktop, self.direct) {
            self.release_all(pc);
            (self.desktop, self.direct) = (desktop, direct);
        }
    }

    fn key<S: Sink>(&mut self, pc: Pc<S>, device: Device, page: u16, usage: u16, down: bool) {
        if page == KEYBOARD_PAGE {
            self.send(pc, device, InputEvent::Key { usage: Usage::Hid(usage), down });
        }
    }

    /// A PC in desktop mode takes positions only; the Xbox pad always takes motion.
    fn mouse_move<S: Sink>(&mut self, pc: Pc<S>, dx: f32, dy: f32) {
        if !(self.desktop && pc.is_some()) {
            self.send(pc, Device::Mouse, InputEvent::MouseMove { dx, dy });
        }
    }

    fn mouse_position<S: Sink>(&mut self, pc: Pc<S>, x: f32, y: f32) {
        if self.desktop && pc.is_some() {
            self.send(pc, Device::Mouse, InputEvent::MousePosition { x, y });
        }
    }

    fn surface<S: Sink>(&mut self, pc: Pc<S>, width: f32, height: f32) {
        self.sync(pc);
        let gestures = self.trackpad.reset();
        self.gestures(pc, gestures);
        self.trackpad = Trackpad::new(width, height);
    }

    fn touch<S: Sink>(&mut self, pc: Pc<S>, touch: Touch, now: f64) -> u32 {
        let Touch { id, phase, x, y, pressure } = touch;
        if self.paused { return 0; }
        // First, so a new stream's reset cannot cut a gesture short.
        let sink = self.sync(pc);
        if !self.direct {
            let gestures = self.trackpad.touch(id, phase, x, y, now);
            return self.gestures(pc, gestures);
        }
        match sink {
            Some(sink) if sink.host_supports_touch() => {
                // A finger cancelled by a release stays off the host until it lifts.
                let known = match phase {
                    Phase::Began => self.touches.insert(id, (x, y)).is_none(),
                    Phase::Moved => self.touches.get_mut(&id).map(|point| *point = (x, y)).is_some(),
                    Phase::Ended | Phase::Cancelled => self.touches.remove(&id).is_some(),
                };
                if known { self.pc.send(sink, Device::Touch, InputEvent::Touch { id, phase, x, y, pressure }); }
            }
            Some(sink) => self.touch_mouse(sink, id, phase, x, y),
            // An Xbox has no pointer to touch with.
            None => {}
        }
        0
    }

    /// The first finger as the mouse: it presses the left button where it lands.
    fn touch_mouse<S: Sink>(&mut self, sink: &S, id: u32, phase: Phase, x: f32, y: f32) {
        let mut send = |event| self.pc.send(sink, Device::Touch, event);
        match (phase, self.finger) {
            (Phase::Began, None) => {
                self.finger = Some(id);
                send(InputEvent::MousePosition { x, y });
                send(InputEvent::MouseButton { button: MouseButton::Left, down: true });
            }
            (Phase::Moved, Some(finger)) if finger == id => send(InputEvent::MousePosition { x, y }),
            (Phase::Ended | Phase::Cancelled, Some(finger)) if finger == id => {
                self.finger = None;
                if phase == Phase::Ended { send(InputEvent::MousePosition { x, y }); }
                send(InputEvent::MouseButton { button: MouseButton::Left, down: false });
            }
            _ => {}
        }
    }

    fn gestures<S: Sink>(&mut self, pc: Pc<S>, gestures: Vec<Gesture>) -> u32 {
        gestures.into_iter().fold(0, |shown, gesture| match gesture {
            Gesture::Input(event) => {
                self.send(pc, Device::Touch, event);
                shown
            }
            Gesture::Keyboard => shown | PW_INPUT_SHOW_KEYBOARD,
        })
    }

    fn release_all<S: Sink>(&mut self, pc: Pc<S>) {
        // The PC side lets go of a drag's or a finger's button with the rest.
        self.trackpad.reset();
        self.finger = None;
        match self.sync(pc) {
            Some(sink) => {
                for (id, (x, y)) in std::mem::take(&mut self.touches) {
                    self.pc.send(sink, Device::Touch, InputEvent::Touch { id, phase: Phase::Cancelled, x, y, pressure: 0.0 });
                }
                self.pc.release_all(sink);
            }
            None => self.pad.release_all(),
        }
    }

    /// Mixes the keyboard-and-mouse pad into `pad`: buttons add up, the
    /// triggers take the stronger press, and a stick axis the keys push wins.
    fn pad(&mut self, pad: &mut PwPad, now: f64) {
        let keys = self.pad.state(now);
        pad.buttons |= keys.buttons;
        pad.lt = pad.lt.max(keys.lt);
        pad.rt = pad.rt.max(keys.rt);
        for (axis, key) in pad.axes.iter_mut().zip(keys.axes) {
            if key != 0 { *axis = key; }
        }
    }
}

static INPUT: LazyLock<Mutex<Input>> = LazyLock::new(Mutex::default);

/// Runs `f` on the input state and the current PC stream.
fn with<R>(f: impl FnOnce(&mut Input, Pc<gamestream::Stream>) -> R) -> R {
    locked(&INPUT, pc_bridge::stream, f)
}

/// The stream is looked up under the input lock: a call that saw no stream
/// cannot take the lock after one that already sent to a new stream.
fn locked<R, S: Sink>(input: &Mutex<Input>, stream: impl FnOnce() -> Option<(u64, Arc<S>)>,
    f: impl FnOnce(&mut Input, Pc<S>) -> R) -> R
{
    let mut input = input.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let stream = stream();
    f(&mut input, stream.as_ref().map(|(session, stream)| (*session, &**stream)))
}

/// A new stream's route: its gate starts open, and an older route's menu
/// no longer moves it.
pub(crate) fn route_started(route: usize) {
    with(|input, pc| {
        input.route = route;
        input.menu(pc, false);
    });
}

/// Opens or shuts the input gate with the current route's menu.
pub(crate) fn menu(route: usize, open: bool) {
    with(|input, pc| if input.route == route { input.menu(pc, open) });
}

/// Serialises the tests that use the global input state.
#[cfg(test)]
pub(crate) fn global() -> std::sync::MutexGuard<'static, ()> {
    static GLOBAL: Mutex<()> = Mutex::new(());
    lock(&GLOBAL)
}

fn source(source: u32) -> Option<Device> {
    Some(match source {
        PW_INPUT_SOURCE_KEYBOARD => Device::Keyboard,
        PW_INPUT_SOURCE_MOUSE => Device::Mouse,
        PW_INPUT_SOURCE_PANEL => Device::Panel,
        _ => return None,
    })
}

fn button(button: u32) -> Option<MouseButton> {
    Some(match button {
        PW_INPUT_BUTTON_LEFT => MouseButton::Left,
        PW_INPUT_BUTTON_RIGHT => MouseButton::Right,
        PW_INPUT_BUTTON_MIDDLE => MouseButton::Middle,
        PW_INPUT_BUTTON_X1 => MouseButton::X1,
        PW_INPUT_BUTTON_X2 => MouseButton::X2,
        _ => return None,
    })
}

fn phase(phase: u32) -> Option<Phase> {
    Some(match phase {
        PW_INPUT_PHASE_BEGAN => Phase::Began,
        PW_INPUT_PHASE_MOVED => Phase::Moved,
        PW_INPUT_PHASE_ENDED => Phase::Ended,
        PW_INPUT_PHASE_CANCELLED => Phase::Cancelled,
        _ => return None,
    })
}

/// Sets one of the two modes: `PW_INPUT_GAME` or `PW_INPUT_DESKTOP` for the
/// mouse, `PW_INPUT_TRACKPAD` or `PW_INPUT_DIRECT` for touch. A change lets go
/// of everything held.
#[unsafe(no_mangle)]
pub extern "C" fn pw_input_mode(mode: u32) {
    with(|input, pc| input.mode(pc, mode));
}

/// A key by its HID usage from a `PW_INPUT_SOURCE_*`; only the keyboard
/// page (0x07) is taken.
#[unsafe(no_mangle)]
pub extern "C" fn pw_input_key(source: u32, usage_page: u16, usage: u16, down: bool) {
    let Some(device) = self::source(source) else { return };
    with(|input, pc| input.key(pc, device, usage_page, usage, down));
}

/// The HID usage that types the Unicode scalar `c` on a US keyboard, with
/// `PW_INPUT_CHAR_SHIFT` if it takes Shift; 0 for none.
#[unsafe(no_mangle)]
pub extern "C" fn pw_input_char_usage(c: u32) -> u32 {
    char::from_u32(c).and_then(char_usage)
        .map_or(0, |(usage, shift)| u32::from(usage) | if shift { PW_INPUT_CHAR_SHIFT } else { 0 })
}

/// Types UTF-8 `text` on a PC.
///
/// # Safety
/// `text` must be a valid C string, or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pw_input_text(text: *const c_char) {
    let Some(text) = (unsafe { read_string(text) }) else { return };
    with(|input, pc| input.send(pc, Device::Panel, InputEvent::Text(text)));
}

/// Relative mouse motion in host pixels, `dy` positive down. A PC in
/// desktop mode ignores it.
#[unsafe(no_mangle)]
pub extern "C" fn pw_input_mouse_move(dx: f32, dy: f32) {
    with(|input, pc| input.mouse_move(pc, dx, dy));
}

/// The pointer over the video, 0…1 from its top left corner. Only a PC in
/// desktop mode takes it.
#[unsafe(no_mangle)]
pub extern "C" fn pw_input_mouse_position(x: f32, y: f32) {
    with(|input, pc| input.mouse_position(pc, x, y));
}

/// A `PW_INPUT_BUTTON_*` pressed or released by a `PW_INPUT_SOURCE_*`.
#[unsafe(no_mangle)]
pub extern "C" fn pw_input_mouse_button(source: u32, button: u32, down: bool) {
    let (Some(device), Some(button)) = (self::source(source), self::button(button)) else { return };
    with(|input, pc| input.send(pc, device, InputEvent::MouseButton { button, down }));
}

/// Wheel notches, fractions allowed: `dy` positive scrolls up, `dx` right.
#[unsafe(no_mangle)]
pub extern "C" fn pw_input_scroll(dx: f32, dy: f32) {
    with(|input, pc| input.send(pc, Device::Mouse, InputEvent::Scroll { dx, dy }));
}

/// The touch surface in points, for the trackpad's distances.
#[unsafe(no_mangle)]
pub extern "C" fn pw_input_surface(width: f32, height: f32) {
    with(|input, pc| input.surface(pc, width, height));
}

/// One touch, `phase` a `PW_INPUT_PHASE_*`, `now_ms` a monotonic clock. In
/// trackpad mode `x` and `y` are 0…1 over the surface and the core reads
/// gestures; in direct mode they are 0…1 over the video and a PC gets a
/// touch, or the left button there if it takes no touches. An `ENDED`
/// touch carries the finger's last point. Returns `PW_INPUT_*` bits.
#[unsafe(no_mangle)]
pub extern "C" fn pw_input_touch(id: u32, phase: u32, x: f32, y: f32, pressure: f32, now_ms: f64) -> u32 {
    let Some(phase) = self::phase(phase) else { return 0 };
    with(|input, pc| input.touch(pc, Touch { id, phase, x, y, pressure }, now_ms))
}

/// Lets go of every key and button, as when the stream loses focus or its
/// menu opens.
#[unsafe(no_mangle)]
pub extern "C" fn pw_input_release_all() {
    with(|input, pc| input.release_all(pc));
}

/// Whether the PC stream takes touches; false without one.
#[unsafe(no_mangle)]
pub extern "C" fn pw_input_host_supports_touch() -> bool {
    pc_bridge::stream().is_some_and(|(_, stream)| stream.host_supports_touch())
}

/// Mixes the keyboard and mouse, as a gamepad, into `pad`: call it each
/// Xbox input tick before `pw_route_input`.
///
/// # Safety
/// `pad` must be valid, or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pw_input_pad(pad: *mut PwPad, now_ms: f64) {
    let Some(pad) = (unsafe { pad.as_mut() }) else { return };
    lock(&INPUT).pad(pad, now_ms);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    const SHIFT: u16 = 0xE1;
    const A: u16 = 0x04;
    const W: u16 = 0x1A;

    #[derive(Debug, PartialEq)]
    enum Call {
        Key(u8, bool, u8),
        Text(String),
        Move(i16, i16),
        Position(i16, i16),
        Button(MouseButton, bool),
        Scroll(i16),
        Touch(u32, Phase),
    }

    #[derive(Default)]
    struct Host {
        calls: RefCell<Vec<Call>>,
        touch: Cell<bool>,
    }

    impl Host {
        fn take(&self) -> Vec<Call> { self.calls.take() }
        fn push(&self, call: Call) { self.calls.borrow_mut().push(call) }
    }

    impl Sink for Host {
        fn key(&self, key: u8, down: bool, modifiers: u8) { self.push(Call::Key(key, down, modifiers)) }
        fn text(&self, text: &str) { self.push(Call::Text(text.into())) }
        fn mouse_move(&self, dx: i16, dy: i16) { self.push(Call::Move(dx, dy)) }
        fn mouse_position(&self, x: i16, y: i16, _: i16, _: i16) { self.push(Call::Position(x, y)) }
        fn mouse_button(&self, button: MouseButton, down: bool) { self.push(Call::Button(button, down)) }
        fn scroll(&self, amount: i16) { self.push(Call::Scroll(amount)) }
        fn hscroll(&self, _: i16) {}
        fn touch(&self, touch: Touch) -> bool {
            self.push(Call::Touch(touch.id, touch.phase));
            self.touch.get()
        }
        fn host_supports_touch(&self) -> bool { self.touch.get() }
    }

    const XBOX: Pc<'static, Host> = None;

    fn pad() -> PwPad { PwPad { axes: [0; 4], buttons: 0, lt: 0, rt: 0 } }

    fn touch(id: u32, phase: Phase, x: f32, y: f32) -> Touch { Touch { id, phase, x, y, pressure: 1.0 } }

    #[test]
    fn pc_keys_carry_held_modifiers_and_other_pages_are_ignored() {
        let (host, mut input) = (Host::default(), Input::default());
        let pc = Some((1, &host));
        input.key(pc, Device::Keyboard, KEYBOARD_PAGE, SHIFT, true);
        input.key(pc, Device::Keyboard, KEYBOARD_PAGE, A, true);
        input.key(pc, Device::Keyboard, 0x0C, A, true);
        assert_eq!(host.take(), [Call::Key(0xA0, true, 1), Call::Key(0x41, true, 1)]);
    }

    #[test]
    fn game_mode_sends_motion_and_desktop_mode_positions() {
        let (host, mut input) = (Host::default(), Input::default());
        let pc = Some((1, &host));
        input.mouse_move(pc, 3.0, -2.0);
        input.mouse_position(pc, 0.5, 0.5);
        assert_eq!(host.take(), [Call::Move(3, -2)]);
        input.mode(pc, PW_INPUT_DESKTOP);
        input.mouse_move(pc, 3.0, -2.0);
        input.mouse_position(pc, 0.0, 1.0);
        assert_eq!(host.take(), [Call::Position(0, i16::MAX)]);
    }

    #[test]
    fn a_mode_change_lets_go_of_held_buttons() {
        let (host, mut input) = (Host::default(), Input::default());
        let pc = Some((1, &host));
        input.send(pc, Device::Mouse, InputEvent::MouseButton { button: MouseButton::Left, down: true });
        host.take();
        input.mode(pc, PW_INPUT_DESKTOP);
        assert_eq!(host.take(), [Call::Button(MouseButton::Left, false)]);
        input.mode(pc, PW_INPUT_DESKTOP);
        assert_eq!(host.take(), []);
    }

    #[test]
    fn release_all_lets_go_of_keys_and_buttons() {
        let (host, mut input) = (Host::default(), Input::default());
        let pc = Some((1, &host));
        input.key(pc, Device::Keyboard, KEYBOARD_PAGE, A, true);
        input.send(pc, Device::Mouse, InputEvent::MouseButton { button: MouseButton::Right, down: true });
        host.take();
        input.release_all(pc);
        assert_eq!(host.take(), [Call::Key(0x41, false, 0), Call::Button(MouseButton::Right, false)]);
    }

    #[test]
    fn a_new_stream_starts_with_nothing_held() {
        let (old, new, mut input) = (Host::default(), Host::default(), Input::default());
        input.key(Some((1, &old)), Device::Keyboard, KEYBOARD_PAGE, SHIFT, true);
        // Shift is let go while no PC stream runs.
        input.key(XBOX, Device::Keyboard, KEYBOARD_PAGE, SHIFT, false);
        input.key(Some((2, &new)), Device::Keyboard, KEYBOARD_PAGE, A, true);
        assert_eq!(new.take(), [Call::Key(0x41, true, 0)]);
        input.release_all(Some((2, &new)));
        assert_eq!(new.take(), [Call::Key(0x41, false, 0)]);
    }

    #[test]
    fn without_a_pc_keys_and_mouse_become_the_xbox_pad() {
        let mut input = Input::default();
        input.key(XBOX, Device::Keyboard, KEYBOARD_PAGE, W, true);
        input.send(XBOX, Device::Mouse, InputEvent::MouseButton { button: MouseButton::Left, down: true });
        let mut physical = PwPad { axes: [100, 0, 200, 0], buttons: 16, lt: 9, rt: 0 };
        input.pad(&mut physical, 0.0);
        assert_eq!(physical.axes, [100, -i16::MAX, 200, 0]);
        assert_eq!(physical.buttons, 16);
        assert_eq!((physical.lt, physical.rt), (9, u16::MAX));
    }

    #[test]
    fn the_xbox_pad_turns_mouse_motion_into_the_right_stick_even_in_desktop_mode() {
        let mut input = Input::default();
        input.mode(XBOX, PW_INPUT_DESKTOP);
        input.pad(&mut pad(), 0.0);
        input.mouse_move(XBOX, 40.0, 0.0);
        let mut sample = pad();
        input.pad(&mut sample, 8.0);
        assert!(sample.axes[2] > 0, "{:?}", sample.axes);
    }

    #[test]
    fn release_all_lets_go_of_the_xbox_pad() {
        let mut input = Input::default();
        input.key(XBOX, Device::Keyboard, KEYBOARD_PAGE, W, true);
        input.release_all(XBOX);
        let mut sample = pad();
        input.pad(&mut sample, 0.0);
        assert_eq!(sample.axes, [0; 4]);
    }

    #[test]
    fn trackpad_tap_clicks_and_three_fingers_ask_for_the_keyboard() {
        let (host, mut input) = (Host::default(), Input::default());
        let pc = Some((1, &host));
        input.surface(pc, 1000.0, 1000.0);
        assert_eq!(input.touch(pc, touch(1, Phase::Began, 0.5, 0.5), 0.0), 0);
        input.touch(pc, touch(1, Phase::Ended, 0.5, 0.5), 50.0);
        assert_eq!(host.take(), [Call::Button(MouseButton::Left, true), Call::Button(MouseButton::Left, false)]);
        input.touch(pc, touch(1, Phase::Began, 0.1, 0.1), 100.0);
        input.touch(pc, touch(2, Phase::Began, 0.2, 0.1), 101.0);
        assert_eq!(input.touch(pc, touch(3, Phase::Began, 0.3, 0.1), 102.0), PW_INPUT_SHOW_KEYBOARD);
    }

    #[test]
    fn direct_touch_reaches_a_host_that_takes_touches() {
        let (host, mut input) = (Host::default(), Input::default());
        host.touch.set(true);
        let pc = Some((1, &host));
        input.mode(pc, PW_INPUT_DIRECT);
        input.touch(pc, touch(7, Phase::Began, 0.5, 0.5), 0.0);
        assert_eq!(host.take(), [Call::Touch(7, Phase::Began)]);
    }

    #[test]
    fn direct_touch_without_host_touch_presses_the_left_button_where_the_first_finger_is() {
        let (host, mut input) = (Host::default(), Input::default());
        let pc = Some((1, &host));
        input.mode(pc, PW_INPUT_DIRECT);
        input.touch(pc, touch(1, Phase::Began, 0.0, 0.0), 0.0);
        input.touch(pc, touch(2, Phase::Began, 1.0, 1.0), 0.0);
        input.touch(pc, touch(1, Phase::Moved, 1.0, 0.0), 0.0);
        input.touch(pc, touch(1, Phase::Ended, 1.0, 1.0), 0.0);
        assert_eq!(host.take(), [
            Call::Position(0, 0),
            Call::Button(MouseButton::Left, true),
            Call::Position(i16::MAX, 0),
            Call::Position(i16::MAX, i16::MAX),
            Call::Button(MouseButton::Left, false),
        ]);
    }

    #[test]
    fn release_all_lets_go_of_a_direct_touch_press() {
        let (host, mut input) = (Host::default(), Input::default());
        let pc = Some((1, &host));
        input.mode(pc, PW_INPUT_DIRECT);
        input.touch(pc, touch(1, Phase::Began, 0.5, 0.5), 0.0);
        host.take();
        input.release_all(pc);
        assert_eq!(host.take(), [Call::Button(MouseButton::Left, false)]);
        // The lifted finger after the release clicks nothing more.
        input.touch(pc, touch(1, Phase::Ended, 0.5, 0.5), 0.0);
        assert_eq!(host.take(), []);
    }

    #[test]
    fn a_drag_that_opens_a_new_stream_still_lets_go() {
        let (host, mut input) = (Host::default(), Input::default());
        input.surface(XBOX, 1000.0, 1000.0);
        let pc = Some((1, &host));
        input.touch(pc, touch(1, Phase::Began, 0.5, 0.5), 0.0);
        input.touch(pc, touch(1, Phase::Moved, 0.6, 0.5), 600.0);
        input.touch(pc, touch(1, Phase::Ended, 0.6, 0.5), 700.0);
        let calls = host.take();
        assert_eq!(calls.first(), Some(&Call::Button(MouseButton::Left, true)));
        assert_eq!(calls.last(), Some(&Call::Button(MouseButton::Left, false)));
    }

    #[test]
    fn leaving_direct_touch_cancels_the_touches_the_host_has_down() {
        let (host, mut input) = (Host::default(), Input::default());
        host.touch.set(true);
        let pc = Some((1, &host));
        input.mode(pc, PW_INPUT_DIRECT);
        input.touch(pc, touch(7, Phase::Began, 0.2, 0.3), 0.0);
        input.touch(pc, touch(8, Phase::Began, 0.4, 0.3), 0.0);
        input.touch(pc, touch(8, Phase::Ended, 0.4, 0.3), 0.0);
        host.take();
        input.mode(pc, PW_INPUT_TRACKPAD);
        assert_eq!(host.take(), [Call::Touch(7, Phase::Cancelled)]);
    }

    #[test]
    fn a_finger_cancelled_by_a_release_stays_off_the_host() {
        let (host, mut input) = (Host::default(), Input::default());
        host.touch.set(true);
        let pc = Some((1, &host));
        input.mode(pc, PW_INPUT_DIRECT);
        input.touch(pc, touch(7, Phase::Began, 0.2, 0.3), 0.0);
        input.release_all(pc);
        assert_eq!(host.take(), [Call::Touch(7, Phase::Began), Call::Touch(7, Phase::Cancelled)]);
        input.touch(pc, touch(7, Phase::Moved, 0.3, 0.3), 0.0);
        input.touch(pc, touch(7, Phase::Ended, 0.3, 0.3), 0.0);
        input.release_all(pc);
        assert_eq!(host.take(), []);
    }

    #[test]
    fn only_the_five_buttons_decode() {
        let decoded: Vec<_> = (0..=6).map(button).collect();
        assert_eq!(decoded, [
            Some(MouseButton::Left), Some(MouseButton::Right), Some(MouseButton::Middle),
            Some(MouseButton::X1), Some(MouseButton::X2), None, None,
        ]);
        assert_eq!(button(u32::MAX), None);
    }

    #[test]
    #[allow(clippy::arc_with_non_send_sync)]
    fn the_stream_is_looked_up_under_the_input_lock() {
        let input = Mutex::new(Input::default());
        let host = Arc::new(Host::default());
        let sent = locked(&input, || {
            assert!(input.try_lock().is_err(), "the stream was looked up before the lock");
            Some((1, host.clone()))
        }, |_, pc| pc.is_some());
        assert!(sent);
    }

    #[test]
    fn text_and_scroll_reach_the_pc() {
        let (host, mut input) = (Host::default(), Input::default());
        let pc = Some((1, &host));
        input.send(pc, Device::Panel, InputEvent::Text("hé".into()));
        input.send(pc, Device::Mouse, InputEvent::Scroll { dx: 0.0, dy: 1.0 });
        assert_eq!(host.take(), [Call::Text("hé".into()), Call::Scroll(120)]);
    }

    /// Without a PC stream the C calls drive the Xbox pad.
    #[test]
    fn c_calls_without_a_pc_drive_the_pad_and_ignore_bad_arguments() {
        let _global = global();
        route_started(0);
        for button in [5, 99, u32::MAX] { pw_input_mouse_button(PW_INPUT_SOURCE_MOUSE, button, true); }
        pw_input_mouse_button(7, PW_INPUT_BUTTON_LEFT, true);
        pw_input_key(PW_INPUT_SOURCE_KEYBOARD, 0x0C, W, true);
        pw_input_key(7, KEYBOARD_PAGE, W, true);
        assert_eq!(pw_input_touch(1, 99, 0.0, 0.0, 0.0, 0.0), 0);
        unsafe { pw_input_text(std::ptr::null()) };
        unsafe { pw_input_pad(std::ptr::null_mut(), 0.0) };
        let mut neutral = pad();
        unsafe { pw_input_pad(&mut neutral, 0.0) };
        assert_eq!((neutral.axes, neutral.buttons, neutral.lt, neutral.rt), ([0; 4], 0, 0, 0));
        pw_input_key(PW_INPUT_SOURCE_KEYBOARD, KEYBOARD_PAGE, W, true);
        pw_input_mouse_button(PW_INPUT_SOURCE_MOUSE, PW_INPUT_BUTTON_RIGHT, true);
        let mut sample = pad();
        unsafe { pw_input_pad(&mut sample, 0.0) };
        assert_eq!(sample.axes[1], -i16::MAX);
        assert_eq!(sample.lt, u16::MAX);
        assert!(!pw_input_host_supports_touch());
        pw_input_release_all();
        let mut sample = pad();
        unsafe { pw_input_pad(&mut sample, 1.0) };
        assert_eq!((sample.axes, sample.lt), ([0; 4], 0));
    }

    #[test]
    fn a_route_menu_shuts_the_gate_until_it_closes_or_a_new_route_starts() {
        let _global = global();
        let route = crate::pw_route_new();
        pw_input_key(PW_INPUT_SOURCE_KEYBOARD, KEYBOARD_PAGE, W, true);
        assert_eq!(unsafe { crate::pw_route_open(route) }, crate::PW_ROUTE_MENU_OPENED);
        let mut sample = pad();
        unsafe { pw_input_pad(&mut sample, 0.0) };
        assert_eq!(sample.axes, [0; 4], "the open menu did not let go of W");
        pw_input_key(PW_INPUT_SOURCE_KEYBOARD, KEYBOARD_PAGE, W, true);
        unsafe { pw_input_pad(&mut sample, 1.0) };
        assert_eq!(sample.axes, [0; 4], "a key reached the pad behind the menu");
        unsafe { crate::pw_route_close(route, false) };
        let mut sample = pad();
        unsafe { pw_input_pad(&mut sample, 2.0) };
        assert_eq!(sample.axes, [0; 4], "W pressed behind the menu was held after it");
        pw_input_key(PW_INPUT_SOURCE_KEYBOARD, KEYBOARD_PAGE, W, true);
        unsafe { pw_input_pad(&mut sample, 2.0) };
        assert_eq!(sample.axes[1], -i16::MAX, "the closed menu kept the gate shut");
        let w_reaches_the_pad = |now| {
            pw_input_release_all();
            pw_input_key(PW_INPUT_SOURCE_KEYBOARD, KEYBOARD_PAGE, W, true);
            let mut sample = pad();
            unsafe { pw_input_pad(&mut sample, now) };
            sample.axes[1] == -i16::MAX
        };
        // A stream stopped with its menu open leaves the next one open.
        unsafe { crate::pw_route_open(route) };
        let next = crate::pw_route_new();
        assert!(w_reaches_the_pad(3.0), "the new stream started with the gate shut");
        // The old route's menu no longer moves the gate.
        unsafe { crate::pw_route_open(route) };
        assert!(w_reaches_the_pad(4.0), "the old route's menu shut the new stream's gate");
        unsafe { crate::pw_route_open(next) };
        unsafe { crate::pw_route_close(route, false) };
        assert!(!w_reaches_the_pad(5.0), "the old route's close opened the new stream's gate");
        unsafe { crate::pw_route_close(next, false) };
        unsafe { crate::pw_route_free(route) };
        pw_input_release_all();
        unsafe { crate::pw_route_free(next) };
    }

    #[test]
    fn the_shut_gate_lets_go_once_and_drops_input() {
        let (host, mut input) = (Host::default(), Input::default());
        let pc = Some((1, &host));
        input.key(pc, Device::Keyboard, KEYBOARD_PAGE, A, true);
        host.take();
        input.menu(pc, true);
        input.menu(pc, true);
        assert_eq!(host.take(), [Call::Key(0x41, false, 0)]);
        input.key(pc, Device::Keyboard, KEYBOARD_PAGE, A, true);
        input.send(pc, Device::Mouse, InputEvent::MouseButton { button: MouseButton::Left, down: true });
        input.mouse_move(pc, 5.0, 0.0);
        input.mode(pc, PW_INPUT_DIRECT);
        input.touch(pc, touch(1, Phase::Began, 0.5, 0.5), 0.0);
        assert_eq!(host.take(), []);
        input.menu(pc, false);
        input.key(pc, Device::Keyboard, KEYBOARD_PAGE, A, true);
        assert_eq!(host.take(), [Call::Key(0x41, true, 0)]);
    }

    #[test]
    fn a_direct_touch_lift_leaves_the_held_mouse_button_down() {
        let (host, mut input) = (Host::default(), Input::default());
        let pc = Some((1, &host));
        input.mode(pc, PW_INPUT_DIRECT);
        input.send(pc, Device::Mouse, InputEvent::MouseButton { button: MouseButton::Left, down: true });
        input.touch(pc, touch(1, Phase::Began, 0.5, 0.5), 0.0);
        input.touch(pc, touch(1, Phase::Ended, 0.5, 0.5), 0.0);
        let left = |down| Call::Button(MouseButton::Left, down);
        assert_eq!(host.take(), [left(true), Call::Position(16384, 16384), left(true), Call::Position(16384, 16384)]);
        input.send(pc, Device::Mouse, InputEvent::MouseButton { button: MouseButton::Left, down: false });
        assert_eq!(host.take(), [left(false)]);
    }

    #[test]
    fn panel_keys_leave_the_physical_ctrl_down() {
        let (host, mut input) = (Host::default(), Input::default());
        let pc = Some((1, &host));
        const CTRL: u16 = 0xE0;
        input.key(pc, Device::Keyboard, KEYBOARD_PAGE, CTRL, true);
        for (usage, down) in [(CTRL, true), (0xE2, true), (0x4C, true), (0x4C, false), (0xE2, false), (CTRL, false)] {
            input.key(pc, Device::Panel, KEYBOARD_PAGE, usage, down);
        }
        assert!(!host.take().contains(&Call::Key(0xA2, false, 0)), "the host saw Ctrl let go");
        input.key(pc, Device::Keyboard, KEYBOARD_PAGE, CTRL, false);
        assert_eq!(host.take(), [Call::Key(0xA2, false, 0)]);
    }

    #[test]
    fn characters_give_their_usage_and_shift() {
        assert_eq!(pw_input_char_usage('c' as u32), 0x06);
        assert_eq!(pw_input_char_usage('?' as u32), 0x38 | PW_INPUT_CHAR_SHIFT);
        assert_eq!(pw_input_char_usage('é' as u32), 0);
        assert_eq!(pw_input_char_usage(0xD800), 0);
    }
}
