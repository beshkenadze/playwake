//! Pure port of the Python worker's StreamInput state machine
//! (media/gstreamer_player.py:407-503). Events are returned, not emitted, so
//! the machine is unit-testable with an injected clock.

pub const VIEW_MENU: u16 = 8 | 4;
pub const GUIDE: u16 = 2;
/// Both chord buttons must arrive inside this window to count as a tap.
pub const CHORD_WINDOW_MS: f64 = 90.0;
/// A replayed tap stays pressed this long, one input tick, however fast the
/// packets follow each other.
const REPLAY_MS: f64 = 8.0;

#[derive(Debug, PartialEq)]
pub enum RouteEvent {
    OverlayKey(&'static str),
    Overlay(bool),
    GuidePulse(bool),
    Rearmed,
    Focus(bool),
}

#[derive(Debug, PartialEq)]
pub struct Routed {
    pub axes: [i16; 4],
    pub buttons: u16,
    pub lt: u16,
    pub rt: u16,
}

impl Routed {
    fn neutral() -> Self {
        Self { axes: [0, 0, 0, 0], buttons: 0, lt: 0, rt: 0 }
    }
}

pub struct Route {
    pub overlay: bool,
    focused: bool,
    release_gate: bool,
    previous: u16,
    chord_started: Option<f64>,
    pending_buttons: u16,
    replay: Option<(u16, f64)>,
    passthrough: bool,
    guide: bool,
    guide_until: Option<f64>,
}

impl Default for Route {
    fn default() -> Self {
        Self {
            overlay: false,
            focused: true,
            release_gate: false,
            previous: 0,
            chord_started: None,
            pending_buttons: 0,
            replay: None,
            passthrough: false,
            guide: false,
            guide_until: None,
        }
    }
}

impl Route {
    fn reset_chord(&mut self) {
        self.chord_started = None;
        self.pending_buttons = 0;
        self.passthrough = false;
    }

    pub fn close_overlay(&mut self, guide: bool) -> Vec<RouteEvent> {
        if !self.overlay { return Vec::new(); }
        self.overlay = false;
        self.release_gate = true;
        self.guide = guide;
        self.guide_until = None;
        self.reset_chord();
        vec![RouteEvent::Overlay(false)]
    }

    pub fn open_overlay(&mut self) -> Vec<RouteEvent> {
        if self.overlay { return Vec::new(); }
        self.overlay = true;
        self.reset_chord();
        vec![RouteEvent::Overlay(true)]
    }

    pub fn focus(&mut self, active: bool) -> Vec<RouteEvent> {
        if self.focused == active { return Vec::new(); }
        self.focused = active;
        self.release_gate = true;
        self.guide = false;
        self.guide_until = None;
        self.reset_chord();
        vec![RouteEvent::Focus(active)]
    }

