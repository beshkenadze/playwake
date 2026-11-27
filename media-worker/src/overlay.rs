//! X11 stream controls and diagnostics composited above the video child.
//! Port of media/stream_overlay.py: the overlay child lives on the SAME Xlib
//! display as the video child (events pumped by the video tick); cairo draws
//! through an Xlib surface on that display.

use x11_dl::xlib;
use x11rb::connection::Connection;
use x11rb::protocol::shape::{ConnectionExt as ShapeExt, SO, SK};
use x11rb::protocol::xproto::ClipOrdering;
use x11rb::rust_connection::RustConnection;

/// The overlay as a picture, for a video sink that draws it itself.
pub struct Layer {
    /// Where the picture goes in the video window.
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    /// Rows of pixels, each blue, green, red and an unused byte.
    pub pixels: Vec<u8>,
}

pub enum OverlayEvent {
    Toggle,
    Select(usize),
    HintVisibility(bool),
}

pub struct Overlay {
    xlib: xlib::Xlib,
    shape_conn: RustConnection,
    display: *mut xlib::Display,
    pub xid: u32,
    parent: u32,
    visual: *mut xlib::Visual,
    hint_pending: bool,
    hint_until: f64,
    pub menu: bool,
    pub diagnostics: bool,
    pub selection: usize,
    pub metrics: Vec<String>,
    pub compact: String,
    pub compact_rate: String,
    pub russian: bool,
    pub xbox_palette: bool,
    pub console_name: String,
    /// The stream comes from a PC, not an Xbox: its menu has a mouse mode
    /// item, and a right click goes to the PC.
    pub pc: bool,
    /// The PC mouse is in desktop mode, not game mode.
    pub desktop_mouse: bool,
    /// Paint into `layer`, not the window: a child window above the video
    /// stops Gamescope from showing the video's own HDR frames.
    pub offscreen: bool,
    pub layer: Option<std::sync::Arc<Layer>>,
    rendered: Option<(String, usize, bool, bool, String, String, Vec<String>)>,
    geometry: Option<(String, i32, i32, i32, i32)>,
    scale: f64,
}

impl Overlay {
    pub fn new(display: *mut xlib::Display, parent: u32, pc: bool) -> Result<Self, String> {
        let xlib = xlib::Xlib::open().map_err(|e| format!("libX11: {e}"))?;
        let (shape_conn, _) = x11rb::connect(None).map_err(|e| format!("X11 shape connect: {e}"))?;
        unsafe {
            let screen = (xlib.XDefaultScreen)(display);
            let visual = (xlib.XDefaultVisual)(display, screen);
            let xid = (xlib.XCreateSimpleWindow)(display, parent as xlib::Window, 0, 0, 1, 1, 0,
                (xlib.XBlackPixel)(display, screen), (xlib.XWhitePixel)(display, screen));
            if xid == 0 { return Err("overlay XCreateSimpleWindow failed".into()); }
            (xlib.XStoreName)(display, xid, b"Playwake stream overlay\0".as_ptr() as *const i8);
            (xlib.XSelectInput)(display, xid, xlib::ExposureMask | xlib::ButtonPressMask);
            // Right click opens the menu from the video or its parent, except
            // in a PC stream, where it is the PC's.
            if !pc {
                (xlib.XGrabButton)(display, 3, xlib::AnyModifier, parent as xlib::Window, 0,
                    (xlib::ButtonPressMask | xlib::ButtonReleaseMask) as u32,
                    xlib::GrabModeAsync, xlib::GrabModeAsync, 0, 0);
            }
            (xlib.XFlush)(display);
            (xlib.XSync)(display, 0);
            Ok(Self {
                xlib, shape_conn, display, xid: xid as u32, parent, visual,
                hint_pending: false, hint_until: 0.0,
                menu: false, diagnostics: false, selection: 0,
                metrics: Vec::new(), compact: "—".into(), compact_rate: "—".into(),
                russian: false, xbox_palette: false, console_name: "Xbox".into(), pc, desktop_mouse: false,
                offscreen: false, layer: None, rendered: None, geometry: None, scale: 1.0,
            })
        }
    }

    pub fn invalidate(&mut self) {
        self.rendered = None;
    }

    pub fn show_startup_hint(&mut self) {
        self.hint_pending = !self.menu;
    }

    /// Button events arrive via the shared Xlib pump (video tick).
    pub fn handle_button(&mut self, window: u32, button: u32, x: i32, y: i32) -> Vec<OverlayEvent> {
        let mut events = Vec::new();
        if button == 3 && !self.pc && (window == self.parent || window == self.xid) {
            events.push(OverlayEvent::Toggle);
        } else if let Some((left, top)) = self.pressed_in(window).filter(|_| button == 1 && self.menu) {
            let x = (x - left) as f64 / self.scale;
            let y = (y - top) as f64 / self.scale;
            events.extend(item_at(x, y, self.items().len()).map(OverlayEvent::Select));
        }
        events
    }

