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
    /// Steam's on-screen keyboard is up, so the video sits above it.
    pub keyboard: bool,
    /// Where the video window was last put.
    placed: Option<(i32, i32, u32, u32)>,
    /// Keeps its size for SteamOS to scale up.
    pub fixed: bool,
    /// Windows of this process taken inside the video window: the Vulkan sink's.
    embedded: Vec<xlib::Window>,
}

/// Where the video goes while an on-screen keyboard covers the lower half of
/// `area`: the largest area of the same shape in the upper half.
fn above_keyboard((x, y, width, height): (i32, i32, u32, u32)) -> (i32, i32, u32, u32) {
    let (half_width, half_height) = (width / 2, height / 2);
    (x + ((width - half_width) / 2) as i32, y, half_width.max(1), half_height.max(1))
}

pub struct Tick {
    pub size_changed: Option<(u32, u32)>,
    /// Keys pressed in the video window: the X keysym and the text it types.
    pub keys: Vec<(u64, String)>,
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
    /// The video window is a normal window of the app, not one that floats
    /// above it: Gamescope shows HDR and scales with SteamOS's filter only
    /// for the window it counts as the app. It takes the keyboard focus, so
    /// it reads the keys itself. A `fixed` window keeps `width` x `height`
    /// for SteamOS to scale up; otherwise it covers the app window.
    /// `vulkan`: the Vulkan sink shows the video; it makes a window of its
    /// own, which this one takes inside.
    pub fn create(width: u32, height: u32, parent: Option<u64>, adopt: bool, fixed: bool, vulkan: bool) -> Result<Self, String> {
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
                let xid = (xlib.XCreateWindow)(display, root, 0, 0, width, height, 0,
                    24, xlib::InputOutput as u32, info.visual, xlib::CWColormap, &mut attrs);
                if xid == 0 { return Err("XCreateWindow failed".into()); }
                (xlib.XStoreName)(display, xid, b"Greenlight Native Video\0".as_ptr() as *const i8);
                (xlib.XSetWMProtocols)(display, xid, &mut delete_atom, 1);
                (xlib.XSelectInput)(display, xid,
                    xlib::ExposureMask | xlib::ButtonPressMask | xlib::StructureNotifyMask | xlib::KeyPressMask);
                if parent_xid != root && !fixed {
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
                let res_conn = if vulkan { Some(x11rb::connect(None).map_err(|e| format!("X11 res connect: {e}"))?.0) } else { None };
                (xid, parent_xid, res_conn)
            };
            (xlib.XFlush)(display);
            (xlib.XSync)(display, 0);
            let known_children = if let Some(conn) = res_conn.as_ref() {
                conn.query_tree(root as u32).map_err(|e| format!("X11 tree: {e}"))?.reply()
                    .map_err(|e| format!("X11 tree reply: {e}"))?.children.into_iter().collect()
            } else {
                HashSet::new()
            };
            Ok(Self {
                xlib, display, xid, parent: parent_xid, size: (width, height), delete_atom,
                deleted: false, adopt, adopted: false, adopt_started: None, known_children,
                res_conn, root, baselayer_previous: None, keyboard: false, placed: None, fixed,
                embedded: Vec::new(),
            })
        }
    }

    /// The size of the app window, which covers the screen.
    pub fn screen(&self) -> Option<(u32, u32)> {
        if self.parent == 0 { return None; }
        unsafe {
            let mut root: xlib::Window = 0;
            let (mut x, mut y, mut w, mut h, mut b, mut d) = (0, 0, 0u32, 0u32, 0u32, 0u32);
            ((self.xlib.XGetGeometry)(self.display, self.parent, &mut root, &mut x, &mut y, &mut w, &mut h, &mut b, &mut d) != 0)
                .then_some((w, h))
        }
    }

    /// Takes a window this process made at the top level into the video
    /// window, under the overlay: GStreamer's Vulkan sink makes one of its own
    /// instead of drawing into the window it is given.
    fn embed_own_windows(&mut self) -> Result<(), String> {
        let Some(conn) = self.res_conn.as_ref() else { return Ok(()) };
        let children = conn.query_tree(self.root as u32).map_err(|e| format!("X11 tree: {e}"))?.reply()
            .map_err(|e| format!("X11 tree reply: {e}"))?.children;
        let new: Vec<u32> = children.into_iter().filter(|child| self.known_children.insert(*child)).collect();
        let Some(conn) = self.res_conn.as_ref() else { return Ok(()) };
        for child in new {
            if child as xlib::Window == self.xid || !self.owns_window(conn, child) { continue; }
            self.embedded.push(child as xlib::Window);
            unsafe {
                (self.xlib.XReparentWindow)(self.display, child as xlib::Window, self.xid, 0, 0);
                (self.xlib.XLowerWindow)(self.display, child as xlib::Window);
                (self.xlib.XMapWindow)(self.display, child as xlib::Window);
                (self.xlib.XFlush)(self.display);
            }
        }
        Ok(())
    }

    /// Gives the windows taken inside the video window its size.
    fn fit_embedded(&self) {
        for &child in &self.embedded {
            unsafe {
                let (mut r, mut x, mut y, mut w, mut h, mut b, mut d) = (0, 0, 0, 0u32, 0u32, 0u32, 0u32);
                if (self.xlib.XGetGeometry)(self.display, child, &mut r, &mut x, &mut y, &mut w, &mut h, &mut b, &mut d) != 0
                    && (x, y, w, h) != (0, 0, self.size.0, self.size.1)
                {
                    (self.xlib.XMoveResizeWindow)(self.display, child, 0, 0, self.size.0, self.size.1);
                    (self.xlib.XFlush)(self.display);
                }
            }
        }
    }

    pub fn sync(&self) {
        unsafe { (self.xlib.XSync)(self.display, 0); }
    }

    pub fn tick(&mut self, now: f64) -> Result<Tick, String> {
        let mut tick = Tick { size_changed: None, keys: Vec::new(), adopted: false, adopt_timeout: false,
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
                    xlib::KeyPress => {
                        let mut key: xlib::XKeyEvent = event.into();
                        let mut text = [0u8; 32];
                        let mut keysym: xlib::KeySym = 0;
                        let length = (self.xlib.XLookupString)(&mut key, text.as_mut_ptr() as *mut i8, text.len() as i32,
                            &mut keysym, std::ptr::null_mut());
                        let typed = if key.state & (xlib::ControlMask | xlib::Mod1Mask) != 0 { String::new() }
                            else { typed_text(keysym as u64, &text[..length.max(0) as usize]) };
                        tick.keys.push((keysym as u64, typed));
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
        self.embed_own_windows()?;
        self.fit_embedded();
        if self.fixed { return Ok(tick); }
        let target = unsafe {
            let mut root: xlib::Window = 0;
            let mut x: i32 = 0; let mut y: i32 = 0;
            let mut w: u32 = 0; let mut h: u32 = 0; let mut b: u32 = 0; let mut d: u32 = 0;
            let drawable = if self.parent != 0 { self.parent } else { self.xid };
            if (self.xlib.XGetGeometry)(self.display, drawable, &mut root, &mut x, &mut y,
                &mut w, &mut h, &mut b, &mut d) == 0 { return Ok(tick); }
            (x, y, w, h)
        };
        let target = if self.keyboard { above_keyboard(target) } else { target };
        if (target.2, target.3) != self.size || self.placed.is_some_and(|placed| placed != target) {
            unsafe {
                (self.xlib.XMoveResizeWindow)(self.display, self.xid, target.0, target.1, target.2, target.3);
                (self.xlib.XFlush)(self.display);
            }
            if (target.2, target.3) != self.size { tick.size_changed = Some((target.2, target.3)); }
            self.size = (target.2, target.3);
        }
        self.placed = Some(target);
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

/// What the pointer did, as XInput2 reports it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PointerEvent {
    /// How far the device moved, before the cursor stops at a screen edge.
    Moved { dx: f64, dy: f64 },
    /// Where the pointer is in the grab window, in pixels.
    At { x: f64, y: f64 },
    /// An X button: 1–3 left, middle, right; 4–7 the wheel; 8 and 9 back and forward.
    Button { button: u32, down: bool },
}

/// The pointer read through XInput2, on a connection of its own so its grab
/// and events stay apart from the Xlib window's.
pub struct Pointer {
    connection: RustConnection,
    device: u16,
    /// A cursor with no pixels, shown while the pointer is hidden.
    blank: u32,
    /// The window holding the grab, and whether the cursor is hidden.
    grabbed: Option<(u32, bool)>,
    /// Dies with this pointer, whose fd closes then.
    alive: std::sync::Arc<()>,
}

impl Pointer {
    pub fn new() -> Result<Self, String> {
        use x11rb::protocol::xinput::{ConnectionExt as _, Device, EventMask, XIEventMask};
        use x11rb::protocol::xproto::{CreateGCAux, Rectangle};
        let error = |e: &dyn std::fmt::Display| format!("XInput2 pointer: {e}");
        let (connection, screen) = x11rb::connect(None).map_err(|e| error(&e))?;
        let root = connection.setup().roots[screen].root;
        let version = connection.xinput_xi_query_version(2, 2).map_err(|e| error(&e))?.reply().map_err(|e| error(&e))?;
        if (version.major_version, version.minor_version) < (2, 2) {
            return Err(format!("XInput {}.{} is older than 2.2", version.major_version, version.minor_version));
        }
        let devices = connection.xinput_xi_query_device(Device::ALL_MASTER).map_err(|e| error(&e))?
            .reply().map_err(|e| error(&e))?.infos;
        let device = master_pointer(&devices).ok_or("XInput2 pointer: no master pointer")?;
        // Raw motion goes only to the root window, whatever has the grab;
        // it is read from the pointer the grab takes.
        connection.xinput_xi_select_events(root, &[EventMask {
            deviceid: device, mask: vec![XIEventMask::RAW_MOTION],
        }]).map_err(|e| error(&e))?.check().map_err(|e| error(&e))?;
        let (pixmap, gc, blank) = (connection.generate_id().map_err(|e| error(&e))?,
            connection.generate_id().map_err(|e| error(&e))?, connection.generate_id().map_err(|e| error(&e))?);
        connection.create_pixmap(1, pixmap, root, 1, 1).map_err(|e| error(&e))?;
        connection.create_gc(gc, pixmap, &CreateGCAux::new().foreground(0)).map_err(|e| error(&e))?;
        connection.poly_fill_rectangle(pixmap, gc, &[Rectangle { x: 0, y: 0, width: 1, height: 1 }]).map_err(|e| error(&e))?;
        connection.create_cursor(blank, pixmap, pixmap, 0, 0, 0, 0, 0, 0, 0, 0).map_err(|e| error(&e))?;
        connection.free_gc(gc).map_err(|e| error(&e))?;
        connection.free_pixmap(pixmap).map_err(|e| error(&e))?.check().map_err(|e| error(&e))?;
        Ok(Self { connection, device, blank, grabbed: None, alive: std::sync::Arc::new(()) })
    }

    /// The fd to watch for events, and a token that dies with this pointer.
    pub fn watch(&self) -> (std::os::fd::RawFd, std::sync::Weak<()>) {
        use std::os::fd::AsRawFd;
        (self.connection.stream().as_raw_fd(), std::sync::Arc::downgrade(&self.alive))
    }

    /// Grabs the pointer for `window`, the cursor hidden or not, or lets it
    /// go for `None`. A grab the server refuses is tried again on the next
    /// call. Returns whether this connection has the pointer now.
    pub fn hold(&mut self, grab: Option<(u32, bool)>) -> bool {
        use x11rb::protocol::xinput::{ConnectionExt as _, GrabOwner, XIEventMask};
        use x11rb::protocol::xproto::{GrabMode, GrabStatus};
        if self.grabbed == grab { return grab.is_some(); }
        let before = self.grabbed;
        if self.grabbed.take().is_some() {
            let _ = self.connection.xinput_xi_ungrab_device(x11rb::CURRENT_TIME, self.device);
        }
        if let Some((window, hidden)) = grab {
            let cursor = if hidden { self.blank } else { x11rb::NONE };
            let mask = XIEventMask::MOTION | XIEventMask::BUTTON_PRESS | XIEventMask::BUTTON_RELEASE;
            let status = self.connection.xinput_xi_grab_device(window, x11rb::CURRENT_TIME, cursor, self.device,
                GrabMode::ASYNC, GrabMode::ASYNC, GrabOwner::NO_OWNER, &[mask.into()])
                .ok().and_then(|cookie| cookie.reply().ok()).map(|reply| reply.status);
            if status == Some(GrabStatus::SUCCESS) { self.grabbed = grab; }
        }
        let _ = self.connection.flush();
        if self.grabbed != before { eprintln!("pw-mouse grab {:?}", self.grabbed); }
        self.grabbed.is_some()
    }

    /// The events that arrived since the last call.
    pub fn events(&mut self) -> Vec<PointerEvent> {
        let mut events = Vec::new();
        while let Ok(Some(event)) = self.connection.poll_for_event() { events.extend(pointer_events(event)); }
        events
    }
}

/// The master pointer among the master devices.
fn master_pointer(devices: &[x11rb::protocol::xinput::XIDeviceInfo]) -> Option<u16> {
    use x11rb::protocol::xinput::DeviceType;
    devices.iter().find(|device| device.type_ == DeviceType::MASTER_POINTER).map(|device| device.deviceid)
}

/// What an X event says about the pointer. A button comes with where it
/// was pressed, which a click without motion before it needs.
fn pointer_events(event: x11rb::protocol::Event) -> Vec<PointerEvent> {
    use x11rb::protocol::Event;
    let at = |x: i32, y: i32| PointerEvent::At { x: f64::from(x) / 65536.0, y: f64::from(y) / 65536.0 };
    match event {
        Event::XinputRawMotion(raw) => {
            let (dx, dy) = raw_motion(&raw.valuator_mask, &raw.axisvalues_raw);
            vec![PointerEvent::Moved { dx, dy }]
        }
        Event::XinputMotion(motion) => vec![at(motion.event_x, motion.event_y)],
        Event::XinputButtonPress(press) => vec![at(press.event_x, press.event_y),
            PointerEvent::Button { button: press.detail, down: true }],
        Event::XinputButtonRelease(release) => vec![at(release.event_x, release.event_y),
            PointerEvent::Button { button: release.detail, down: false }],
        _ => Vec::new(),
    }
}

/// The motion along valuators 0 and 1 of a raw event. Its values come one
/// for each bit set in the mask, in bit order.
fn raw_motion(mask: &[u32], values: &[x11rb::protocol::xinput::Fp3232]) -> (f64, f64) {
    let set = |bit: u32| mask.first().is_some_and(|word| word & (1 << bit) != 0);
    let value = |index: usize| values.get(index).map_or(0.0, |v| f64::from(v.integral) + f64::from(v.frac) / 4_294_967_296.0);
    let dx = if set(0) { value(0) } else { 0.0 };
    let dy = if set(1) { value(usize::from(set(0))) } else { 0.0 };
    (dx, dy)
}

/// Whether Gamescope shows HDR now. Its Vulkan layer goes by the same fact
/// when it offers a window the HDR10 colour space.
pub fn hdr_output() -> bool {
    let Ok((connection, screen)) = x11rb::connect(None) else { return false };
    let root = connection.setup().roots[screen].root;
    let value = || {
        let atom = connection.intern_atom(true, b"GAMESCOPE_HDR_OUTPUT_FEEDBACK").ok()?.reply().ok()?.atom;
        connection.get_property(false, root, atom, x11rb::protocol::xproto::AtomEnum::CARDINAL, 0, 1)
            .ok()?.reply().ok()?.value32()?.next()
    };
    value().is_some_and(|value| value != 0)
}

/// GAMESCOPE_FOCUSED_APP root-atom tracker on the control display.
pub struct GamescopeFocus {
    connection: RustConnection,
    root: u32,
    atom: Option<u32>,
    /// STEAM_INPUT_FOCUS, which Steam sets on its overlay window.
    input_focus: Option<u32>,
    /// Steam's overlay window, once found.
    steam_overlay: Option<u32>,
}

/// The value of STEAM_INPUT_FOCUS while Steam shows its on-screen keyboard:
/// the game keeps the keys and Steam takes only the pointer.
const KEYBOARD_SHOWN: u32 = 2;

impl GamescopeFocus {
    pub fn new(display: &str) -> Result<Self, String> {
        let (connection, screen_index) = x11rb::connect(Some(display))
            .map_err(|e| format!("Gamescope X11 display unavailable: {e}"))?;
        let root = connection.setup().roots[screen_index].root;
        let atom = connection.intern_atom(true, b"GAMESCOPE_FOCUSED_APP")
            .ok().and_then(|cookie| cookie.reply().ok())
            .map(|reply| reply.atom)
            .filter(|atom| *atom != 0);
        let input_focus = connection.intern_atom(true, b"STEAM_INPUT_FOCUS")
            .ok().and_then(|cookie| cookie.reply().ok())
            .map(|reply| reply.atom)
            .filter(|atom| *atom != 0);
        Ok(Self { connection, root, atom, input_focus, steam_overlay: None })
    }

    fn cardinal(&self, window: u32, atom: u32) -> Option<u32> {
        let reply = self.connection.get_property(false, window, atom,
            x11rb::protocol::xproto::AtomEnum::CARDINAL, 0, 1).ok()?.reply().ok()?;
        reply.value32()?.next()
    }

    /// Whether Steam's on-screen keyboard is up.
    pub fn keyboard_shown(&mut self) -> bool {
        let Some(atom) = self.input_focus else { return false };
        if let Some(value) = self.steam_overlay.and_then(|window| self.cardinal(window, atom)) {
            return value == KEYBOARD_SHOWN;
        }
        // Steam's overlay is the root child that carries the property.
        let children = self.connection.query_tree(self.root).ok().and_then(|cookie| cookie.reply().ok())
            .map(|reply| reply.children).unwrap_or_default();
        self.steam_overlay = children.into_iter().find(|window| self.cardinal(*window, atom).is_some());
        self.steam_overlay.and_then(|window| self.cardinal(window, atom)) == Some(KEYBOARD_SHOWN)
    }

    /// The filter SteamOS scales the focused app with, as Gamescope reports it.
    pub fn scaling_filter(&self) -> Option<&'static str> {
        let atom = |name: &[u8]| self.connection.intern_atom(true, name).ok()?.reply().ok().map(|reply| reply.atom)
            .filter(|atom| *atom != 0);
        if atom(b"GAMESCOPE_FSR_FEEDBACK").and_then(|atom| self.cardinal(self.root, atom)) == Some(1) {
            return Some("FSR");
        }
        let wanted = atom(b"GAMESCOPE_NEW_SCALING_FILTER").and_then(|atom| self.cardinal(self.root, atom));
        Some(scaling_filter_name(wanted.unwrap_or(0)))
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
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_video_keeps_its_shape_in_the_upper_half_above_the_keyboard() {
        assert_eq!(above_keyboard((0, 0, 1920, 1080)), (480, 0, 960, 540));
        assert_eq!(above_keyboard((10, 20, 1280, 800)), (330, 20, 640, 400));
        // A window too small to halve still has a size.
        assert_eq!(above_keyboard((0, 0, 1, 1)), (0, 0, 1, 1));
    }

    #[test]
    fn raw_motion_reads_the_values_of_the_set_valuators() {
        use x11rb::protocol::xinput::Fp3232;
        let half = Fp3232 { integral: -1, frac: 1 << 31 };
        let three = Fp3232 { integral: 3, frac: 0 };
        assert_eq!(raw_motion(&[0b11], &[three, half]), (3.0, -0.5));
        // Only y moved: its value is the first one.
        assert_eq!(raw_motion(&[0b10], &[half]), (0.0, -0.5));
        // A wheel valuator after x does not count as y.
        assert_eq!(raw_motion(&[0b1001], &[three, half]), (3.0, 0.0));
        assert_eq!(raw_motion(&[], &[]), (0.0, 0.0));
    }

    #[test]
    fn the_grab_takes_the_master_pointer_not_the_keyboard() {
        use x11rb::protocol::xinput::{DeviceType, XIDeviceInfo};
        let device = |deviceid, type_| XIDeviceInfo { deviceid, type_, attachment: 0, enabled: true, name: Vec::new(), classes: Vec::new() };
        assert_eq!(master_pointer(&[device(3, DeviceType::MASTER_KEYBOARD), device(2, DeviceType::MASTER_POINTER)]), Some(2));
        assert_eq!(master_pointer(&[device(3, DeviceType::MASTER_KEYBOARD)]), None);
    }

    #[test]
    fn a_button_comes_with_where_it_was_pressed() {
        use x11rb::protocol::{Event, xinput::ButtonPressEvent};
        let press = ButtonPressEvent { detail: 1, event_x: 100 << 16, event_y: (50 << 16) + (1 << 15), ..Default::default() };
        assert_eq!(pointer_events(Event::XinputButtonPress(press.clone())), [
            PointerEvent::At { x: 100.0, y: 50.5 }, PointerEvent::Button { button: 1, down: true }]);
        assert_eq!(pointer_events(Event::XinputButtonRelease(press))[1], PointerEvent::Button { button: 1, down: false });
    }
}

/// The text a key typed. XLookupString gives Latin-1 bytes, and nothing for a
/// character outside Latin-1, which an X keysym then carries as Unicode.
pub fn typed_text(keysym: u64, latin1: &[u8]) -> String {
    match keysym {
        0x0100_0000..=0x0110_ffff => char::from_u32((keysym - 0x0100_0000) as u32).map(String::from).unwrap_or_default(),
        _ => latin1.iter().map(|&byte| byte as char).collect(),
    }
}

/// Gamescope's name for its scaling filter number.
pub fn scaling_filter_name(filter: u32) -> &'static str {
    match filter {
        1 => "Nearest",
        2 => "FSR",
        3 => "NIS",
        4 => "Integer",
        5 => "Sharp",
        _ => "Linear",
    }
}

/// The Windows virtual-key code of a key that types no text, by its X keysym.
pub fn virtual_key(keysym: u64) -> Option<u8> {
    Some(match keysym {
        0xff08 => 0x08,
        0xff09 => 0x09,
        0xff0d | 0xff8d => 0x0d,
        0xff1b => 0x1b,
        0xff55 => 0x21,
        0xff56 => 0x22,
        0xff57 => 0x23,
        0xff50 => 0x24,
        0xff51 => 0x25,
        0xff52 => 0x26,
        0xff53 => 0x27,
        0xff54 => 0x28,
        0xffff => 0x2e,
        _ => return None,
    })
}

#[cfg(test)]
mod key_tests {
    #[test]
    fn named_keys_get_the_codes_the_app_sends() {
        assert_eq!(super::virtual_key(0xff0d), Some(0x0d));
        assert_eq!(super::virtual_key(0xff08), Some(0x08));
        assert_eq!(super::virtual_key(0xff54), Some(0x28));
        assert_eq!(super::virtual_key(0x61), None);
    }

    #[test]
    fn typed_text_keeps_accents_and_unicode_letters() {
        assert_eq!(super::typed_text(0xe9, &[0xe9]), "é");
        assert_eq!(super::typed_text(0x61, b"a"), "a");
        // Cyrillic "я" as a Unicode keysym.
        assert_eq!(super::typed_text(0x0100_044f, &[]), "я");
    }

    #[test]
    fn names_gamescopes_scaling_filters() {
        assert_eq!(super::scaling_filter_name(2), "FSR");
        assert_eq!(super::scaling_filter_name(3), "NIS");
        assert_eq!(super::scaling_filter_name(0), "Linear");
        assert_eq!(super::scaling_filter_name(99), "Linear");
    }
}
