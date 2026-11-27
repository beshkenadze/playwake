//! A touch screen used as a laptop trackpad, with Moonlight's gestures:
//! - one finger moves the cursor;
//! - a tap clicks the left button;
//! - a second finger tapped while the first rests clicks the right button;
//! - a finger held about half a second and then moved drags with the left
//!   button down;
//! - two fingers moved together scroll;
//! - a third finger asks for the on-screen keyboard.
//!
//! Pure logic: the time comes with every touch, in milliseconds.

use super::{InputEvent, MouseButton, Phase};

/// A finger held this long before it moves drags instead of moving.
const HOLD_MS: f64 = 500.0;
/// How far, in points, fingers may wander and still tap or hold.
const SLOP: f32 = 10.0;
/// Two-finger travel, in points, for one wheel notch.
const POINTS_PER_NOTCH: f32 = 20.0;

#[derive(Clone, Debug, PartialEq)]
pub enum Gesture {
    Input(InputEvent),
    /// Three fingers: show the on-screen keyboard.
    Keyboard,
}

#[derive(Debug)]
enum State {
    Idle,
    One { id: u32, since: f64, start: Point, reported: Point, moved: bool, dragging: bool },
    /// `origins`: where each finger was when the second came down. Any of
    /// them wandering off spoils the tap; the centroid scrolls.
    Two { origins: Vec<(u32, Point)>, start: Point, reported: Point, scrolled: bool, wandered: bool },
    /// The gesture is over; the fingers left on the screen do nothing.
    Spent,
}

type Point = (f32, f32);

pub struct Trackpad {
    size: Point,
    fingers: Vec<(u32, Point)>,
    state: State,
}

impl Trackpad {
    /// `width` and `height`: the touch surface in points. The cursor moves
    /// one host pixel per point the finger travels.
    pub fn new(width: f32, height: f32) -> Self {
        Self { size: (width, height), fingers: Vec::new(), state: State::Idle }
    }

    /// One touch, `x` and `y` 0…1 over the surface, `now` in milliseconds.
    /// `Ended` must carry the finger's last point: it counts as a move.
    pub fn touch(&mut self, id: u32, phase: Phase, x: f32, y: f32, now: f64) -> Vec<Gesture> {
        let at = (x * self.size.0, y * self.size.1);
        let mut out = Vec::new();
        match phase {
            Phase::Began => self.began(id, at, now, &mut out),
            Phase::Moved => self.moved(id, at, now, true, &mut out),
            Phase::Ended => {
                self.moved(id, at, now, false, &mut out);
                self.ended(id, &mut out);
            }
            Phase::Cancelled => {
                if self.fingers.iter().any(|finger| finger.0 == id) {
                    // An extra finger on a drag leaves the drag alone.
                    if !matches!(self.state, State::One { id: primary, .. } if primary != id) {
                        self.release(&mut out);
                    }
                    self.lift(id);
                }
            }
        }
        out
    }

    /// Ends the gesture at once, as when the stream loses focus: a drag lets
    /// go of the button, and nothing clicks.
    pub fn reset(&mut self) -> Vec<Gesture> {
        let mut out = Vec::new();
        self.release(&mut out);
        self.fingers.clear();
        self.state = State::Idle;
        out
    }

    fn began(&mut self, id: u32, at: Point, now: f64, out: &mut Vec<Gesture>) {
        if self.fingers.iter().any(|finger| finger.0 == id) { return; }
        self.fingers.push((id, at));
        match (&self.state, self.fingers.len()) {
            (State::Idle, 1) => {
                self.state = State::One { id, since: now, start: at, reported: at, moved: false, dragging: false };
            }
            (State::One { dragging: false, .. }, 2) => {
                let start = self.centroid();
                self.state = State::Two {
                    origins: self.fingers.clone(), start, reported: start, scrolled: false, wandered: false,
                };
            }
            (State::Two { .. }, 3) => {
                out.push(Gesture::Keyboard);
                self.state = State::Spent;
            }
            _ => {}
        }
    }