    /// The menu's items; the app acts on them by their index.
    fn items(&self) -> Vec<String> {
        let label = |ru: &str, en: &str| if self.russian { ru.to_string() } else { en.to_string() };
        let diagnostics = if self.diagnostics { label("Скрыть статистику", "Hide statistics") }
            else { label("Статистика", "Statistics") };
        let guide = if self.pc { "Guide" } else { "Xbox Guide" };
        let mut items = vec![label("Продолжить", "Resume"), guide.to_string(), diagnostics,
            label("Переподключить", "Reconnect"), label("Завершить стрим", "End stream")];
        if self.pc {
            items.push(if self.desktop_mouse { label("Мышь: рабочий стол", "Mouse: desktop") }
                else { label("Мышь: игра", "Mouse: game") });
        }
        items
    }

    /// Where the overlay starts in the window a press came from, if the
    /// press can be on the overlay. Offscreen, presses land on the video.
    fn pressed_in(&self, window: u32) -> Option<(i32, i32)> {
        if window == self.xid { return Some((0, 0)); }
        if !self.offscreen || window != self.parent { return None; }
        self.geometry.as_ref().map(|geometry| (geometry.1, geometry.2))
    }

    pub fn update(&mut self, video_size: (u32, u32), now: f64) -> Vec<OverlayEvent> {
        let mut events = Vec::new();
        if self.menu {
            self.hint_pending = false;
            self.hint_until = 0.0;
        } else if self.hint_pending {
            self.hint_pending = false;
            self.hint_until = now + 4.0;
        }
        let hint = now < self.hint_until;
        let mode = if self.menu { "menu" } else if hint { "hint" } else if self.diagnostics { "hud" } else { "" };
        let was_hint = self.geometry.as_ref().is_some_and(|g| g.0 == "hint");
        if hint != was_hint {
            events.push(OverlayEvent::HintVisibility(hint));
        }
        if mode.is_empty() {
            if self.geometry.is_some() {
                self.layer = None;
                unsafe {
                    (self.xlib.XUnmapWindow)(self.display, self.xid as xlib::Window);
                    (self.xlib.XFlush)(self.display);
                }
                self.geometry = None;
                self.rendered = None;
            }
            return events;
        }
        let (width, height) = (video_size.0 as f64, video_size.1 as f64);
        self.scale = scale(width, height, self.items().len());
        let (logical_width, logical_height) = if self.menu {
            (440.0, height / self.scale)
        } else if hint {
            (570.0, 86.0)
        } else {
            (490.0, 49.0)
        };
        let w = (logical_width * self.scale).round().max(1.0) as i32;
        let h = (logical_height * self.scale).round().max(1.0) as i32;
        let (x, y) = if self.menu {
            (width as i32 - w, 0)
        } else if hint {
            ((width as i32 - w) / 2, (24.0 * self.scale).round() as i32)
        } else {
            ((12.0 * self.scale).round() as i32, (12.0 * self.scale).round() as i32)
        };
        let layout = (mode.to_string(), x, y, w, h);
        if self.geometry.as_ref() != Some(&layout) && self.offscreen {
            self.geometry = Some(layout);
            self.rendered = None;
        } else if self.geometry.as_ref() != Some(&layout) {
            unsafe {
                (self.xlib.XMoveResizeWindow)(self.display, self.xid as xlib::Window, x, y, w as u32, h as u32);
            }
            if self.menu {
                let _ = self.shape_conn.shape_mask(SO::SET, SK::INPUT, self.xid, 0, 0, 0u32);
            } else {
                let _ = self.shape_conn.shape_rectangles(SO::SET, SK::INPUT,
                    ClipOrdering::UNSORTED, self.xid, 0, 0, &[]);
            }
            let _ = self.shape_conn.flush();
            unsafe {
                (self.xlib.XMapRaised)(self.display, self.xid as xlib::Window);
                (self.xlib.XFlush)(self.display);
            }
            self.geometry = Some(layout);
            // A resize repaints the new area with the window background, so
            // repaint even when the text itself did not change.
            self.rendered = None;
        }
        let frame = (mode.to_string(), self.selection, self.diagnostics, self.desktop_mouse,
            self.compact.clone(), self.compact_rate.clone(),
            if self.menu && self.diagnostics { self.metrics.clone() } else { Vec::new() });
        if self.rendered.as_ref() != Some(&frame) {
            if self.offscreen {
                self.layer = self.picture(w, h, logical_width, logical_height, mode)
                    .map(|pixels| std::sync::Arc::new(Layer { x, y, width: w as u32, height: h as u32, pixels }));
            } else {
                self.draw(w, h, logical_width, logical_height, mode);
            }
            self.rendered = Some(frame);
        }
        events
    }

