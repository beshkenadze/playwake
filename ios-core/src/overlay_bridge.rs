//! C ABI for the on-screen pad over the stream (`playwake_core::input::overlay`).
//! The frontend draws the layout this gives it, asks which element a point
//! is on, and passes the fingers on them in; the pad state joins the
//! controller in `pw_input_pad`, and its keys and mouse buttons go to a PC
//! as the panel's. Layout edits are pure: JSON in, JSON out.

use crate::bridge::read_string;
use crate::input_bridge::{phase, with};
use playwake_core::input::overlay::{Layout, Surface};
use std::ffi::CString;
use std::os::raw::c_char;

/// How an element looks now (see `Look`).
#[repr(C)]
pub struct PwOverlayLook {
    pub pressed: bool,
    pub x: f32,
    pub y: f32,
}

fn layout_pointer(layout: &Layout) -> *mut c_char {
    CString::new(layout.to_json()).unwrap_or_default().into_raw()
}

/// # Safety
/// `json` must be a valid C string, or null.
unsafe fn read_layout(json: *const c_char) -> Layout {
    unsafe { read_string(json) }.map(|json| Layout::from_json(&json)).unwrap_or_default()
}

/// Puts `json` in use, the default pad if it is no layout, and returns the
/// layout in use, its values in range. Lets go of every finger. Release the
/// result with `pw_free_string`.
///
/// # Safety
/// `json` must be a valid C string, or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pw_overlay_set_layout(json: *const c_char) -> *mut c_char {
    let layout = unsafe { read_layout(json) };
    let result = layout_pointer(&layout);
    with(|input, pc| input.overlay_layout(pc, layout));
    result
}

/// `json` with its values in range, or the default pad if it is no layout.
/// Release the result with `pw_free_string`.
///
/// # Safety
/// `json` must be a valid C string, or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pw_overlay_normalize(json: *const c_char) -> *mut c_char {
    layout_pointer(&unsafe { read_layout(json) })
}

/// What an element's `size` is a fraction of on a surface of `width` by
/// `height` points.
#[unsafe(no_mangle)]
pub extern "C" fn pw_overlay_unit(width: f32, height: f32) -> f32 {
    Surface { width, height }.unit()
}

/// The default pad as JSON. Release the result with `pw_free_string`.
#[unsafe(no_mangle)]
pub extern "C" fn pw_overlay_default_layout() -> *mut c_char {
    layout_pointer(&Layout::default())
}

/// The surface the pad covers, in points; a new size lets go of every finger.
#[unsafe(no_mangle)]
pub extern "C" fn pw_overlay_surface(width: f32, height: f32) {
    with(|input, pc| input.overlay_surface(pc, Surface { width, height }));
}

/// The element at a point of the surface, or -1.
#[unsafe(no_mangle)]
pub extern "C" fn pw_overlay_hit(x: f32, y: f32) -> i32 {
    with(|input, _| input.overlay().hit(x, y)).map_or(-1, |index| index as i32)
}

/// One finger at a point of the surface, `phase` a `PW_INPUT_PHASE_*`,
/// `pressure` 0…1 (0 without force sensing). Returns `PW_OVERLAY_*` bits.
#[unsafe(no_mangle)]
pub extern "C" fn pw_overlay_touch(id: u32, phase: u32, x: f32, y: f32, pressure: f32) -> u32 {
    let Some(phase) = self::phase(phase) else { return 0 };
    with(|input, pc| input.overlay_touch(pc, id, phase, x, y, pressure))
}

/// Fills `look` for the element at `index` of the layout in use.
///
/// # Safety
/// `look` must be valid, or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pw_overlay_look(index: u32, look: *mut PwOverlayLook) {
    let Some(look) = (unsafe { look.as_mut() }) else { return };
    let now = with(|input, _| input.overlay().look(index as usize));
    *look = PwOverlayLook { pressed: now.pressed, x: now.x, y: now.y };
}

/// Lets go of every finger on the pad, as it hides: they stay out until they lift.
#[unsafe(no_mangle)]
pub extern "C" fn pw_overlay_release() {
    with(|input, pc| input.overlay_release(pc));
}

/// `json` with the element at `index`, in the orientation of a surface of
/// `width` by `height` points, centred at `x`, `y` (0…1) with diameter
/// `size`, all of it on the surface. Release the result with `pw_free_string`.
///
/// # Safety
/// `json` must be a valid C string, or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pw_overlay_place(json: *const c_char, width: f32, height: f32, index: u32,
    x: f32, y: f32, size: f32) -> *mut c_char
{
    let mut layout = unsafe { read_layout(json) };
    layout.place(Surface { width, height }, index as usize, x, y, size);
    layout_pointer(&layout)
}

