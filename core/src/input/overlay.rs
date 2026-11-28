//! The on-screen gamepad over the stream on a touch screen: its layout,
//! saved as JSON, and the gamepad state that fingers on it make. The
//! frontend only draws the layout and passes its fingers in; which control
//! a finger presses, how far a stick leans and which keys a PC gets are
//! worked out here, for every touch screen alike.

use super::pad::{axis, length, press, scaled, Target};
use super::{InputEvent, MouseButton, Phase, Usage};
use crate::route::Routed;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side { Left, Right }

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Control {
    /// Leans as far as the finger is from its centre.
    Stick { side: Side },
    /// Eight ways, by the finger's direction from its centre.
    Dpad,
    /// An analog trigger, pressed as hard as the finger presses.
    Trigger { side: Side },
    /// A pad button; a finger can slide from one button to the next.
    Button { target: Target },
    /// A keyboard key by its HID usage (page 0x07), for a PC.
    Key { usage: u16, label: String },
    Mouse { button: MouseButton },
}

impl Control {
    /// Buttons, keys and mouse buttons: a finger slides between them.
    fn slides(&self) -> bool {
        matches!(self, Control::Button { .. } | Control::Key { .. } | Control::Mouse { .. })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Element {
    #[serde(flatten)]
    pub control: Control,
    /// The centre, 0…1 across and down the surface.
    pub x: f32,
    pub y: f32,
    /// The diameter, a fraction of `Surface::unit`.
    pub size: f32,
}

/// Placements for each orientation: a phone held upright has its video at
/// the top and the pad below, one held sideways has the pad over the video.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Layout {
    /// 0.1…1 for the whole pad.
    pub opacity: f32,
    /// The part of a stick's radius, from its centre, that moves nothing.
    pub dead_zone: f32,
    pub portrait: Vec<Element>,
    pub landscape: Vec<Element>,
}

const MIN_SIZE: f32 = 0.05;
const MAX_SIZE: f32 = 0.6;
const MIN_OPACITY: f32 = 0.1;
const MAX_DEAD_ZONE: f32 = 0.9;
/// Within this part of its radius from the centre the d-pad presses nothing.
const DPAD_DEAD_ZONE: f32 = 0.2;
/// sin 22.5°: a direction is held within 67.5° of it, so the d-pad has
/// eight even sectors.
const DPAD_SECTOR: f32 = 0.383;

impl Default for Layout {
    /// A full Xbox pad: two sticks, the d-pad, A/B/X/Y, bumpers, triggers,
    /// stick clicks as buttons of their own, View, Guide and Menu.
    fn default() -> Self {
        Self {
            opacity: 0.6,
            dead_zone: 0.12,
            portrait: pad(&[
                (Spot::LeftTrigger, 0.09, 0.665, 0.14), (Spot::LeftBumper, 0.25, 0.665, 0.14),
                (Spot::View, 0.40, 0.665, 0.09), (Spot::Guide, 0.50, 0.665, 0.09), (Spot::Menu, 0.60, 0.665, 0.09),
                (Spot::RightBumper, 0.75, 0.665, 0.14), (Spot::RightTrigger, 0.91, 0.665, 0.14),
                (Spot::LeftStick, 0.19, 0.80, 0.32),
                (Spot::Y, 0.80, 0.733, 0.12), (Spot::X, 0.67, 0.80, 0.12),
                (Spot::B, 0.93, 0.80, 0.12), (Spot::A, 0.80, 0.867, 0.12),
                (Spot::Dpad, 0.37, 0.925, 0.24), (Spot::RightStick, 0.645, 0.925, 0.26),
                (Spot::LeftThumb, 0.07, 0.95, 0.09), (Spot::RightThumb, 0.93, 0.95, 0.09),
            ]),
            landscape: pad(&[
                // Below the stream menu button in the top left corner.
                (Spot::LeftTrigger, 0.07, 0.23, 0.15), (Spot::LeftBumper, 0.20, 0.23, 0.15),
                (Spot::View, 0.40, 0.19, 0.11), (Spot::Guide, 0.50, 0.19, 0.11), (Spot::Menu, 0.60, 0.19, 0.11),
                (Spot::RightBumper, 0.80, 0.23, 0.15), (Spot::RightTrigger, 0.93, 0.23, 0.15),
                (Spot::LeftStick, 0.14, 0.55, 0.40), (Spot::LeftThumb, 0.30, 0.42, 0.12),
                (Spot::Dpad, 0.32, 0.80, 0.30),
                (Spot::Y, 0.88, 0.375, 0.14), (Spot::X, 0.805, 0.52, 0.14),
                (Spot::B, 0.955, 0.52, 0.14), (Spot::A, 0.88, 0.665, 0.14),
                (Spot::RightStick, 0.68, 0.78, 0.34), (Spot::RightThumb, 0.54, 0.62, 0.12),
            ]),
        }
    }
}

/// The controls of the default pad.
#[derive(Clone, Copy)]
enum Spot {
    LeftStick, RightStick, Dpad, LeftTrigger, RightTrigger,
    A, B, X, Y, LeftBumper, RightBumper, LeftThumb, RightThumb, View, Menu, Guide,
}

fn pad(spots: &[(Spot, f32, f32, f32)]) -> Vec<Element> {
    spots.iter().map(|&(spot, x, y, size)| {
        let button = |target| Control::Button { target };
        let control = match spot {
            Spot::LeftStick => Control::Stick { side: Side::Left },
            Spot::RightStick => Control::Stick { side: Side::Right },
            Spot::Dpad => Control::Dpad,
            Spot::LeftTrigger => Control::Trigger { side: Side::Left },
            Spot::RightTrigger => Control::Trigger { side: Side::Right },
            Spot::A => button(Target::A),
            Spot::B => button(Target::B),
            Spot::X => button(Target::X),
            Spot::Y => button(Target::Y),
            Spot::LeftBumper => button(Target::LeftBumper),
            Spot::RightBumper => button(Target::RightBumper),
            Spot::LeftThumb => button(Target::LeftThumb),
            Spot::RightThumb => button(Target::RightThumb),
            Spot::View => button(Target::View),
            Spot::Menu => button(Target::Menu),
            Spot::Guide => button(Target::Guide),
        };
        Element { control, x, y, size }
    }).collect()
}

impl Layout {
    /// A saved layout, its values brought into range; the default pad for
    /// text that is no layout.
    pub fn from_json(text: &str) -> Self {
        serde_json::from_str::<Layout>(text).map(Layout::sanitized).unwrap_or_default()
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    fn sanitized(mut self) -> Self {
        let defaults = Layout::default();
        self.opacity = within(self.opacity, MIN_OPACITY, 1.0, defaults.opacity);
        self.dead_zone = within(self.dead_zone, 0.0, MAX_DEAD_ZONE, defaults.dead_zone);
        for element in self.portrait.iter_mut().chain(self.landscape.iter_mut()) {
            element.x = within(element.x, 0.0, 1.0, 0.5);
            element.y = within(element.y, 0.0, 1.0, 0.5);
            element.size = within(element.size, MIN_SIZE, MAX_SIZE, 0.15);
        }
        self
    }

    pub fn elements(&self, portrait: bool) -> &[Element] {
        if portrait { &self.portrait } else { &self.landscape }
    }

    /// Moves and resizes an element of a `surface` in points to the centre
    /// `x`, `y` (0…1) and diameter `size`, keeping all of it on the surface.
    pub fn place(&mut self, surface: Surface, index: usize, x: f32, y: f32, size: f32) {
        let list = if surface.portrait() { &mut self.portrait } else { &mut self.landscape };
        let Some(element) = list.get_mut(index) else { return };
        let size = within(size, MIN_SIZE, MAX_SIZE, element.size);
        let radius = size * surface.unit() / 2.0;
        let span = |value: f32, side: f32, current: f32| {
            let margin = if side > 0.0 { (radius / side).min(0.5) } else { 0.0 };
            within(value, margin, 1.0 - margin, current)
        };
        element.x = span(x, surface.width, element.x);
        element.y = span(y, surface.height, element.y);
        element.size = size;
    }

    pub fn set_opacity(&mut self, opacity: f32) {
        self.opacity = within(opacity, MIN_OPACITY, 1.0, self.opacity);
    }
}

/// `value` clamped to `min…max`, or `fallback` if it is no number.
fn within(value: f32, min: f32, max: f32, fallback: f32) -> f32 {
    if value.is_finite() { value.clamp(min, max) } else { fallback }
}

/// The area the pad covers, in points.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Surface {
    pub width: f32,
    pub height: f32,
}

/// The shorter side counts up to this many points, so a tablet does not
/// get a pad twice as large as a phone's.
const MAX_UNIT: f32 = 480.0;

impl Surface {
    pub fn portrait(self) -> bool { self.height > self.width }