    /// The overlay painted into memory.
    fn picture(&self, pixel_width: i32, pixel_height: i32, width: f64, height: f64, mode: &str) -> Option<Vec<u8>> {
        let mut surface = cairo::ImageSurface::create(cairo::Format::Rgb24, pixel_width, pixel_height).ok()?;
        {
            let ctx = cairo::Context::new(&surface).ok()?;
            ctx.scale(pixel_width as f64 / width, pixel_height as f64 / height);
            self.paint(&ctx, width, height, mode);
        }
        let (stride, row) = (surface.stride() as usize, pixel_width as usize * 4);
        let data = surface.data().ok()?;
        Some(data.chunks(stride).take(pixel_height as usize).flat_map(|line| &line[..row]).copied().collect())
    }

    fn draw(&mut self, pixel_width: i32, pixel_height: i32, width: f64, height: f64, mode: &str) {
        // cairo-rs 0.20 dropped the safe Xlib surface wrapper; use the ffi
        // surface (same call Python makes) with a safe Context for drawing.
        let surface = unsafe {
            cairo::ffi::cairo_xlib_surface_create(self.display as *mut _,
                self.xid as std::os::raw::c_ulong, self.visual as *mut _,
                pixel_width, pixel_height)
        };
        if surface.is_null() { return; }
        let ctx = unsafe { cairo::Context::from_raw_full(cairo::ffi::cairo_create(surface)) };
        let _ = ctx.scale(pixel_width as f64 / width, pixel_height as f64 / height);
        self.paint(&ctx, width, height, mode);
        drop(ctx);
        unsafe {
            cairo::ffi::cairo_surface_destroy(surface);
            (self.xlib.XFlush)(self.display);
        }
    }

    fn paint(&self, ctx: &cairo::Context, width: f64, height: f64, mode: &str) {
        let muted = (0.63, 0.63, 0.65);
        let accent = if self.xbox_palette { (0.42, 0.75, 0.29) } else { (0.55, 0.86, 0.70) };
        let label = |ru: &str, en: &str| if self.russian { ru.to_string() } else { en.to_string() };
        let rect = |ctx: &cairo::Context, x: f64, y: f64, w: f64, h: f64, color: (f64, f64, f64)| {
            ctx.set_source_rgb(color.0, color.1, color.2);
            ctx.rectangle(x, y, w, h);
            let _ = ctx.fill();
        };
        let text = |ctx: &cairo::Context, x: f64, y: f64, value: &str, size: f64, bold: bool, color: (f64, f64, f64)| {
            ctx.set_source_rgb(color.0, color.1, color.2);
            ctx.select_font_face("DejaVu Sans", cairo::FontSlant::Normal,
                if bold { cairo::FontWeight::Bold } else { cairo::FontWeight::Normal });
            ctx.set_font_size(size);
            ctx.move_to(x, y);
            let _ = ctx.show_text(value);
        };
        if mode == "menu" {
            rect(ctx, 0.0, 0.0, width, height, (0.12, 0.12, 0.13));
            rect(ctx, 0.0, 0.0, 1.0, height, (0.28, 0.28, 0.29));
            let name: String = self.console_name.chars().take(26).collect();
            text(ctx, 32.0, 53.0, &name, 26.0, true, (0.93, 0.95, 0.99));
            ctx.set_source_rgb(accent.0, accent.1, accent.2);
            ctx.arc(36.0, 79.0, 4.0, 0.0, 6.284);
            let _ = ctx.fill();
            text(ctx, 48.0, 85.0, &label("Стрим идёт", "Streaming"), 16.0, false, accent);
            text(ctx, 32.0, 111.0, &self.compact_rate, 13.0, false, muted);
            text(ctx, 32.0, 131.0, &self.compact, 13.0, false, muted);
            let names = self.items();
            for (index, name) in names.iter().enumerate() {
                let y = ITEMS_TOP + ITEM_STEP * index as f64;
                let selected = index == self.selection;
                if selected {
                    rect(ctx, 20.0, y - 2.0, width - 40.0, 56.0, accent);
                    rect(ctx, 23.0, y + 1.0, width - 46.0, 50.0, (0.24, 0.27, 0.26));
                }
                let color = if index == 4 { (0.99, 0.53, 0.51) } else { (0.93, 0.93, 0.93) };
                text(ctx, 40.0, y + 33.0, name, 18.0, selected, color);
            }
            let metrics = ITEMS_TOP + ITEM_STEP * names.len() as f64 + 25.0;
            if self.diagnostics && height > metrics + 178.0 {
                for (index, line) in self.metrics.iter().skip(1).enumerate() {
                    text(ctx, 32.0, metrics + 19.0 * index as f64, line, 13.0, false, muted);
                }
            }
            let paused = if self.pc { label("Ввод на ПК приостановлен", "PC input paused") }
                else { label("Ввод на Xbox приостановлен", "Xbox input paused") };
            text(ctx, 32.0, height - 73.0, &paused, 15.0, false, muted);
            rect(ctx, 0.0, height - 56.0, width, 1.0, (0.28, 0.28, 0.29));
            text(ctx, 120.0, height - 22.0, &label("A  Выбрать     B  В игру", "A  Select     B  Back to game"), 15.0, true, (0.93, 0.95, 0.99));
        } else if mode == "hint" {
            rect(ctx, 0.0, 0.0, width, height, (0.12, 0.12, 0.13));
            rect(ctx, 0.0, 0.0, 4.0, height, accent);
            text(ctx, 19.0, 25.0, &label("Меню Playwake", "Playwake stream menu"), 15.0, true, accent);
            text(ctx, 19.0, 54.0, "View + Menu", 24.0, true, (0.93, 0.95, 0.99));
            text(ctx, 19.0, 78.0, &label("Нажмите одновременно", "Tap both buttons together"), 15.0, false, (0.93, 0.95, 0.99));
        } else {
            rect(ctx, 0.0, 0.0, width, height, (0.08, 0.09, 0.09));
            text(ctx, 10.0, 19.0, &self.compact_rate, 13.0, false, (0.93, 0.95, 0.99));
            text(ctx, 10.0, 37.0, &self.compact, 13.0, false, (0.93, 0.95, 0.99));
        }
    }

