//! X11 video window and event pump, mirroring the Python worker's Xlib
//! architecture (media/gstreamer_player.py:54-274): one Xlib display owns the
//! video child, the overlay child and the button grab; x11rb is used only for
//! XRes adoption and Gamescope focus queries.

use std::collections::HashSet;
use x11_dl::xlib;
use x11rb::connection::Connection;
use x11rb::protocol::res::{ConnectionExt as ResExt, ClientIdSpec, ClientIdMask};
use x11rb::protocol::xproto::ConnectionExt as XProtoExt;
use x11rb::rust_connection::RustConnection;

pub struct X11Window {
    pub xlib: xlib::Xlib,
    pub display: *mut xlib::Display,
    pub xid: xlib::Window,
    parent: xlib::Window,
    pub size: (u32, u32),
    delete_atom: xlib::Atom,
    pub deleted: bool,
    adopt: bool,
    pub adopted: bool,
    pub adopt_started: Option<f64>,
    known_children: HashSet<u32>,
    res_conn: Option<RustConnection>,
    root: xlib::Window,
    baselayer_previous: Option<u32>,
}

pub struct Tick {
    pub size_changed: Option<(u32, u32)>,
    pub adopted: bool,
    pub adopt_timeout: bool,
    pub buttons: Vec<(u32, u32, i32, i32)>,
    pub exposes: Vec<u32>,
}

extern "C" fn x_error_handler(_display: *mut xlib::Display, event: *mut xlib::XErrorEvent) -> i32 {
    unsafe {
        let e = &*event;
        eprintln!("pw-x11 Xlib error: code={} request={} minor={} resource={:#x} serial={}",
            e.error_code, e.request_code, e.minor_code, e.resourceid, e.serial);
    }
    0
}

impl X11Window {
    pub fn create(width: u32, height: u32, parent: Option<u64>, adopt: bool) -> Result<Self, String> {
        let xlib = xlib::Xlib::open().map_err(|e| format!("libX11: {e}"))?;
        unsafe { (xlib.XSetErrorHandler)(Some(x_error_handler)); }
        unsafe {
            let display = (xlib.XOpenDisplay)(std::ptr::null());
            if display.is_null() { return Err("X11 display unavailable".into()); }
            let screen = (xlib.XDefaultScreen)(display);
            let root = (xlib.XDefaultRootWindow)(display);
            let mut delete_atom = (xlib.XInternAtom)(display, b"WM_DELETE_WINDOW\0".as_ptr() as *const i8, 0);
            let wm_protocols = (xlib.XInternAtom)(display, b"WM_PROTOCOLS\0".as_ptr() as *const i8, 0);
            let (xid, parent_xid, res_conn) = if adopt {
                let (conn, _) = x11rb::connect(None).map_err(|e| format!("X11 res connect: {e}"))?;
                (0, 0, Some(conn))
            } else {
                // Xwayland rejects CreateWindow of a depth-24 child inside the
                // ARGB32 GPUI parent (BadMatch), so the video window is an
                // override-redirect root child stacked above the app window at
                // the same geometry — visually identical to embedding.
                let parent_xid = parent.map(|p| p as xlib::Window).unwrap_or(root);
                let mut info: xlib::XVisualInfo = std::mem::zeroed();
                let matched = (xlib.XMatchVisualInfo)(display, screen, 24, xlib::TrueColor, &mut info);
                if matched == 0 { return Err("no 24-bit TrueColor visual".into()); }
                let mut attrs: xlib::XSetWindowAttributes = std::mem::zeroed();
                attrs.colormap = (xlib.XDefaultColormap)(display, screen);
                attrs.override_redirect = 1;
                let xid = (xlib.XCreateWindow)(display, root, 0, 0, width, height, 0,
                    24, xlib::InputOutput as u32, info.visual,
                    xlib::CWColormap | xlib::CWOverrideRedirect, &mut attrs);
                if xid == 0 { return Err("XCreateWindow failed".into()); }
                (xlib.XStoreName)(display, xid, b"Greenlight Native Video\0".as_ptr() as *const i8);
                (xlib.XSetWMProtocols)(display, xid, &mut delete_atom, 1);
                (xlib.XSelectInput)(display, xid,
                    xlib::ExposureMask | xlib::ButtonPressMask | xlib::StructureNotifyMask);
                if parent_xid != root {
                    let mut pr: xlib::Window = 0;
                    let mut px: i32 = 0; let mut py: i32 = 0;
                    let mut pw: u32 = 0; let mut ph: u32 = 0; let mut pb: u32 = 0; let mut pd: u32 = 0;
                    if (xlib.XGetGeometry)(display, parent_xid, &mut pr, &mut px, &mut py,
                        &mut pw, &mut ph, &mut pb, &mut pd) != 0 {
                        (xlib.XMoveResizeWindow)(display, xid, px, py, pw, ph);
                    }
                }
                (xlib.XMapRaised)(display, xid);
                let _ = wm_protocols;
                (xid, parent_xid, None)
            };
            (xlib.XFlush)(display);
            (xlib.XSync)(display, 0);
            let known_children = if adopt {
                let conn = res_conn.as_ref().unwrap();
                conn.query_tree(root as u32).map_err(|e| format!("X11 tree: {e}"))?.reply()
                    .map_err(|e| format!("X11 tree reply: {e}"))?.children.into_iter().collect()
            } else {
                HashSet::new()
            };
            Ok(Self {
                xlib, display, xid, parent: parent_xid, size: (width, height), delete_atom,
                deleted: false, adopt, adopted: false, adopt_started: None, known_children,
                res_conn, root, baselayer_previous: None,
            })
        }
    }