    /// What `Element::size` is a fraction of.
    pub fn unit(self) -> f32 { self.width.min(self.height).min(MAX_UNIT).max(0.0) }

    fn center(self, element: &Element) -> (f32, f32) {
        (element.x * self.width, element.y * self.height)
    }

    fn radius(self, element: &Element) -> f32 { element.size * self.unit() / 2.0 }
}

/// How an element looks now, to draw it.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Look {
    pub pressed: bool,
    /// A stick's lean or the d-pad's direction, -1…1 each way, `y` down; a
    /// trigger's press 0…1 in `x`.
    pub x: f32,
    pub y: f32,
}

/// What a touch changed: keys and mouse buttons for the stream, and
/// whether a control was newly pressed, for a tap of haptic feedback.
#[derive(Debug, Default, PartialEq)]
pub struct Change {
    pub events: Vec<InputEvent>,
    pub haptic: bool,
}

#[derive(Clone, Copy, Debug)]
struct Finger {
    /// `None` while a finger that began on a button is off every button.
    element: Option<usize>,
    slides: bool,
    x: f32,
    y: f32,
    pressure: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Held {
    Key(u16),
    Mouse(MouseButton),
}

/// Fingers on a layout in, a gamepad state and PC keys out.
#[derive(Default)]
pub struct Overlay {
    layout: Layout,
    surface: Surface,
    fingers: BTreeMap<u32, Finger>,
    /// Buttons pressed since the last `sample`, so a tap between two
    /// samples still shows in one.
    latched: u16,
}

impl Overlay {
    pub fn new(layout: Layout) -> Self {
        Self { layout, ..Self::default() }
    }