    pub fn close(&mut self) {
        unsafe {
            (self.xlib.XDestroyWindow)(self.display, self.xid as xlib::Window);
            (self.xlib.XFlush)(self.display);
        }
    }
}

/// Where the menu's first item starts and how far apart items are, in
/// unscaled pixels; an item is 52 pixels high.
const ITEMS_TOP: f64 = 152.0;
const ITEM_STEP: f64 = 58.0;

/// How much the overlay is enlarged in a window: up to twice, as long as
/// it fits, the menu of `items` items with its footer below them.
fn scale(width: f64, height: f64, items: usize) -> f64 {
    let tall = 545.0 + ITEM_STEP * items.saturating_sub(5) as f64;
    2.0_f64.min(width / 1280.0).min(height / tall)
}

/// The menu item at a point of the menu, of `count` items.
fn item_at(x: f64, y: f64, count: usize) -> Option<usize> {
    let offset = y - ITEMS_TOP;
    let index = (offset / ITEM_STEP).floor();
    ((22.0..=418.0).contains(&x) && index >= 0.0 && index < count as f64 && offset % ITEM_STEP < 52.0)
        .then_some(index as usize)
}

// The shared Xlib display is only touched from the glib main-loop thread.
unsafe impl Send for Overlay {}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_footer_stays_below_the_last_item_in_a_wide_window() {
        for (width, height) in [(2560.0, 1080.0), (1280.0, 800.0), (3440.0, 1440.0), (640.0, 400.0)] {
            for items in [5, 6] {
                let logical_height = height / scale(width, height, items);
                // The paused line's baseline sits 73 above the bottom, its text about 15 high.
                let last_item_bottom = ITEMS_TOP + ITEM_STEP * (items - 1) as f64 + 52.0;
                assert!(logical_height - 73.0 - 15.0 >= last_item_bottom, "{width}x{height} with {items} items");
            }
        }
    }

    #[test]
    fn clicks_pick_the_item_under_them_including_the_pc_mouse_item() {
        assert_eq!(item_at(100.0, 152.0, 5), Some(0));
        assert_eq!(item_at(100.0, 152.0 + 58.0 * 4.0 + 51.0, 5), Some(4));
        // The gap between two items and the space past the last one pick nothing.
        assert_eq!(item_at(100.0, 152.0 + 53.0, 5), None);
        assert_eq!(item_at(100.0, 152.0 + 58.0 * 5.0 + 10.0, 5), None);
        assert_eq!(item_at(100.0, 152.0 + 58.0 * 5.0 + 10.0, 6), Some(5));
        assert_eq!(item_at(10.0, 152.0, 5), None);
        assert_eq!(item_at(100.0, 100.0, 5), None);
    }
}