    pub fn sync(&self) {
        unsafe { (self.xlib.XSync)(self.display, 0); }
    }

    pub fn tick(&mut self, now: f64) -> Result<Tick, String> {
        let mut tick = Tick { size_changed: None, adopted: false, adopt_timeout: false,
            buttons: Vec::new(), exposes: Vec::new() };
        unsafe {
            while (self.xlib.XPending)(self.display) > 0 {
                let mut event: xlib::XEvent = std::mem::zeroed();
                (self.xlib.XNextEvent)(self.display, &mut event);
                match event.get_type() {
                    xlib::ClientMessage => {
                        let message: xlib::XClientMessageEvent = event.into();
                        if message.message_type == self.delete_atom { self.deleted = true; }
                    }
                    xlib::ButtonPress => {
                        let button: xlib::XButtonEvent = event.into();
                        tick.buttons.push((button.window as u32, button.button, button.x, button.y));
                    }
                    xlib::Expose => {
                        let expose: xlib::XExposeEvent = event.into();
                        tick.exposes.push(expose.window as u32);
                    }
                    _ => {}
                }
            }
        }
        if self.adopt && !self.adopted {
            if let Some(started) = self.adopt_started {
                if now - started > 5000.0 { tick.adopt_timeout = true; }
            }
            if let Some(conn) = self.res_conn.as_ref() {
                let children = conn.query_tree(self.root as u32)
                    .map_err(|e| format!("X11 tree: {e}"))?.reply()
                    .map_err(|e| format!("X11 tree reply: {e}"))?.children;
                for child in children {
                    if self.known_children.contains(&child) { continue; }
                    self.known_children.insert(child);
                    if self.owns_window(conn, child) {
                        let geometry = conn.get_geometry(self.root as u32)
                            .map_err(|e| format!("X11 geometry: {e}"))?.reply()
                            .map_err(|e| format!("X11 geometry reply: {e}"))?;
                        let (w, h) = (geometry.width as u32, geometry.height as u32);
                        unsafe {
                            (self.xlib.XMoveResizeWindow)(self.display, child as xlib::Window, 0, 0, w, h);
                            (self.xlib.XMapRaised)(self.display, child as xlib::Window);
                            (self.xlib.XSync)(self.display, 0);
                        }
                        self.xid = child as xlib::Window;
                        self.size = (w, h);
                        self.adopted = true;
                        tick.adopted = true;
                        tick.size_changed = Some((w, h));
                        break;
                    }
                }
            }
            return Ok(tick);
        }
        if self.xid == 0 { return Ok(tick); }
        let target = unsafe {
            let mut root: xlib::Window = 0;
            let mut x: i32 = 0; let mut y: i32 = 0;
            let mut w: u32 = 0; let mut h: u32 = 0; let mut b: u32 = 0; let mut d: u32 = 0;
            let drawable = if self.parent != 0 { self.parent } else { self.xid };
            if (self.xlib.XGetGeometry)(self.display, drawable, &mut root, &mut x, &mut y,
                &mut w, &mut h, &mut b, &mut d) == 0 { return Ok(tick); }
            (x, y, w, h)
        };
        if (target.2, target.3) != self.size {
            unsafe {
                (self.xlib.XMoveResizeWindow)(self.display, self.xid, target.0, target.1, target.2, target.3);
                (self.xlib.XFlush)(self.display);
            }
            self.size = (target.2, target.3);
            tick.size_changed = Some((target.2, target.3));
        }
        Ok(tick)
    }