    pub fn layout(&self) -> &Layout { &self.layout }

    /// A new layout lets go of every finger.
    pub fn set_layout(&mut self, layout: Layout) -> Vec<InputEvent> {
        let released = self.release_all();
        self.layout = layout;
        released
    }

    /// The surface in points; a new size lets go of every finger, as the
    /// elements under them move.
    pub fn set_surface(&mut self, surface: Surface) -> Vec<InputEvent> {
        if surface == self.surface { return Vec::new(); }
        let released = self.release_all();
        self.surface = surface;
        released
    }

    fn elements(&self) -> &[Element] { self.layout.elements(self.surface.portrait()) }

    /// The element at a point of the surface, the one whose centre is
    /// nearest for its size where elements overlap.
    pub fn hit(&self, x: f32, y: f32) -> Option<usize> {
        self.hit_where(x, y, |_| true)
    }

    fn hit_where(&self, x: f32, y: f32, take: impl Fn(&Control) -> bool) -> Option<usize> {
        let surface = self.surface;
        self.elements().iter().enumerate()
            .filter(|(_, element)| take(&element.control))
            .filter_map(|(index, element)| {
                let (cx, cy) = surface.center(element);
                let radius = surface.radius(element);
                let reach = (x - cx).hypot(y - cy) / radius;
                (radius > 0.0 && reach <= 1.0).then_some((index, reach))
            })
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(index, _)| index)
    }

    /// One finger, at a point of the surface. A finger belongs to the
    /// element it lands on until it lifts; one that lands on none, or was
    /// down when everything was let go, stays out.
    pub fn touch(&mut self, id: u32, phase: Phase, x: f32, y: f32, pressure: f32) -> Change {
        let before = (self.presses(), self.held());
        match phase {
            Phase::Began => {
                if let (false, Some(element)) = (self.fingers.contains_key(&id), self.hit(x, y)) {
                    let slides = self.elements()[element].control.slides();
                    self.fingers.insert(id, Finger { element: Some(element), slides, x, y, pressure });
                }
            }
            Phase::Moved => {
                let slid = self.hit_where(x, y, Control::slides);
                if let Some(finger) = self.fingers.get_mut(&id) {
                    (finger.x, finger.y, finger.pressure) = (x, y, pressure);
                    if finger.slides { finger.element = slid; }
                }
            }
            Phase::Ended | Phase::Cancelled => { self.fingers.remove(&id); }
        }
        let haptic = self.presses().difference(&before.0).next().is_some();
        self.latched |= self.state().buttons;
        Change { events: changes(&before.1, &self.held()), haptic }
    }

    /// Lets go of every finger: they stay out until they lift. Returns the
    /// releases of the keys and mouse buttons they held.
    pub fn release_all(&mut self) -> Vec<InputEvent> {
        let held = self.held();
        self.fingers.clear();
        self.latched = 0;
        changes(&held, &BTreeSet::new())
    }

    /// The state for the next packet: `state` with the buttons tapped since
    /// the last sample. Call it once each input tick.
    pub fn sample(&mut self) -> Routed {
        let mut routed = self.state();
        routed.buttons |= std::mem::take(&mut self.latched);
        routed
    }