/// `json` with the pad's opacity, 0.1…1. Release the result with `pw_free_string`.
///
/// # Safety
/// `json` must be a valid C string, or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pw_overlay_opacity(json: *const c_char, opacity: f32) -> *mut c_char {
    let mut layout = unsafe { read_layout(json) };
    layout.set_opacity(opacity);
    layout_pointer(&layout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::pw_free_string;
    use crate::input_bridge::{global, PW_INPUT_PHASE_BEGAN, PW_INPUT_PHASE_ENDED, PW_OVERLAY_HAPTIC};
    use crate::input_route::PwPad;

    /// Takes a string the core returned.
    fn take(pointer: *mut c_char) -> String {
        let text = unsafe { read_string(pointer) }.unwrap();
        pw_free_string(pointer);
        text
    }

    fn layout(text: &str) -> CString { CString::new(text).unwrap() }

    #[test]
    fn set_layout_returns_the_layout_in_use_or_the_default() {
        let _global = global();
        let default = take(pw_overlay_default_layout());
        assert_eq!(take(unsafe { pw_overlay_set_layout(std::ptr::null()) }), default);
        assert_eq!(take(unsafe { pw_overlay_set_layout(layout("{").as_ptr()) }), default);
        let used = take(unsafe { pw_overlay_set_layout(layout(r#"{"opacity": 5}"#).as_ptr()) });
        assert_eq!(Layout::from_json(&used).opacity, 1.0);
        unsafe { pw_overlay_set_layout(std::ptr::null()) };
    }

    #[test]
    fn a_tap_on_a_drawn_button_presses_it_in_the_pad() {
        let _global = global();
        // A new stream's route opens the gate another test's menu may have shut.
        unsafe { crate::input_route::pw_route_free(crate::input_route::pw_route_new()) };
        unsafe { pw_overlay_set_layout(std::ptr::null()) };
        pw_overlay_surface(750.0, 381.0);
        let a = Layout::default().landscape.iter().position(|element| element.x == 0.88 && element.y == 0.665).unwrap();
        let (x, y) = (0.88 * 750.0, 0.665 * 381.0);
        assert_eq!(pw_overlay_hit(x, y), a as i32);
        assert_eq!(pw_overlay_hit(375.0, 260.0), -1);
        assert_eq!(pw_overlay_touch(7, PW_INPUT_PHASE_BEGAN, x, y, 0.0), PW_OVERLAY_HAPTIC);
        let mut look = PwOverlayLook { pressed: false, x: 0.0, y: 0.0 };
        unsafe { pw_overlay_look(a as u32, &mut look) };
        assert!(look.pressed);
        let mut pad = PwPad { axes: [0; 4], buttons: 0, lt: 0, rt: 0 };
        unsafe { crate::input_bridge::pw_input_pad(&mut pad, 0.0) };
        assert_eq!(pad.buttons, 16);
        pw_overlay_release();
        assert_eq!(pw_overlay_touch(7, PW_INPUT_PHASE_ENDED, x, y, 0.0), 0);
        let mut pad = PwPad { axes: [0; 4], buttons: 0, lt: 0, rt: 0 };
        unsafe { crate::input_bridge::pw_input_pad(&mut pad, 8.0) };
        assert_eq!(pad.buttons, 0, "released");
        pw_overlay_surface(0.0, 0.0);
    }

    #[test]
    fn normalize_and_unit_need_no_state() {
        let default = take(pw_overlay_default_layout());
        assert_eq!(take(unsafe { pw_overlay_normalize(layout("[]").as_ptr()) }), default);
        let used = Layout::from_json(&take(unsafe { pw_overlay_normalize(layout(r#"{"dead_zone": 3}"#).as_ptr()) }));
        assert_eq!(used.dead_zone, 0.9);
        assert_eq!(pw_overlay_unit(402.0, 778.0), 402.0);
        assert_eq!(pw_overlay_unit(1376.0, 990.0), 480.0);
    }

    #[test]
    fn place_and_opacity_edit_the_json_they_get() {
        let default = take(pw_overlay_default_layout());
        let json = layout(&default);
        let placed = Layout::from_json(&take(unsafe { pw_overlay_place(json.as_ptr(), 750.0, 381.0, 0, 0.5, 0.5, 0.3) }));
        assert_eq!((placed.landscape[0].x, placed.landscape[0].y, placed.landscape[0].size), (0.5, 0.5, 0.3));
        assert_eq!(placed.portrait, Layout::default().portrait);
        let faint = Layout::from_json(&take(unsafe { pw_overlay_opacity(json.as_ptr(), 0.25) }));
        assert_eq!(faint.opacity, 0.25);
    }
}