    fn owns_window(&self, conn: &RustConnection, xid: u32) -> bool {
        let Ok(cookie) = conn.res_query_client_ids(&[ClientIdSpec {
            client: xid, mask: ClientIdMask::LOCAL_CLIENT_PID,
        }]) else { return false };
        let Ok(reply) = cookie.reply() else { return false };
        let pid = std::process::id();
        reply.ids.iter().any(|value| value.value.iter().any(|id| *id == pid))
    }

    /// Gamescope base-layer assist (worker-side magic 413094, Python parity).
    pub fn assist_gamescope_focus(&mut self) {
        unsafe {
            let atom = (self.xlib.XInternAtom)(self.display,
                b"GAMESCOPECTRL_BASELAYER_WINDOW\0".as_ptr() as *const i8, 0);
            let mut actual: xlib::Atom = 0;
            let mut format: i32 = 0;
            let mut count: u64 = 0;
            let mut remaining: u64 = 0;
            let mut data: *mut u8 = std::ptr::null_mut();
            if (self.xlib.XGetWindowProperty)(self.display, self.root, atom, 0, 1, 0,
                xlib::XA_CARDINAL, &mut actual, &mut format, &mut count, &mut remaining, &mut data) == 0
                && !data.is_null()
            {
                self.baselayer_previous = Some(*(data as *const u32));
                (self.xlib.XFree)(data as *mut _);
            }
            let steam_game = (self.xlib.XInternAtom)(self.display, b"STEAM_GAME\0".as_ptr() as *const i8, 0);
            let value: u32 = 413094;
            (self.xlib.XChangeProperty)(self.display, self.xid, steam_game, xlib::XA_CARDINAL, 32,
                xlib::PropModeReplace, &value as *const u32 as *const u8, 1);
            let xid_value = self.xid as u32;
            (self.xlib.XChangeProperty)(self.display, self.root, atom, xlib::XA_CARDINAL, 32,
                xlib::PropModeReplace, &xid_value as *const u32 as *const u8, 1);
            (self.xlib.XFlush)(self.display);
        }
    }

    pub fn close(&mut self) {
        unsafe {
            if let Some(previous) = self.baselayer_previous.take() {
                let atom = (self.xlib.XInternAtom)(self.display,
                    b"GAMESCOPECTRL_BASELAYER_WINDOW\0".as_ptr() as *const i8, 0);
                (self.xlib.XChangeProperty)(self.display, self.root, atom, xlib::XA_CARDINAL, 32,
                    xlib::PropModeReplace, &previous as *const u32 as *const u8, 1);
            }
            if self.xid != 0 && !self.adopt {
                (self.xlib.XDestroyWindow)(self.display, self.xid);
            }
            (self.xlib.XFlush)(self.display);
            (self.xlib.XCloseDisplay)(self.display);
        }
    }
}

// The Xlib display is only touched from the glib main-loop thread.
unsafe impl Send for X11Window {}

/// GAMESCOPE_FOCUSED_APP root-atom tracker on the control display.
pub struct GamescopeFocus {
    connection: RustConnection,
    root: u32,
    atom: Option<u32>,
}

impl GamescopeFocus {
    pub fn new(display: &str) -> Result<Self, String> {
        let (connection, screen_index) = x11rb::connect(Some(display))
            .map_err(|e| format!("Gamescope X11 display unavailable: {e}"))?;
        let root = connection.setup().roots[screen_index].root;
        let atom = connection.intern_atom(true, b"GAMESCOPE_FOCUSED_APP")
            .ok().and_then(|cookie| cookie.reply().ok())
            .map(|reply| reply.atom)
            .filter(|atom| *atom != 0);
        Ok(Self { connection, root, atom })
    }

    pub fn focused_app(&self) -> Option<u32> {
        let atom = self.atom?;
        let reply = self.connection.get_property(false, self.root, atom,
            x11rb::protocol::xproto::AtomEnum::CARDINAL, 0, 1)
            .ok()?.reply().ok()?;
        if reply.type_ != u32::from(x11rb::protocol::xproto::AtomEnum::CARDINAL)
            || reply.format != 32 || reply.value_len != 1
        {
            return None;
        }
        reply.value32()?.next().map(|value| value & 0xffff_ffff)
    }
}