    /// `may_drag`: false for the lift, which finishes a drag but starts none.
    fn moved(&mut self, id: u32, at: Point, now: f64, may_drag: bool, out: &mut Vec<Gesture>) {
        let Some(finger) = self.fingers.iter_mut().find(|finger| finger.0 == id) else { return };
        if finger.1 == at { return; }
        finger.1 = at;
        let centroid = self.centroid();
        match &mut self.state {
            State::One { id: primary, since, start, reported, moved, dragging } if *primary == id => {
                if !*moved && !*dragging {
                    if may_drag && now - *since >= HOLD_MS {
                        *dragging = true;
                        out.push(button(MouseButton::Left, true));
                    } else if distance(at, *start) > SLOP {
                        *moved = true;
                    }
                }
                if *moved || *dragging {
                    let (dx, dy) = (at.0 - reported.0, at.1 - reported.1);
                    *reported = at;
                    out.push(Gesture::Input(InputEvent::MouseMove { dx, dy }));
                }
            }
            State::Two { origins, start, reported, scrolled, wandered } => {
                if origins.iter().any(|origin| origin.0 == id && distance(origin.1, at) > SLOP) { *wandered = true; }
                if !*scrolled && distance(centroid, *start) > SLOP { *scrolled = true; }
                if *scrolled {
                    let (dx, dy) = (centroid.0 - reported.0, centroid.1 - reported.1);
                    *reported = centroid;
                    // Natural scrolling: the content follows the fingers.
                    out.push(Gesture::Input(InputEvent::Scroll {
                        dx: -dx / POINTS_PER_NOTCH,
                        dy: dy / POINTS_PER_NOTCH,
                    }));
                }
            }
            _ => {}
        }
    }

    fn ended(&mut self, id: u32, out: &mut Vec<Gesture>) {
        if !self.fingers.iter().any(|finger| finger.0 == id) { return; }
        match self.state {
            State::One { id: primary, moved, dragging, .. } if primary == id => {
                if dragging {
                    out.push(button(MouseButton::Left, false));
                } else if !moved {
                    out.extend([button(MouseButton::Left, true), button(MouseButton::Left, false)]);
                }
                self.state = State::Spent;
            }
            State::Two { scrolled, wandered, .. } => {
                if !scrolled && !wandered {
                    out.extend([button(MouseButton::Right, true), button(MouseButton::Right, false)]);
                }
                self.state = State::Spent;
            }
            _ => {}
        }
        self.lift(id);
    }

    /// Lets go of a dragged button and spends the gesture.
    fn release(&mut self, out: &mut Vec<Gesture>) {
        if let State::One { dragging: true, .. } = self.state {
            out.push(button(MouseButton::Left, false));
        }
        if !matches!(self.state, State::Idle) { self.state = State::Spent; }
    }

    fn lift(&mut self, id: u32) {
        self.fingers.retain(|finger| finger.0 != id);
        if self.fingers.is_empty() { self.state = State::Idle; }
    }

    fn centroid(&self) -> Point {
        let count = self.fingers.len().max(1) as f32;
        let sum = self.fingers.iter().fold((0.0, 0.0), |sum, finger| (sum.0 + finger.1.0, sum.1 + finger.1.1));
        (sum.0 / count, sum.1 / count)
    }
}

fn button(button: MouseButton, down: bool) -> Gesture {
    Gesture::Input(InputEvent::MouseButton { button, down })
}