    /// The gamepad the fingers hold: axes with `y` down, triggers 0…65535.
    pub fn state(&self) -> Routed {
        let targets = self.on().filter_map(|(_, element)| match element.control {
            Control::Button { target } => Some(target),
            _ => None,
        });
        let mut routed = press(targets.collect::<Vec<_>>());
        for (finger, element) in self.on() {
            match element.control {
                Control::Stick { side } => {
                    let (x, y) = self.stick(finger, element);
                    let first = if side == Side::Left { 0 } else { 2 };
                    routed.axes[first] = axis(x);
                    routed.axes[first + 1] = axis(y);
                }
                Control::Dpad => routed.buttons |= dpad(self.lean(finger, element)),
                Control::Trigger { side } => {
                    let value = (trigger(finger) * f32::from(u16::MAX)).round() as u16;
                    let trigger = if side == Side::Left { &mut routed.lt } else { &mut routed.rt };
                    *trigger = (*trigger).max(value);
                }
                _ => {}
            }
        }
        routed
    }

    /// How the element at `index` looks now.
    pub fn look(&self, index: usize) -> Look {
        let mut look = Look::default();
        for (finger, element) in self.on().filter(|(finger, _)| finger.element == Some(index)) {
            look.pressed = true;
            (look.x, look.y) = match element.control {
                Control::Stick { .. } => clamped(self.lean(finger, element)),
                Control::Dpad => {
                    let bits = dpad(self.lean(finger, element));
                    let way = |plus: Target, minus: Target| {
                        f32::from(bits & plus.bit() != 0) - f32::from(bits & minus.bit() != 0)
                    };
                    (way(Target::DpadRight, Target::DpadLeft), way(Target::DpadDown, Target::DpadUp))
                }
                Control::Trigger { .. } => (trigger(finger), 0.0),
                _ => (0.0, 0.0),
            };
        }
        look
    }

    /// The fingers on an element, with it.
    fn on(&self) -> impl Iterator<Item = (&Finger, &Element)> {
        let elements = self.elements();
        self.fingers.values().filter_map(|finger| Some((finger, elements.get(finger.element?)?)))
    }

    /// The finger's offset from the element's centre, in radii.
    fn lean(&self, finger: &Finger, element: &Element) -> (f32, f32) {
        let (cx, cy) = self.surface.center(element);
        let radius = self.surface.radius(element);
        if radius <= 0.0 { return (0.0, 0.0); }
        ((finger.x - cx) / radius, (finger.y - cy) / radius)
    }

    /// The stick's lean past the dead zone, the rest of the way spread over 0…1.
    fn stick(&self, finger: &Finger, element: &Element) -> (f32, f32) {
        let lean = self.lean(finger, element);
        let reach = length(lean).min(1.0);
        let dead = self.layout.dead_zone;
        let reach = if reach <= dead { 0.0 } else { (reach - dead) / (1.0 - dead) };
        scaled(lean, reach)
    }

    /// Each element held, with the d-pad's direction: a new entry is a new press.
    fn presses(&self) -> BTreeSet<(usize, u16)> {
        self.on().filter_map(|(finger, element)| {
            let direction = match element.control {
                Control::Dpad => dpad(self.lean(finger, element)),
                _ => 0,
            };
            Some((finger.element?, direction))
        }).collect()
    }

    fn held(&self) -> BTreeSet<Held> {
        self.on().filter_map(|(_, element)| match element.control {
            Control::Key { usage, .. } => Some(Held::Key(usage)),
            Control::Mouse { button } => Some(Held::Mouse(button)),
            _ => None,
        }).collect()
    }
}

/// `lean` cut to the unit circle.
fn clamped(lean: (f32, f32)) -> (f32, f32) {
    scaled(lean, length(lean).min(1.0))
}

/// A finger's press 0…1; full when it reports none.
fn trigger(finger: &Finger) -> f32 {
    if finger.pressure.is_finite() && finger.pressure > 0.0 { finger.pressure.min(1.0) } else { 1.0 }
}

/// The d-pad bits for a lean in radii from its centre.
fn dpad(lean: (f32, f32)) -> u16 {
    let reach = length(lean);
    if !(reach >= DPAD_DEAD_ZONE) { return 0; }
    let (x, y) = (lean.0 / reach, lean.1 / reach);
    [(y < -DPAD_SECTOR, Target::DpadUp), (y > DPAD_SECTOR, Target::DpadDown),
     (x < -DPAD_SECTOR, Target::DpadLeft), (x > DPAD_SECTOR, Target::DpadRight)]
        .into_iter().filter(|(held, _)| *held).fold(0, |bits, (_, target)| bits | target.bit())
}

