//! C ABI over the desktop stream-input route (media-worker/src/route.rs), so the
//! View+Menu tap, stream-menu navigation, the Xbox Guide pulse and the release
//! gate behave exactly as on SteamOS. The route is not thread-safe: the caller
//! drives one instance from a single queue.

use crate::route::{Route, RouteEvent};

/// Bits of the event mask returned by the route calls.
pub const PW_ROUTE_MENU_OPENED: u32 = 1 << 0;
pub const PW_ROUTE_MENU_CLOSED: u32 = 1 << 1;
pub const PW_ROUTE_KEY_PREVIOUS: u32 = 1 << 2;
pub const PW_ROUTE_KEY_NEXT: u32 = 1 << 3;
pub const PW_ROUTE_KEY_ACTIVATE: u32 = 1 << 4;
pub const PW_ROUTE_KEY_BACK: u32 = 1 << 5;

/// Controller state in the wire layout; routed in place.
#[repr(C)]
pub struct PwPad {
    pub axes: [i16; 4],
    pub buttons: u16,
    pub lt: u16,
    pub rt: u16,
}

/// The event mask, after opening or shutting the input gate with the menu.
fn report(route: *const Route, events: &[RouteEvent]) -> u32 {
    for event in events {
        if let RouteEvent::Overlay(open) = event { crate::input_bridge::menu(route as usize, *open); }
    }
    mask(events)
}

fn mask(events: &[RouteEvent]) -> u32 {
    events.iter().fold(0, |mask, event| mask | match event {
        RouteEvent::Overlay(true) => PW_ROUTE_MENU_OPENED,
        RouteEvent::Overlay(false) => PW_ROUTE_MENU_CLOSED,
        RouteEvent::OverlayKey("left") => PW_ROUTE_KEY_PREVIOUS,
        RouteEvent::OverlayKey("right") => PW_ROUTE_KEY_NEXT,
        RouteEvent::OverlayKey("activate") => PW_ROUTE_KEY_ACTIVATE,
        RouteEvent::OverlayKey("back") => PW_ROUTE_KEY_BACK,
        _ => 0,
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn pw_route_new() -> *mut Route {
    let route = Box::into_raw(Box::new(Route::default()));
    // A stream stopped with its menu open never closed it.
    crate::input_bridge::route_started(route as usize);
    route
}

/// # Safety
/// `route` must come from `pw_route_new` and not be used afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pw_route_free(route: *mut Route) {
    if !route.is_null() {
        drop(unsafe { Box::from_raw(route) });
    }
}

/// Routes one controller sample: rewrites `pad` with what may go to the
/// console and returns the event mask. `now_ms` is a monotonic clock.
///
/// # Safety
/// `route` and `pad` must be valid, or null (then nothing happens).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pw_route_input(route: *mut Route, pad: *mut PwPad, now_ms: f64) -> u32 {
    let address = route;
    let (Some(route), Some(pad)) = (unsafe { route.as_mut() }, unsafe { pad.as_mut() }) else {
        return 0;
    };
    let (routed, events) = route.route(pad.axes, pad.buttons, pad.lt, pad.rt, now_ms);
    pad.axes = routed.axes;
    pad.buttons = routed.buttons;
    pad.lt = routed.lt;
    pad.rt = routed.rt;
    report(address, &events)
}

/// Opens the stream menu (touch, keyboard). Returns the event mask.
///
/// # Safety
/// `route` must be valid, or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pw_route_open(route: *mut Route) -> u32 {
    unsafe { route.as_mut() }.map_or(0, |route| report(route, &route.open_overlay()))
}

/// Closes the stream menu; `guide` sends one Xbox Guide press afterwards.
///
/// # Safety
/// `route` must be valid, or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pw_route_close(route: *mut Route, guide: bool) -> u32 {
    unsafe { route.as_mut() }.map_or(0, |route| report(route, &route.close_overlay(guide)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::route::{GUIDE, VIEW_MENU};

    fn pad(buttons: u16) -> PwPad {
        PwPad { axes: [0; 4], buttons, lt: 0, rt: 0 }
    }

    #[test]
    fn view_menu_tap_opens_the_menu_without_reaching_the_console() {
        let _global = crate::input_bridge::global();
        let route = pw_route_new();
        let mut sample = pad(VIEW_MENU);
        let events = unsafe { pw_route_input(route, &mut sample, 0.0) };
        assert_eq!(events & PW_ROUTE_MENU_OPENED, PW_ROUTE_MENU_OPENED);
        assert_eq!(sample.buttons, 0);
        unsafe { pw_route_free(route) };
    }

    #[test]
    fn menu_keys_are_reported_and_swallowed() {
        let _global = crate::input_bridge::global();
        let route = pw_route_new();
        assert_eq!(unsafe { pw_route_open(route) }, PW_ROUTE_MENU_OPENED);
        let mut down = pad(512);
        assert_eq!(unsafe { pw_route_input(route, &mut down, 10.0) }, PW_ROUTE_KEY_NEXT);
        assert_eq!(down.buttons, 0);
        let mut back = pad(32);
        assert_eq!(unsafe { pw_route_input(route, &mut back, 20.0) }, PW_ROUTE_KEY_BACK);
        assert_eq!(back.buttons, 0);
        unsafe { pw_route_free(route) };
    }

    #[test]
    fn closing_with_guide_presses_guide_once_the_pad_is_released() {
        let _global = crate::input_bridge::global();
        let route = pw_route_new();
        unsafe { pw_route_open(route) };
        assert_eq!(unsafe { pw_route_close(route, true) }, PW_ROUTE_MENU_CLOSED);
        let mut sample = pad(0);
        unsafe { pw_route_input(route, &mut sample, 0.0) };
        assert_eq!(sample.buttons, GUIDE);
        unsafe { pw_route_free(route) };
    }

    #[test]
    fn null_pointers_are_ignored() {
        let mut sample = pad(16);
        assert_eq!(unsafe { pw_route_input(std::ptr::null_mut(), &mut sample, 0.0) }, 0);
        assert_eq!(unsafe { pw_route_open(std::ptr::null_mut()) }, 0);
    }
}