fn distance(a: Point, b: Point) -> f32 {
    (a.0 - b.0).hypot(a.1 - b.1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pad() -> Trackpad { Trackpad::new(1024.0, 512.0) }

    /// A touch at `x`, `y` in points, so the deltas compare exactly.
    fn touch(pad: &mut Trackpad, id: u32, phase: Phase, x: f32, y: f32, now: f64) -> Vec<Gesture> {
        pad.touch(id, phase, x / 1024.0, y / 512.0, now)
    }

    fn moved(dx: f32, dy: f32) -> Gesture { Gesture::Input(InputEvent::MouseMove { dx, dy }) }
    fn scroll(dx: f32, dy: f32) -> Gesture { Gesture::Input(InputEvent::Scroll { dx, dy }) }
    fn click(which: MouseButton) -> Vec<Gesture> { vec![button(which, true), button(which, false)] }

    #[test]
    fn a_tap_clicks_left() {
        let mut pad = pad();
        assert_eq!(touch(&mut pad, 1, Phase::Began, 500.0, 250.0, 0.0), vec![]);
        assert_eq!(touch(&mut pad, 1, Phase::Moved, 502.0, 250.0, 50.0), vec![], "a wobble inside the slop");
        assert_eq!(touch(&mut pad, 1, Phase::Ended, 502.0, 250.0, 120.0), click(MouseButton::Left));
    }

    #[test]
    fn a_held_finger_lifted_without_moving_still_clicks() {
        let mut pad = pad();
        touch(&mut pad, 1, Phase::Began, 500.0, 250.0, 0.0);
        assert_eq!(touch(&mut pad, 1, Phase::Ended, 500.0, 250.0, 900.0), click(MouseButton::Left));
    }

    #[test]
    fn a_moving_finger_moves_the_cursor_and_does_not_click() {
        let mut pad = pad();
        touch(&mut pad, 1, Phase::Began, 500.0, 250.0, 0.0);
        assert_eq!(touch(&mut pad, 1, Phase::Moved, 520.0, 250.0, 30.0), vec![moved(20.0, 0.0)]);
        assert_eq!(touch(&mut pad, 1, Phase::Moved, 520.0, 270.0, 60.0), vec![moved(0.0, 20.0)]);
        assert_eq!(touch(&mut pad, 1, Phase::Moved, 521.0, 270.0, 900.0), vec![moved(1.0, 0.0)], "no drag once moving");
        assert_eq!(touch(&mut pad, 1, Phase::Ended, 521.0, 270.0, 950.0), vec![]);
    }

    #[test]
    fn hold_then_move_drags() {
        let mut pad = pad();
        touch(&mut pad, 1, Phase::Began, 500.0, 250.0, 0.0);
        assert_eq!(touch(&mut pad, 1, Phase::Moved, 503.0, 250.0, 600.0),
            vec![button(MouseButton::Left, true), moved(3.0, 0.0)]);
        assert_eq!(touch(&mut pad, 1, Phase::Moved, 600.0, 250.0, 700.0), vec![moved(97.0, 0.0)]);
        assert_eq!(touch(&mut pad, 1, Phase::Ended, 600.0, 250.0, 800.0), vec![button(MouseButton::Left, false)]);
    }

    #[test]
    fn cancel_mid_drag_lets_go_and_never_clicks() {
        let mut pad = pad();
        touch(&mut pad, 1, Phase::Began, 500.0, 250.0, 0.0);
        touch(&mut pad, 1, Phase::Moved, 550.0, 250.0, 600.0);
        assert_eq!(touch(&mut pad, 1, Phase::Cancelled, 550.0, 250.0, 650.0), vec![button(MouseButton::Left, false)]);
        assert_eq!(touch(&mut pad, 1, Phase::Ended, 550.0, 250.0, 660.0), vec![], "the finger is gone");
    }

    #[test]
    fn cancel_of_a_tap_never_clicks() {
        let mut pad = pad();
        touch(&mut pad, 1, Phase::Began, 500.0, 250.0, 0.0);
        assert_eq!(touch(&mut pad, 1, Phase::Cancelled, 500.0, 250.0, 50.0), vec![]);
        touch(&mut pad, 2, Phase::Began, 500.0, 250.0, 100.0);
        assert_eq!(touch(&mut pad, 2, Phase::Ended, 500.0, 250.0, 150.0), click(MouseButton::Left), "the next gesture is fresh");
    }

    #[test]
    fn second_finger_tap_clicks_right_even_after_a_long_rest() {
        let mut pad = pad();
        touch(&mut pad, 1, Phase::Began, 500.0, 250.0, 0.0);
        assert_eq!(touch(&mut pad, 2, Phase::Began, 600.0, 250.0, 2000.0), vec![]);
        assert_eq!(touch(&mut pad, 2, Phase::Ended, 600.0, 250.0, 2100.0), click(MouseButton::Right));
        assert_eq!(touch(&mut pad, 1, Phase::Moved, 700.0, 350.0, 2200.0), vec![], "the resting finger does not jump the cursor");
        assert_eq!(touch(&mut pad, 1, Phase::Ended, 700.0, 350.0, 2300.0), vec![], "and does not click left");
    }

    #[test]
    fn two_fingers_scroll_and_do_not_click() {
        let mut pad = pad();
        touch(&mut pad, 1, Phase::Began, 400.0, 250.0, 0.0);
        touch(&mut pad, 2, Phase::Began, 600.0, 250.0, 10.0);
        assert_eq!(touch(&mut pad, 1, Phase::Moved, 400.0, 290.0, 30.0), vec![scroll(0.0, 1.0)], "centroid 20 points down");
        assert_eq!(touch(&mut pad, 2, Phase::Moved, 600.0, 290.0, 40.0), vec![scroll(0.0, 1.0)]);
        assert_eq!(touch(&mut pad, 2, Phase::Moved, 640.0, 290.0, 50.0), vec![scroll(-1.0, 0.0)]);
        assert_eq!(touch(&mut pad, 1, Phase::Ended, 400.0, 290.0, 60.0), vec![]);
        assert_eq!(touch(&mut pad, 2, Phase::Moved, 900.0, 450.0, 70.0), vec![]);
        assert_eq!(touch(&mut pad, 2, Phase::Ended, 900.0, 450.0, 80.0), vec![]);
    }

    #[test]
    fn a_second_finger_cancels_the_hold() {
        let mut pad = pad();
        touch(&mut pad, 1, Phase::Began, 500.0, 250.0, 0.0);
        touch(&mut pad, 2, Phase::Began, 600.0, 250.0, 100.0);
        assert_eq!(touch(&mut pad, 1, Phase::Moved, 502.0, 250.0, 900.0), vec![], "no drag after the hold time");
        assert_eq!(touch(&mut pad, 1, Phase::Ended, 502.0, 250.0, 950.0), click(MouseButton::Right));
        assert_eq!(touch(&mut pad, 2, Phase::Ended, 600.0, 250.0, 960.0), vec![], "no left click afterwards");
    }

    #[test]
    fn extra_fingers_do_not_break_a_drag() {
        let mut pad = pad();
        touch(&mut pad, 1, Phase::Began, 500.0, 250.0, 0.0);
        touch(&mut pad, 1, Phase::Moved, 510.0, 250.0, 600.0);
        assert_eq!(touch(&mut pad, 2, Phase::Began, 700.0, 250.0, 650.0), vec![]);
        assert_eq!(touch(&mut pad, 2, Phase::Moved, 800.0, 250.0, 660.0), vec![]);
        assert_eq!(touch(&mut pad, 1, Phase::Moved, 520.0, 250.0, 670.0), vec![moved(10.0, 0.0)]);
        assert_eq!(touch(&mut pad, 2, Phase::Ended, 800.0, 250.0, 680.0), vec![]);
        assert_eq!(touch(&mut pad, 1, Phase::Ended, 520.0, 250.0, 690.0), vec![button(MouseButton::Left, false)]);
    }

    #[test]
    fn three_fingers_ask_for_the_keyboard_once() {
        let mut pad = pad();
        touch(&mut pad, 1, Phase::Began, 300.0, 250.0, 0.0);
        touch(&mut pad, 2, Phase::Began, 500.0, 250.0, 5.0);
        assert_eq!(touch(&mut pad, 3, Phase::Began, 700.0, 250.0, 10.0), vec![Gesture::Keyboard]);
        assert_eq!(touch(&mut pad, 4, Phase::Began, 900.0, 250.0, 15.0), vec![]);
        assert_eq!(touch(&mut pad, 1, Phase::Moved, 300.0, 450.0, 20.0), vec![]);
        for id in 1..=4 {
            assert_eq!(touch(&mut pad, id, Phase::Ended, 500.0, 250.0, 30.0), vec![], "finger {id}");
        }
        touch(&mut pad, 5, Phase::Began, 500.0, 250.0, 40.0);
        assert_eq!(touch(&mut pad, 5, Phase::Ended, 500.0, 250.0, 50.0), click(MouseButton::Left));
    }

    #[test]
    fn the_lift_point_counts_as_a_move() {
        let mut pad = pad();
        touch(&mut pad, 1, Phase::Began, 500.0, 250.0, 0.0);
        assert_eq!(touch(&mut pad, 1, Phase::Ended, 530.0, 250.0, 100.0), vec![moved(30.0, 0.0)], "no click");
        touch(&mut pad, 1, Phase::Began, 500.0, 250.0, 200.0);
        touch(&mut pad, 1, Phase::Moved, 510.0, 250.0, 800.0);
        assert_eq!(touch(&mut pad, 1, Phase::Ended, 520.0, 250.0, 900.0),
            vec![moved(10.0, 0.0), button(MouseButton::Left, false)], "a drag keeps its last stretch");
    }

    #[test]
    fn a_rested_finger_lifted_inside_the_slop_clicks_without_a_drag() {
        let mut pad = pad();
        touch(&mut pad, 1, Phase::Began, 500.0, 250.0, 0.0);
        assert_eq!(touch(&mut pad, 1, Phase::Ended, 502.0, 250.0, 600.0), click(MouseButton::Left));
        touch(&mut pad, 1, Phase::Began, 500.0, 250.0, 700.0);
        assert_eq!(touch(&mut pad, 1, Phase::Ended, 530.0, 250.0, 1300.0), vec![moved(30.0, 0.0)]);
    }

    #[test]
    fn a_scroll_never_clicks_right() {
        // Each finger stays inside the slop, yet rounding carries the centroid past it.
        let mut pad = pad();
        touch(&mut pad, 1, Phase::Began, 500.0, 250.0, 0.0);
        touch(&mut pad, 2, Phase::Began, 510.0 + 6.0 / 32768.0, 250.0, 1.0);
        touch(&mut pad, 1, Phase::Moved, 510.0, 250.0, 2.0);
        assert_eq!(touch(&mut pad, 2, Phase::Moved, 520.0 + 6.0 / 32768.0, 250.0, 3.0).len(), 1, "it scrolls");
        assert_eq!(touch(&mut pad, 1, Phase::Ended, 510.0, 250.0, 4.0), vec![]);
    }

    #[test]
    fn a_pinch_does_not_click_right() {
        let mut pad = pad();
        touch(&mut pad, 1, Phase::Began, 400.0, 250.0, 0.0);
        touch(&mut pad, 2, Phase::Began, 600.0, 250.0, 10.0);
        assert_eq!(touch(&mut pad, 1, Phase::Moved, 382.0, 250.0, 20.0), vec![]);
        assert_eq!(touch(&mut pad, 2, Phase::Moved, 618.0, 250.0, 30.0), vec![], "the centroid stays put");
        assert_eq!(touch(&mut pad, 1, Phase::Ended, 382.0, 250.0, 40.0), vec![]);
    }

    #[test]
    fn cancelling_an_extra_finger_keeps_the_drag() {
        let mut pad = pad();
        touch(&mut pad, 1, Phase::Began, 500.0, 250.0, 0.0);
        touch(&mut pad, 1, Phase::Moved, 510.0, 250.0, 600.0);
        touch(&mut pad, 2, Phase::Began, 700.0, 250.0, 650.0);
        assert_eq!(touch(&mut pad, 2, Phase::Cancelled, 700.0, 250.0, 660.0), vec![]);
        assert_eq!(touch(&mut pad, 1, Phase::Moved, 520.0, 250.0, 670.0), vec![moved(10.0, 0.0)]);
        assert_eq!(touch(&mut pad, 1, Phase::Ended, 520.0, 250.0, 680.0), vec![button(MouseButton::Left, false)]);
    }

    #[test]
    fn reset_lets_go_of_a_drag() {
        let mut pad = pad();
        touch(&mut pad, 1, Phase::Began, 500.0, 250.0, 0.0);
        touch(&mut pad, 1, Phase::Moved, 510.0, 250.0, 600.0);
        assert_eq!(pad.reset(), vec![button(MouseButton::Left, false)]);
        assert_eq!(pad.reset(), vec![]);
        assert_eq!(touch(&mut pad, 1, Phase::Ended, 510.0, 250.0, 700.0), vec![]);
    }

    #[test]
    fn unknown_and_repeated_fingers_are_ignored() {
        let mut pad = pad();
        assert_eq!(touch(&mut pad, 9, Phase::Moved, 500.0, 250.0, 0.0), vec![]);
        assert_eq!(touch(&mut pad, 9, Phase::Ended, 500.0, 250.0, 0.0), vec![]);
        touch(&mut pad, 1, Phase::Began, 500.0, 250.0, 0.0);
        assert_eq!(touch(&mut pad, 1, Phase::Began, 500.0, 250.0, 10.0), vec![]);
        assert_eq!(touch(&mut pad, 1, Phase::Ended, 500.0, 250.0, 20.0), click(MouseButton::Left));
    }
}