    pub fn route(&mut self, axes: [i16; 4], buttons: u16, lt: u16, rt: u16, now: f64)
        -> (Routed, Vec<RouteEvent>)
    {
        let mut events = Vec::new();
        let pressed = buttons & !self.previous;
        self.previous = buttons;

        if !self.focused {
            return (Routed::neutral(), events);
        }
        if self.overlay {
            for (bit, key) in [(1024 | 256, "left"), (2048 | 512, "right"), (16, "activate"), (32, "back")] {
                if pressed & bit != 0 {
                    events.push(RouteEvent::OverlayKey(key));
                    break;
                }
            }
            return (Routed::neutral(), events);
        }
        if self.guide {
            if self.guide_until.is_none() {
                self.guide_until = Some(now + 100.0);
                events.push(RouteEvent::GuidePulse(true));
            }
            if now < self.guide_until.unwrap() {
                return (Routed { buttons: GUIDE, ..Routed::neutral() }, events);
            }
            events.push(RouteEvent::GuidePulse(false));
            self.guide = false;
            self.guide_until = None;
            return (Routed::neutral(), events);
        }
        if self.release_gate {
            if buttons == 0 && axes.iter().map(|a| a.abs()).max().unwrap_or(0) < 4096
                && lt.max(rt) < 4096
            {
                self.release_gate = false;
                events.push(RouteEvent::Rearmed);
            }
            return (Routed::neutral(), events);
        }

        let chord = buttons & VIEW_MENU;
        if chord == 0 {
            if self.pending_buttons != 0 {
                self.replay = Some((self.pending_buttons, now + REPLAY_MS));
            }
            self.reset_chord();
            let replay = match self.replay {
                Some((held, until)) if now < until => held,
                _ => { self.replay = None; 0 }
            };
            return (Routed { axes, buttons: buttons | replay, lt, rt }, events);
        }
        if self.passthrough {
            return (Routed { axes, buttons, lt, rt }, events);
        }
        if self.chord_started.is_none() {
            self.chord_started = Some(now);
        }
        let elapsed = now - self.chord_started.unwrap();
        // The second button must arrive before the first is sent to Xbox:
        // a simultaneous tap opens the overlay instead of a hold.
        if chord == VIEW_MENU && elapsed <= CHORD_WINDOW_MS {
            events.extend(self.open_overlay());
            return (Routed::neutral(), events);
        }
        self.pending_buttons |= chord;
        if elapsed >= CHORD_WINDOW_MS {
            self.passthrough = true;
            let replay = self.pending_buttons;
            self.pending_buttons = 0;
            return (Routed { axes, buttons: buttons | replay, lt, rt }, events);
        }
        (Routed { axes, buttons: buttons & !VIEW_MENU, lt, rt }, events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn axes() -> [i16; 4] { [100, 200, 300, 400] }

    #[test]
    fn passthrough_when_focused_and_idle() {
        let mut r = Route::default();
        let (out, ev) = r.route(axes(), 16, 0, 0, 0.0);
        assert_eq!(out.buttons, 16);
        assert_eq!(out.axes, axes());
        assert!(ev.is_empty());
    }

    #[test]
    fn unfocused_returns_neutral() {
        let mut r = Route::default();
        r.focus(false);
        let (out, _) = r.route(axes(), 16, 500, 500, 0.0);
        assert_eq!(out, Routed::neutral());
    }

    #[test]
    fn focus_change_emits_and_gates() {
        let mut r = Route::default();
        let ev = r.focus(false);
        assert_eq!(ev, vec![RouteEvent::Focus(false)]);
        let ev = r.focus(false);
        assert!(ev.is_empty());
        let ev = r.focus(true);
        assert_eq!(ev, vec![RouteEvent::Focus(true)]);
        // gate holds neutral until inputs return to neutral
        let (out, ev) = r.route(axes(), 16, 0, 0, 10.0);
        assert_eq!(out, Routed::neutral());
        assert!(ev.is_empty());
        let (out, ev) = r.route([0, 0, 0, 0], 0, 0, 0, 20.0);
        assert_eq!(out, Routed::neutral());
        assert_eq!(ev, vec![RouteEvent::Rearmed]);
        let (out, _) = r.route(axes(), 16, 0, 0, 30.0);
        assert_eq!(out.buttons, 16);
    }

    #[test]
    fn gate_waits_for_triggers_too() {
        let mut r = Route::default();
        r.focus(false);
        r.focus(true);
        let (_, ev) = r.route([0, 0, 0, 0], 0, 5000, 0, 10.0);
        assert!(ev.is_empty());
        let (_, ev) = r.route([0, 0, 0, 0], 0, 0, 0, 20.0);
        assert_eq!(ev, vec![RouteEvent::Rearmed]);
    }

    #[test]
    fn overlay_swallows_input_and_emits_keys() {
        let mut r = Route::default();
        r.open_overlay();
        let (out, ev) = r.route(axes(), 16, 0, 0, 0.0);
        assert_eq!(out, Routed::neutral());
        assert_eq!(ev, vec![RouteEvent::OverlayKey("activate")]);
        let (_, ev) = r.route(axes(), 32, 0, 0, 10.0);
        assert_eq!(ev, vec![RouteEvent::OverlayKey("back")]);
        let (_, ev) = r.route(axes(), 1024, 0, 0, 20.0);
        assert_eq!(ev, vec![RouteEvent::OverlayKey("left")]);
        let (_, ev) = r.route(axes(), 512, 0, 0, 30.0);
        assert_eq!(ev, vec![RouteEvent::OverlayKey("right")]);
        // held (not pressed) buttons emit nothing
        let (_, ev) = r.route(axes(), 512, 0, 0, 40.0);
        assert!(ev.is_empty());
    }

    #[test]
    fn overlay_key_priority_left_over_activate() {
        let mut r = Route::default();
        r.open_overlay();
        let (_, ev) = r.route(axes(), 16 | 1024, 0, 0, 0.0);
        assert_eq!(ev, vec![RouteEvent::OverlayKey("left")]);
    }

    #[test]
    fn simultaneous_tap_opens_overlay() {
        let mut r = Route::default();
        let (out, ev) = r.route(axes(), VIEW_MENU, 0, 0, 0.0);
        assert_eq!(out, Routed::neutral());
        assert_eq!(ev, vec![RouteEvent::Overlay(true)]);
        assert!(r.overlay);
    }

    #[test]
    fn second_button_within_window_opens_overlay() {
        let mut r = Route::default();
        let (out, ev) = r.route(axes(), 8, 0, 0, 0.0);
        assert_eq!(out.buttons, 0);
        assert!(ev.is_empty());
        // Menu arrives 40 ms after View: still a simultaneous tap.
        let (out, ev) = r.route(axes(), VIEW_MENU, 0, 0, 40.0);
        assert_eq!(out, Routed::neutral());
        assert_eq!(ev, vec![RouteEvent::Overlay(true)]);
        assert!(r.overlay);
    }

    #[test]
    fn single_chord_bit_passes_through_after_90ms() {
        let mut r = Route::default();
        let (out, _) = r.route(axes(), 8, 0, 0, 0.0);
        assert_eq!(out.buttons, 0);
        let (out, _) = r.route(axes(), 8, 0, 0, 89.0);
        assert_eq!(out.buttons, 0);
        let (out, _) = r.route(axes(), 8, 0, 0, 90.0);
        assert_eq!(out.buttons, 8);
        // passthrough stays armed for subsequent chord frames
        let (out, _) = r.route(axes(), 8 | 16, 0, 0, 100.0);
        assert_eq!(out.buttons, 8 | 16);
    }

    #[test]
    fn short_tap_replayed_after_chord_wait() {
        let mut r = Route::default();
        r.route(axes(), 8, 0, 0, 0.0);
        r.route(axes(), 8, 0, 0, 30.0);
        // tap released while waiting for second button
        let (out, _) = r.route(axes(), 0, 0, 0, 60.0);
        assert_eq!(out.buttons, 8);
    }

    #[test]
    fn replayed_tap_lasts_a_tick_when_packets_come_faster() {
        let mut r = Route::default();
        r.route(axes(), 8, 0, 0, 0.0);
        assert_eq!(r.route(axes(), 0, 0, 0, 60.0).0.buttons, 8);
        // a stick event one millisecond later must not end the tap
        assert_eq!(r.route(axes(), 16, 0, 0, 61.0).0.buttons, 8 | 16);
        assert_eq!(r.route(axes(), 0, 0, 0, 67.9).0.buttons, 8);
        assert_eq!(r.route(axes(), 0, 0, 0, 68.0).0.buttons, 0);
        assert_eq!(r.route(axes(), 0, 0, 0, 69.0).0.buttons, 0);
    }

    #[test]
    fn late_second_button_passes_chord_to_xbox() {
        let mut r = Route::default();
        r.route(axes(), 8, 0, 0, 0.0);
        let (out, ev) = r.route(axes(), VIEW_MENU, 0, 0, 100.0);
        assert_eq!(out.buttons, VIEW_MENU);
        assert!(ev.is_empty());
        assert!(!r.overlay);
        // Passthrough stays armed, so the held chord is not stripped.
        let (out, ev) = r.route(axes(), VIEW_MENU, 0, 0, 600.0);
        assert_eq!(out.buttons, VIEW_MENU);
        assert!(ev.is_empty());
    }

    #[test]
    fn close_overlay_gates_and_guides() {
        let mut r = Route::default();
        r.open_overlay();
        let ev = r.close_overlay(true);
        assert_eq!(ev, vec![RouteEvent::Overlay(false)]);
        // Python checks guide before the release gate: the pulse fires on the
        // first routed frame even while the gate is armed.
        let (out, ev) = r.route([0, 0, 0, 0], 0, 0, 0, 10.0);
        assert_eq!(out.buttons, GUIDE);
        assert_eq!(ev, vec![RouteEvent::GuidePulse(true)]);
        let (out, ev) = r.route([0, 0, 0, 0], 0, 0, 0, 20.0);
        assert_eq!(out.buttons, GUIDE);
        assert!(ev.is_empty());
        let (out, ev) = r.route(axes(), 0, 0, 0, 109.0);
        assert_eq!(out.buttons, GUIDE);
        assert!(ev.is_empty());
        let (out, ev) = r.route(axes(), 0, 0, 0, 110.0);
        assert_eq!(out, Routed::neutral());
        assert_eq!(ev, vec![RouteEvent::GuidePulse(false)]);
        let (out, ev) = r.route([0, 0, 0, 0], 0, 0, 0, 120.0);
        assert_eq!(out, Routed::neutral());
        assert_eq!(ev, vec![RouteEvent::Rearmed]);
    }

    #[test]
    fn close_without_overlay_is_noop() {
        let mut r = Route::default();
        assert!(r.close_overlay(false).is_empty());
    }

    #[test]
    fn open_overlay_twice_emits_once() {
        let mut r = Route::default();
        assert_eq!(r.open_overlay(), vec![RouteEvent::Overlay(true)]);
        assert!(r.open_overlay().is_empty());
    }

    #[test]
    fn focus_loss_clears_guide_and_chord() {
        let mut r = Route::default();
        r.route(axes(), 8, 0, 0, 0.0);
        r.focus(false);
        r.focus(true);
        let (out, ev) = r.route([0, 0, 0, 0], 0, 0, 0, 10.0);
        assert_eq!(out, Routed::neutral());
        assert_eq!(ev, vec![RouteEvent::Rearmed]);
        let (_, ev) = r.route([0, 0, 0, 0], 0, 0, 0, 20.0);
        assert!(ev.is_empty());
        // The chord window was reset, so both buttons count as a fresh tap.
        let (out, ev) = r.route(axes(), VIEW_MENU, 0, 0, 30.0);
        assert_eq!(out, Routed::neutral());
        assert_eq!(ev, vec![RouteEvent::Overlay(true)]);
    }

    #[test]
    fn neutral_axes_threshold_4096() {
        let mut r = Route::default();
        r.focus(false);
        r.focus(true);
        let (_, ev) = r.route([4096, 0, 0, 0], 0, 0, 0, 10.0);
        assert!(ev.is_empty());
        let (_, ev) = r.route([4095, 0, 0, 0], 0, 0, 0, 20.0);
        assert_eq!(ev, vec![RouteEvent::Rearmed]);
    }

    #[test]
    fn overlay_open_resets_pending_tap() {
        let mut r = Route::default();
        r.route(axes(), 8, 0, 0, 0.0);
        r.open_overlay();
        r.close_overlay(false);
        let (_, ev) = r.route([0, 0, 0, 0], 0, 0, 0, 10.0);
        assert_eq!(ev, vec![RouteEvent::Rearmed]);
        let (out, _) = r.route(axes(), 16, 0, 0, 20.0);
        assert_eq!(out.buttons, 16);
    }

    #[test]
    fn guide_pulse_only_once_per_close() {
        let mut r = Route::default();
        r.open_overlay();
        r.close_overlay(true);
        let (out, ev) = r.route([0, 0, 0, 0], 0, 0, 0, 0.0);
        assert_eq!(out.buttons, GUIDE);
        assert_eq!(ev, vec![RouteEvent::GuidePulse(true)]);
        let (_, ev) = r.route(axes(), 0, 0, 0, 10.0);
        assert!(ev.is_empty());
        let (_, ev) = r.route(axes(), 0, 0, 0, 100.0);
        assert_eq!(ev, vec![RouteEvent::GuidePulse(false)]);
        let (_, ev) = r.route([0, 0, 0, 0], 0, 0, 0, 110.0);
        assert_eq!(ev, vec![RouteEvent::Rearmed]);
        let (out, ev) = r.route(axes(), 16, 0, 0, 120.0);
        assert_eq!(out.buttons, 16);
        assert!(ev.is_empty());
    }

    #[test]
    fn pressed_edge_detection_uses_previous() {
        let mut r = Route::default();
        r.open_overlay();
        let (_, ev) = r.route(axes(), 16, 0, 0, 0.0);
        assert_eq!(ev, vec![RouteEvent::OverlayKey("activate")]);
        let (_, ev) = r.route(axes(), 16 | 32, 0, 0, 10.0);
        assert_eq!(ev, vec![RouteEvent::OverlayKey("back")]);
    }

    #[test]
    fn passthrough_chord_not_stripped() {
        let mut r = Route::default();
        r.route(axes(), 4, 0, 0, 0.0);
        r.route(axes(), 4, 0, 0, 90.0);
        let (out, _) = r.route(axes(), 4 | 8, 0, 0, 100.0);
        assert_eq!(out.buttons, 4 | 8);
    }

    #[test]
    fn chord_bits_stripped_before_threshold() {
        let mut r = Route::default();
        let (out, _) = r.route(axes(), 8 | 16, 0, 0, 0.0);
        assert_eq!(out.buttons, 16);
        let (out, _) = r.route(axes(), 8 | 32, 0, 0, 50.0);
        assert_eq!(out.buttons, 32);
    }
}