/// Releases, then presses, from `before` to `after`.
fn changes(before: &BTreeSet<Held>, after: &BTreeSet<Held>) -> Vec<InputEvent> {
    let event = |held: &Held, down| match *held {
        Held::Key(usage) => InputEvent::Key { usage: Usage::Hid(usage), down },
        Held::Mouse(button) => InputEvent::MouseButton { button, down },
    };
    before.difference(after).map(|held| event(held, false))
        .chain(after.difference(before).map(|held| event(held, true)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::route::GUIDE;

    const PHONE_PORTRAIT: Surface = Surface { width: 402.0, height: 778.0 };
    const PHONE_LANDSCAPE: Surface = Surface { width: 750.0, height: 381.0 };
    const TABLET_PORTRAIT: Surface = Surface { width: 1032.0, height: 1300.0 };
    const TABLET_LANDSCAPE: Surface = Surface { width: 1376.0, height: 990.0 };
    const A: u16 = 16;
    const B: u16 = 32;

    fn overlay(surface: Surface) -> Overlay {
        let mut overlay = Overlay::new(Layout::default());
        overlay.set_surface(surface);
        overlay
    }

    /// The centre of the first element with `control`, in points.
    fn at(overlay: &Overlay, control: &Control) -> (f32, f32) {
        let element = overlay.elements().iter().find(|element| &element.control == control).expect("no such element");
        overlay.surface.center(element)
    }

    fn radius(overlay: &Overlay, control: &Control) -> f32 {
        let element = overlay.elements().iter().find(|element| &element.control == control).unwrap();
        overlay.surface.radius(element)
    }

    fn button(target: Target) -> Control { Control::Button { target } }
    const LEFT_STICK: Control = Control::Stick { side: Side::Left };

    #[test]
    fn the_default_pad_is_a_full_xbox_pad_both_ways() {
        let layout = Layout::default();
        for portrait in [true, false] {
            let controls: Vec<&Control> = layout.elements(portrait).iter().map(|element| &element.control).collect();
            let mut expected = vec![
                Control::Stick { side: Side::Left }, Control::Stick { side: Side::Right }, Control::Dpad,
                Control::Trigger { side: Side::Left }, Control::Trigger { side: Side::Right },
            ];
            expected.extend([Target::A, Target::B, Target::X, Target::Y, Target::LeftBumper, Target::RightBumper,
                Target::LeftThumb, Target::RightThumb, Target::View, Target::Menu, Target::Guide].map(button));
            for control in &expected {
                assert_eq!(controls.iter().filter(|c| **c == control).count(), 1, "{control:?} portrait {portrait}");
            }
            assert_eq!(controls.len(), expected.len(), "no keys or mouse buttons by default");
        }
    }

    #[test]
    fn default_elements_stay_apart_and_on_the_surface() {
        for surface in [PHONE_PORTRAIT, PHONE_LANDSCAPE, TABLET_PORTRAIT, TABLET_LANDSCAPE] {
            let overlay = overlay(surface);
            let elements = overlay.elements();
            for (i, a) in elements.iter().enumerate() {
                let ((x, y), r) = (surface.center(a), surface.radius(a));
                assert!(x - r >= 0.0 && x + r <= surface.width && y - r >= 0.0 && y + r <= surface.height,
                    "{:?} leaves {surface:?}", a.control);
                for b in &elements[i + 1..] {
                    let ((bx, by), br) = (surface.center(b), surface.radius(b));
                    assert!((x - bx).hypot(y - by) >= r + br, "{:?} overlaps {:?} on {surface:?}", a.control, b.control);
                }
            }
        }
    }

    #[test]
    fn a_layout_survives_json() {
        let mut layout = Layout::default();
        layout.landscape.push(Element {
            control: Control::Key { usage: 0x29, label: "Esc".into() }, x: 0.5, y: 0.5, size: 0.1,
        });
        layout.landscape.push(Element { control: Control::Mouse { button: MouseButton::Right }, x: 0.5, y: 0.3, size: 0.1 });
        let json = layout.to_json();
        assert!(json.contains(r#"{"type":"button","target":"A","x":"#), "{json}");
        assert!(json.contains(r#"{"type":"key","usage":41,"label":"Esc","#), "{json}");
        assert!(json.contains(r#"{"type":"mouse","button":"Right","#), "{json}");
        assert!(json.contains(r#"{"type":"stick","side":"left","#), "{json}");
        assert_eq!(Layout::from_json(&json), layout);
    }

    #[test]
    fn a_broken_layout_falls_back_or_is_brought_into_range() {
        assert_eq!(Layout::from_json("not json"), Layout::default());
        assert_eq!(Layout::from_json(r#"{"portrait": 3}"#), Layout::default());
        let partial = Layout::from_json(r#"{"opacity": 0.3}"#);
        assert_eq!(partial.opacity, 0.3);
        assert_eq!(partial.landscape, Layout::default().landscape, "missing fields take the default");
        let wild = Layout::from_json(r#"{"opacity": 7, "dead_zone": -1,
            "portrait": [{"type":"dpad","x":-3,"y":9,"size":40}], "landscape": []}"#);
        assert_eq!((wild.opacity, wild.dead_zone), (1.0, 0.0));
        assert_eq!(wild.portrait, vec![Element { control: Control::Dpad, x: 0.0, y: 1.0, size: MAX_SIZE }]);
        assert!(wild.landscape.is_empty());
    }

    #[test]
    fn a_finger_presses_the_button_it_lands_on() {
        let mut overlay = overlay(PHONE_LANDSCAPE);
        let (x, y) = at(&overlay, &button(Target::A));
        let change = overlay.touch(1, Phase::Began, x, y, 1.0);
        assert!(change.haptic && change.events.is_empty());
        assert_eq!(overlay.state().buttons, A);
        overlay.touch(1, Phase::Ended, x, y, 1.0);
        assert_eq!(overlay.state(), Routed { axes: [0; 4], buttons: 0, lt: 0, rt: 0 });
    }

    #[test]
    fn a_tap_between_two_samples_shows_in_one() {
        let mut overlay = overlay(PHONE_LANDSCAPE);
        let (x, y) = at(&overlay, &button(Target::A));
        overlay.touch(1, Phase::Began, x, y, 1.0);
        overlay.touch(1, Phase::Ended, x, y, 1.0);
        assert_eq!(overlay.state().buttons, 0);
        assert_eq!(overlay.sample().buttons, A);
        assert_eq!(overlay.sample().buttons, 0, "only once");
        overlay.touch(2, Phase::Began, x, y, 1.0);
        overlay.release_all();
        assert_eq!(overlay.sample().buttons, 0, "letting go forgets the tap");
    }

    #[test]
    fn guide_view_and_menu_and_stick_clicks_press_their_bits() {
        let mut overlay = overlay(PHONE_PORTRAIT);
        for (id, target) in [Target::Guide, Target::View, Target::Menu, Target::LeftThumb, Target::RightThumb]
            .into_iter().enumerate()
        {
            let (x, y) = at(&overlay, &button(target));
            overlay.touch(id as u32, Phase::Began, x, y, 1.0);
        }
        assert_eq!(overlay.state().buttons, GUIDE | 8 | 4 | 16384 | 32768);
    }

    #[test]
    fn a_finger_off_every_element_stays_out_even_on_one_later() {
        let mut overlay = overlay(PHONE_LANDSCAPE);
        assert_eq!(overlay.hit(375.0, 260.0), None, "the middle of the video is free");
        overlay.touch(1, Phase::Began, 375.0, 260.0, 1.0);
        let (x, y) = at(&overlay, &button(Target::A));
        let change = overlay.touch(1, Phase::Moved, x, y, 1.0);
        assert!(!change.haptic);
        assert_eq!(overlay.state().buttons, 0);
    }

    #[test]
    fn a_finger_slides_from_button_to_button_and_off() {
        let mut overlay = overlay(PHONE_LANDSCAPE);
        let (ax, ay) = at(&overlay, &button(Target::A));
        let (bx, by) = at(&overlay, &button(Target::B));
        overlay.touch(1, Phase::Began, ax, ay, 1.0);
        let change = overlay.touch(1, Phase::Moved, bx, by, 1.0);
        assert!(change.haptic, "a slide onto B presses it");
        assert_eq!(overlay.state().buttons, B);
        overlay.touch(1, Phase::Moved, 375.0, 260.0, 1.0);
        assert_eq!(overlay.state().buttons, 0, "off every button nothing is held");
        overlay.touch(1, Phase::Moved, ax, ay, 1.0);
        assert_eq!(overlay.state().buttons, A, "the finger is still followed");
    }

    #[test]
    fn a_button_finger_does_not_slide_onto_a_stick() {
        let mut overlay = overlay(PHONE_LANDSCAPE);
        let (ax, ay) = at(&overlay, &button(Target::RightThumb));
        let (sx, sy) = at(&overlay, &Control::Stick { side: Side::Right });
        overlay.touch(1, Phase::Began, ax, ay, 1.0);
        overlay.touch(1, Phase::Moved, sx + 30.0, sy, 1.0);
        assert_eq!(overlay.state(), Routed { axes: [0; 4], buttons: 0, lt: 0, rt: 0 });
    }

    #[test]
    fn a_stick_leans_past_its_dead_zone_and_keeps_its_finger() {
        let mut overlay = overlay(PHONE_LANDSCAPE);
        let (x, y) = at(&overlay, &LEFT_STICK);
        let r = radius(&overlay, &LEFT_STICK);
        overlay.touch(1, Phase::Began, x, y, 1.0);
        assert_eq!(overlay.state().axes, [0; 4], "the centre is still");
        overlay.touch(1, Phase::Moved, x + r * 0.1, y, 1.0);
        assert_eq!(overlay.state().axes, [0; 4], "within the dead zone");
        overlay.touch(1, Phase::Moved, x + r * 0.56, y, 1.0);
        let axes = overlay.state().axes;
        assert!((axes[0] - 16384).abs() <= 1 && axes[1..] == [0; 3], "(0.56 - 0.12) / 0.88 is half way: {axes:?}");
        overlay.touch(1, Phase::Moved, x, y - r * 3.0, 1.0);
        assert_eq!(overlay.state().axes, [0, -32767, 0, 0], "beyond its edge, up is full and negative");
        let look = overlay.look(overlay.hit(x, y).unwrap());
        assert!(look.pressed && look.x.abs() < 1e-6 && (look.y + 1.0).abs() < 1e-6, "{look:?}");
    }

    #[test]
    fn fingers_on_several_controls_hold_them_together() {
        let mut overlay = overlay(PHONE_PORTRAIT);
        let (sx, sy) = at(&overlay, &LEFT_STICK);
        let r = radius(&overlay, &LEFT_STICK);
        let (ax, ay) = at(&overlay, &button(Target::A));
        let (tx, ty) = at(&overlay, &Control::Trigger { side: Side::Right });
        let (rx, ry) = at(&overlay, &Control::Stick { side: Side::Right });
        overlay.touch(1, Phase::Began, sx, sy, 1.0);
        overlay.touch(1, Phase::Moved, sx - r, sy, 1.0);
        overlay.touch(2, Phase::Began, ax, ay, 1.0);
        overlay.touch(3, Phase::Began, tx, ty, 1.0);
        overlay.touch(4, Phase::Began, rx, ry, 1.0);
        let r2 = radius(&overlay, &Control::Stick { side: Side::Right });
        overlay.touch(4, Phase::Moved, rx, ry + r2, 1.0);
        assert_eq!(overlay.state(), Routed { axes: [-32767, 0, 0, 32767], buttons: A, lt: 0, rt: u16::MAX });
        overlay.touch(2, Phase::Ended, ax, ay, 1.0);
        assert_eq!(overlay.state(), Routed { axes: [-32767, 0, 0, 32767], buttons: 0, lt: 0, rt: u16::MAX });
        overlay.touch(1, Phase::Cancelled, sx - r, sy, 1.0);
        assert_eq!(overlay.state(), Routed { axes: [0, 0, 0, 32767], buttons: 0, lt: 0, rt: u16::MAX });
    }

    #[test]
    fn the_dpad_has_eight_ways_and_a_still_centre() {
        let mut overlay = overlay(PHONE_LANDSCAPE);
        let (x, y) = at(&overlay, &Control::Dpad);
        let r = radius(&overlay, &Control::Dpad);
        let index = overlay.hit(x, y).unwrap();
        overlay.touch(1, Phase::Began, x, y, 1.0);
        assert_eq!(overlay.state().buttons, 0, "the centre presses nothing");
        let change = overlay.touch(1, Phase::Moved, x, y - r * 0.8, 1.0);
        assert!(change.haptic, "a new direction is a new press");
        assert_eq!(overlay.state().buttons, 256);
        overlay.touch(1, Phase::Moved, x + r * 0.6, y - r * 0.6, 1.0);
        assert_eq!(overlay.state().buttons, 256 | 2048, "up and right");
        assert_eq!(overlay.look(index), Look { pressed: true, x: 1.0, y: -1.0 });
        overlay.touch(1, Phase::Moved, x - r * 2.0, y + r * 0.2, 1.0);
        assert_eq!(overlay.state().buttons, 1024, "left, past the edge");
        let change = overlay.touch(1, Phase::Moved, x - r * 2.0, y + r * 0.3, 1.0);
        assert!(!change.haptic, "the same direction is no new press");
    }

    #[test]
    fn a_trigger_is_an_axis_pressed_as_hard_as_the_finger() {
        let mut overlay = overlay(PHONE_LANDSCAPE);
        let (x, y) = at(&overlay, &Control::Trigger { side: Side::Left });
        overlay.touch(1, Phase::Began, x, y, 0.5);
        assert_eq!(overlay.state().lt, 32768);
        overlay.touch(1, Phase::Moved, x, y, 0.0);
        assert_eq!(overlay.state().lt, u16::MAX, "no force reading presses fully");
        let index = overlay.hit(x, y).unwrap();
        assert_eq!(overlay.look(index), Look { pressed: true, x: 1.0, y: 0.0 });
    }

    fn with_keys() -> Overlay {
        let mut layout = Layout::default();
        layout.landscape = vec![
            Element { control: Control::Key { usage: 0x29, label: "Esc".into() }, x: 0.2, y: 0.5, size: 0.2 },
            Element { control: Control::Mouse { button: MouseButton::Left }, x: 0.5, y: 0.5, size: 0.2 },
        ];
        let mut overlay = Overlay::new(layout);
        overlay.set_surface(PHONE_LANDSCAPE);
        overlay
    }

    #[test]
    fn keys_and_mouse_buttons_go_down_once_and_up_with_the_last_finger() {
        let mut overlay = with_keys();
        let escape = |down| InputEvent::Key { usage: Usage::Hid(0x29), down };
        let (ex, ey) = (150.0, 190.5);
        assert_eq!(overlay.touch(1, Phase::Began, ex, ey, 1.0).events, vec![escape(true)]);
        assert_eq!(overlay.touch(2, Phase::Began, ex, ey, 1.0).events, vec![], "a second finger presses nothing new");
        assert_eq!(overlay.touch(1, Phase::Ended, ex, ey, 1.0).events, vec![]);
        // The finger slides from Esc to the left button.
        let change = overlay.touch(2, Phase::Moved, 375.0, ey, 1.0);
        assert_eq!(change.events, vec![escape(false), InputEvent::MouseButton { button: MouseButton::Left, down: true }]);
        assert_eq!(overlay.state().buttons, 0, "keys are no pad buttons");
        assert_eq!(overlay.touch(2, Phase::Ended, 375.0, ey, 1.0).events,
            vec![InputEvent::MouseButton { button: MouseButton::Left, down: false }]);
    }

    #[test]
    fn after_letting_go_fingers_still_down_stay_out() {
        let mut overlay = with_keys();
        overlay.touch(1, Phase::Began, 150.0, 190.5, 1.0);
        assert_eq!(overlay.release_all(), vec![InputEvent::Key { usage: Usage::Hid(0x29), down: false }]);
        assert_eq!(overlay.touch(1, Phase::Moved, 150.0, 190.5, 1.0), Change::default());
        assert_eq!(overlay.touch(1, Phase::Ended, 150.0, 190.5, 1.0), Change::default());
        assert_eq!(overlay.touch(1, Phase::Began, 150.0, 190.5, 1.0).events.len(), 1, "a new touch counts");
    }

    #[test]
    fn a_new_surface_or_layout_lets_go() {
        let mut overlay = with_keys();
        overlay.touch(1, Phase::Began, 150.0, 190.5, 1.0);
        assert_eq!(overlay.set_surface(PHONE_LANDSCAPE), vec![], "the same size keeps the fingers");
        assert_eq!(overlay.set_surface(PHONE_PORTRAIT).len(), 1);
        let mut overlay = with_keys();
        overlay.touch(1, Phase::Began, 150.0, 190.5, 1.0);
        assert_eq!(overlay.set_layout(Layout::default()).len(), 1);
        assert_eq!(overlay.state().buttons, 0);
    }

    #[test]
    fn the_portrait_layout_is_used_on_an_upright_surface() {
        let overlay = overlay(PHONE_PORTRAIT);
        assert_eq!(overlay.elements(), Layout::default().portrait.as_slice());
        let (x, y) = at(&overlay, &button(Target::A));
        assert!(y > PHONE_PORTRAIT.height * 0.6, "the pad is below the video");
        assert!(overlay.hit(x, y).is_some());
    }

    #[test]
    fn a_tablet_pad_is_no_larger_than_on_a_large_phone() {
        assert_eq!(TABLET_LANDSCAPE.unit(), MAX_UNIT);
        assert_eq!(PHONE_LANDSCAPE.unit(), 381.0);
    }

    #[test]
    fn placing_keeps_an_element_whole_on_the_surface() {
        let mut layout = Layout::default();
        let size = 0.2;
        layout.place(PHONE_LANDSCAPE, 0, 0.0, 2.0, size);
        let element = &layout.landscape[0];
        let radius = size * PHONE_LANDSCAPE.unit() / 2.0;
        assert!((element.x - radius / PHONE_LANDSCAPE.width).abs() < 1e-6, "{element:?}");
        assert!((element.y - (1.0 - radius / PHONE_LANDSCAPE.height)).abs() < 1e-6, "{element:?}");
        assert_eq!(element.size, size);
        layout.place(PHONE_LANDSCAPE, 0, 0.5, 0.5, 9.0);
        assert_eq!(layout.landscape[0].size, MAX_SIZE);
        layout.place(PHONE_LANDSCAPE, 0, f32::NAN, 0.5, f32::NAN);
        assert_eq!(layout.landscape[0].size, MAX_SIZE, "no number keeps the size");
        assert_eq!(layout.portrait, Layout::default().portrait, "the other orientation stays");
        layout.place(PHONE_LANDSCAPE, 99, 0.5, 0.5, 0.2);
        layout.set_opacity(0.0);
        assert_eq!(layout.opacity, MIN_OPACITY);
    }
}
