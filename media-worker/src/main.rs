//! Native GStreamer media worker for Playwake.
//!
//! Speaks the same JSON-lines stdio protocol as `media/gstreamer_player.py`
//! (host side: `src/media.rs`). Milestones 1-3: webrtc glue, handshake,
//! pipeline, probes, diagnostics, input loop, evdev routing, X11 embedding,
//! Vulkan adoption, Gamescope focus and the menu-input mode. Overlay drawing
//! and latency trials land in milestone 4.

mod clock;
mod protocol;

#[cfg(target_os = "linux")]
mod handshake;
#[cfg(target_os = "linux")]
mod hud;
#[cfg(target_os = "linux")]
mod input;
#[cfg(target_os = "linux")]
mod overlay;
#[cfg(target_os = "linux")]
mod pipeline;
#[cfg(target_os = "linux")]
use playwake_core::route;
#[cfg(target_os = "linux")]
mod state;
#[cfg(target_os = "linux")]
mod transport;
#[cfg(target_os = "linux")]
mod trial;
#[cfg(target_os = "linux")]
mod pc;
#[cfg(target_os = "linux")]
mod present;
#[cfg(target_os = "linux")]
mod scene;
#[cfg(target_os = "linux")]
mod webrtc;
#[cfg(target_os = "linux")]
mod x11;

#[cfg(target_os = "linux")]
pub static LIMIT_KBPS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(20000);

#[cfg(target_os = "linux")]
use gstreamer as gst;
#[cfg(target_os = "linux")]
use gstreamer::glib;
#[cfg(target_os = "linux")]
use gstreamer::prelude::*;
#[cfg(target_os = "linux")]
use gstreamer_webrtc as gst_webrtc;
#[cfg(target_os = "linux")]
use std::sync::{Arc, Mutex};

#[cfg(target_os = "linux")]
type Slot = Arc<Mutex<Option<Worker>>>;

/// What a worker shows.
#[cfg(target_os = "linux")]
enum Source {
    Xbox,
    Pc(protocol::Pc),
    /// The LSFG preview's scene at this rate, with the HUD's title.
    Scene { fps: u32, title: String },
}

#[cfg(target_os = "linux")]
struct Worker {
    pipeline: gst::Pipeline,
    /// The WebRTC session of an Xbox stream.
    rtc: Option<Arc<Mutex<webrtc::Rtc>>>,
    /// The session of a PC stream, once it has started.
    pc: Option<Arc<gamestream::Stream>>,
    state: Arc<Mutex<state::State>>,
    video: Option<pipeline::VideoBranch>,
    x11: Option<x11::X11Window>,
    overlay: Option<overlay::Overlay>,
    focus: Option<x11::GamescopeFocus>,
    target_app: Option<u32>,
    input: input::Input,
    renderer: String,
    window: Option<u64>,
    lsfg: bool,
    volume: u32,
    tracked: std::collections::HashSet<String>,
    quit: glib::MainLoop,
    start_time: f64,
    input_retry: bool,
    hud_active: bool,
    /// Shows the LSFG preview's scene instead of a stream.
    preview: bool,
    /// Removes the pipeline's error watch when dropped, so it lives here.
    bus_watch: Option<gst::bus::BusWatchGuard>,
    /// The pointer of a PC stream's video window.
    pointer: Option<x11::Pointer>,
    mouse: input::Mouse,
    keys: input::Keys,
    /// Gamescope shows this app, not Steam or another one.
    focused: bool,
}

#[cfg(target_os = "linux")]
fn main() {
    if std::env::args().nth(1).as_deref() == Some("--selftest") {
        std::process::exit(selftest());
    }
    if std::env::args().nth(1).as_deref() == Some("--menu-input") {
        menu_input_mode();
        return;
    }
    // The app discards what the worker writes to stderr; this keeps it in a file.
    if let Some(path) = std::env::var_os("ALLY_WORKER_STDERR") {
        use std::os::fd::AsRawFd;
        if let Ok(file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            unsafe { libc::dup2(file.as_raw_fd(), 2); }
        }
    }
    if let Err(error) = run() {
        protocol::error(error);
        std::process::exit(1);
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("playwake-media: the native worker is Linux-only");
    std::process::exit(2);
}

/// Install-time verification replacement for the old PyGObject checks.
#[cfg(target_os = "linux")]
fn selftest() -> i32 {
    if let Err(error) = gst::init() {
        println!("[FAIL] GStreamer init: {error}");
        return 1;
    }
    let mut failed = false;
    let required = ["webrtcbin", "rtph264depay", "h264parse", "vulkanupload", "vulkansink",
        "vulkancolorconvert", "videoconvert", "videoscale", "ximagesink", "rtpopusdepay",
        "opusdec", "audioconvert", "audioresample", "volume", "queue", "tee", "fakesink", "appsrc"];
    for element in required {
        if gst::ElementFactory::find(element).is_some() {
            println!("[ok]   element {element}");
        } else {
            println!("[FAIL] element {element}");
            failed = true;
        }
    }
    for group in [["vah264dec", "avdec_h264"], ["pulsesink", "autoaudiosink"]] {
        if group.iter().any(|e| gst::ElementFactory::find(e).is_some()) {
            println!("[ok]   element {}", group.join(" or "));
        } else {
            println!("[FAIL] element {}", group.join(" or "));
            failed = true;
        }
    }
    if gst::ElementFactory::find("vapostproc").is_none() {
        println!("[warn] vapostproc missing: VA-API GPU path unavailable, software decode still works");
    }
    let devices = evdev::enumerate().count();
    if devices > 0 {
        println!("[ok]   evdev ({devices} input devices)");
    } else {
        println!("[FAIL] evdev: no readable /dev/input devices");
        failed = true;
    }
    if failed { 1 } else { 0 }
}

#[cfg(target_os = "linux")]
fn run() -> Result<(), String> {
    gst::init().map_err(|e| format!("GStreamer init: {e}"))?;
    let quit = glib::MainLoop::new(None, false);
    let slot: Slot = Arc::new(Mutex::new(None));

    {
        let slot = slot.clone();
        let quit = quit.clone();
        std::thread::spawn(move || {
            use std::io::BufRead;
            let stdin = std::io::stdin();
            for line in stdin.lock().lines() {
                let Ok(line) = line else { break };
                let slot = slot.clone();
                let quit = quit.clone();
                glib::MainContext::default().invoke(move || {
                    if let Some(command) = protocol::parse(&line) {
                        dispatch(&slot, &quit, command);
                    }
                });
            }
            glib::MainContext::default().invoke(move || quit.quit());
        });
    }

    // 1 s HUD cadence while the overlay menu or diagnostics are visible
    // (Python sync_hud_timer). Opening the menu shows live values too.
    {
        let slot = slot.clone();
        glib::timeout_add_local(std::time::Duration::from_secs(1), move || {
            let mut guard = slot.lock().unwrap();
            if let Some(w) = guard.as_mut() {
                let wants_stats = w.input.route.overlay || w.state.lock().unwrap().diagnostics;
                if wants_stats {
                    if !w.hud_active {
                        w.hud_active = true;
                        let mut st = w.state.lock().unwrap();
                        st.reset_hud();
                        st.hud_enabled = true;
                    }
                    let queue = w.video.as_ref().and_then(|v| v.queue.clone());
                    let element = w.rtc.as_ref().map(|rtc| rtc.lock().unwrap().element.clone());
                    hud::refresh_hud(&w.state, element.as_ref(), queue.as_ref());
                } else if w.hud_active {
                    w.hud_active = false;
                    let mut st = w.state.lock().unwrap();
                    st.hud_enabled = false;
                    st.reset_hud();
                }
            }
            glib::ControlFlow::Continue
        });
    }

    // X11 geometry tick: 80 ms covers both Python cadences (120/80 ms).
    {
        let slot = slot.clone();
        let quit = quit.clone();
        glib::timeout_add_local(std::time::Duration::from_millis(80), move || {
            let now = clock::now_ms();
            let mut guard = slot.lock().unwrap();
            if let Some(w) = guard.as_mut() {
                // A PC stream takes typed text, so its picture moves out of
                // the way of Steam's on-screen keyboard.
                let keyboard = w.pc.is_some() && w.focus.as_mut().is_some_and(|focus| focus.keyboard_shown());
                if let Some(x11) = w.x11.as_mut() {
                    x11.keyboard = keyboard;
                    match x11.tick(now) {
                        Ok(tick) => {
                            // The video window has the keyboard focus: what is
                            // typed goes to the PC, as the app sent it before.
                            if w.preview {
                                for key in tick.keys.iter().filter(|key| key.down) {
                                    match key.keysym {
                                        0xff0d | 0xff8d => protocol::event("preview-toggle", serde_json::json!({})),
                                        0xff1b => protocol::event("preview-back", serde_json::json!({})),
                                        _ => {}
                                    }
                                }
                            }
                            if let Some(pc) = w.pc.as_ref().filter(|_| !w.input.route.overlay) {
                                w.keys.send(pc.as_ref(), &tick.keys);
                            }
                            if tick.adopted {
                                protocol::event("vulkan-window-embedded", serde_json::json!({}));
                            }
                            if x11.fixed {
                                let screen = x11.screen();
                                let filter = w.focus.as_ref().and_then(|focus| focus.scaling_filter()).unwrap_or("Linear");
                                w.state.lock().unwrap().scaled_to = screen.map(|screen| (screen, filter));
                            }
                            if let Some((width, height)) = tick.size_changed {
                                protocol::event("video-surface", serde_json::json!({ "width": width, "height": height }));
                                w.state.lock().unwrap().viewport = (width, height);
                            }
                            if let Some(overlay) = w.overlay.as_mut() {
                                overlay.menu = w.input.route.overlay;
                                let st = w.state.lock().unwrap();
                                overlay.diagnostics = st.diagnostics;
                                overlay.compact = st.hud_compact.clone();
                                overlay.compact_rate = st.hud_compact_rate.clone();
                                overlay.metrics = st.hud_metrics.clone();
                                drop(st);
                                let size = x11.size;
                                let buttons = tick.buttons.clone();
                                for (window, button, x, y) in buttons {
                                    for event in overlay.handle_button(window, button, x, y) {
                                        match event {
                                            overlay::OverlayEvent::Toggle => {
                                                let events = if w.input.route.overlay {
                                                    w.input.route.close_overlay(false)
                                                } else {
                                                    w.input.route.open_overlay()
                                                };
                                                input::emit_route_events(events);
                                            }
                                            overlay::OverlayEvent::Select(index) => {
                                                protocol::event("stream-overlay-key",
                                                    serde_json::json!({ "key": "select", "index": index }));
                                            }
                                            overlay::OverlayEvent::HintVisibility(visible) => {
                                                protocol::event("stream-overlay-hint",
                                                    serde_json::json!({ "visible": visible }));
                                            }
                                        }
                                    }
                                }
                                for window in &tick.exposes {
                                    if *window == overlay.xid { overlay.invalidate(); }
                                }
                                for event in overlay.update(size, now / 1000.0) {
                                    if let overlay::OverlayEvent::HintVisibility(visible) = event {
                                        protocol::event("stream-overlay-hint",
                                            serde_json::json!({ "visible": visible }));
                                    }
                                }
                                w.state.lock().unwrap().overlay_layer = overlay.layer.clone();
                            }
                            if tick.adopt_timeout {
                                protocol::error("Could not attach the Vulkan video window");
                                quit.quit();
                            }
                            if x11.deleted {
                                quit.quit();
                            }
                        }
                        Err(error) => {
                            protocol::error(error);
                            quit.quit();
                        }
                    }
                }
            }
            glib::ControlFlow::Continue
        });
    }

    // Gamescope focused-app poll (16 ms, Python update_input_focus).
    {
        let slot = slot.clone();
        glib::timeout_add_local(std::time::Duration::from_millis(16), move || {
            let mut guard = slot.lock().unwrap();
            if let Some(w) = guard.as_mut() {
                if let (Some(focus), Some(target)) = (w.focus.as_ref(), w.target_app) {
                    let app = focus.focused_app();
                    // The gamepad moves over the on-screen keyboard, not the game.
                    let typing = w.x11.as_ref().is_some_and(|x11| x11.keyboard);
                    w.focused = app.is_none() || app == Some(target);
                    let events = w.input.route.focus(w.focused && !typing);
                    input::emit_route_events(events);
                }
                w.sync_pc_input();
            }
            glib::ControlFlow::Continue
        });
    }

    quit.run();

    if let Some(mut w) = slot.lock().unwrap().take() {
        if let Some(pc) = &w.pc {
            w.keys.hold(pc.as_ref(), false);
            w.mouse.update(pc.as_ref(), false, [], (0, 0), (0, 0));
        }
        if let Some(overlay) = w.overlay.as_mut() { overlay.close(); }
        if let Some(x11) = w.x11.as_mut() { x11.close(); }
        if let Some(pc) = &w.pc { pc.stop(); }
        let _ = w.pipeline.set_state(gst::State::Null);
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn dispatch(slot: &Slot, quit: &glib::MainLoop, command: protocol::Command) {
    use protocol::Command;
    match command {
        Command::Start { bitrate, width, height, benchmark, capture_diagnostics, renderer, lsfg,
                         interactive, russian, xbox_palette, compact_stats, volume, console_name, jitter_ms, pc } => {
            let source = match pc { Some(pc) => Source::Pc(pc), None => Source::Xbox };
            start(slot, quit, bitrate, width, height, benchmark, capture_diagnostics, renderer, lsfg,
                interactive, russian, xbox_palette, compact_stats, volume, console_name, jitter_ms, source);
        }
        Command::Preview { width, height, fps, lsfg, performance, flow_scale, russian, xbox_palette } => {
            let title = match (lsfg, performance, russian) {
                (false, _, true) => "LSFG выкл.".to_string(),
                (false, _, false) => "LSFG off".to_string(),
                (true, true, _) => format!("LSFG 2× Performance · Flow Scale {flow_scale}"),
                (true, false, true) => format!("LSFG 2× Качество · Flow Scale {flow_scale}"),
                (true, false, false) => format!("LSFG 2× Quality · Flow Scale {flow_scale}"),
            };
            start(slot, quit, 20000, width, height, false, false, "vulkan".into(), lsfg,
                true, russian, xbox_palette, true, 100, "Playwake".into(), None, Source::Scene { fps, title });
        }
        Command::Answer { sdp } => {
            if let Some(rtc) = slot.lock().unwrap().as_ref().and_then(|w| w.rtc.as_ref()) {
                webrtc::set_answer(rtc, &sdp);
            }
        }
        Command::Ice { candidates } => {
            if let Some(rtc) = slot.lock().unwrap().as_ref().and_then(|w| w.rtc.as_ref()) {
                webrtc::add_ice(rtc, &candidates);
            }
        }
        Command::Stats => {
            if let Some(w) = slot.lock().unwrap().as_ref() {
                let queue = w.video.as_ref().and_then(|v| v.queue.clone());
                let element = w.rtc.as_ref().map(|rtc| rtc.lock().unwrap().element.clone());
                let mut st = w.state.lock().unwrap();
                hud::emit_stats(&mut st, queue.as_ref(), element.as_ref());
            }
        }
        Command::Pulse { axis, buttons, duration, trial } => {
            if let Some(w) = slot.lock().unwrap().as_mut() {
                w.input.pulse(axis, buttons, duration, trial);
                w.send_input(slot);
            }
        }
        Command::Gamepad { axes, buttons, lt, rt } => {
            if let Some(w) = slot.lock().unwrap().as_mut() {
                w.input.set_gamepad(&axes, buttons, lt, rt);
                w.send_input(slot);
            }
        }
        Command::DiagnosticsToggle => {
            if let Some(w) = slot.lock().unwrap().as_ref() {
                let mut st = w.state.lock().unwrap();
                st.diagnostics = !st.diagnostics;
                protocol::event("stream-diagnostics", serde_json::json!({ "enabled": st.diagnostics }));
            }
        }
        Command::OverlayOpen => {
            if let Some(w) = slot.lock().unwrap().as_mut() {
                let events = w.input.route.open_overlay();
                input::emit_route_events(events);
                w.sync_pc_input();
            }
        }
        Command::OverlayClose => {
            if let Some(w) = slot.lock().unwrap().as_mut() {
                let events = w.input.route.close_overlay(false);
                input::emit_route_events(events);
                w.sync_pc_input();
            }
        }
        Command::XboxGuide => {
            if let Some(w) = slot.lock().unwrap().as_mut() {
                let events = w.input.route.close_overlay(true);
                input::emit_route_events(events);
                w.sync_pc_input();
            }
        }
        Command::MouseModeToggle => {
            if let Some(w) = slot.lock().unwrap().as_mut() {
                if let Some(pc) = &w.pc { w.mouse.toggle_mode(pc.as_ref()); }
                let desktop = w.mouse.mode == input::MouseMode::Desktop;
                if let Some(overlay) = w.overlay.as_mut() { overlay.desktop_mouse = desktop; }
                protocol::event("stream-mouse-mode", serde_json::json!({ "mode": if desktop { "desktop" } else { "game" } }));
                w.sync_pc_input();
            }
        }
        Command::Type { text } => {
            if let Some(pc) = slot.lock().unwrap().as_ref().and_then(|w| w.pc.as_ref()) { pc.text(&text); }
        }
        Command::Key { code } => {
            if let Some(pc) = slot.lock().unwrap().as_ref().and_then(|w| w.pc.as_ref()) {
                pc.key(code, true, 0);
                pc.key(code, false, 0);
            }
        }
        Command::InputFocus { active } => {
            if let Some(w) = slot.lock().unwrap().as_mut() {
                w.focused = active;
                let events = w.input.route.focus(active);
                input::emit_route_events(events);
                w.sync_pc_input();
            }
        }
        Command::Trial { restore_view } => {
            let mut guard = slot.lock().unwrap();
            let Some(w) = guard.as_mut() else { return };
            {
                let mut st = w.state.lock().unwrap();
                if st.trial.is_some() { return; }
                st.trial = Some(trial::TrialState { restore_view, ..Default::default() });
            }
            let slot_actuate = slot.clone();
            glib::timeout_add(std::time::Duration::from_millis(300), move || {
                let mut guard = slot_actuate.lock().unwrap();
                let Some(w) = guard.as_mut() else { return glib::ControlFlow::Continue };
                let now = clock::now_ms();
                let mut st = w.state.lock().unwrap();
                let Some(trial_state) = st.trial.as_mut() else { return glib::ControlFlow::Continue };
                if trial_state.baseline_moved || trial_state.frames < 4 {
                    st.trial = None;
                    protocol::event("trial-error", serde_json::json!({ "message": "Hold the camera still and try again" }));
                    return glib::ControlFlow::Continue;
                }
                trial_state.started = Some(now);
                let restore = trial_state.restore_view;
                drop(st);
                w.input.pulse(0.9, 0, 230, true);
                w.send_input(&slot_actuate);
                if restore {
                    let slot_restore = slot_actuate.clone();
                    glib::timeout_add(std::time::Duration::from_millis(550), move || {
                        let mut guard = slot_restore.lock().unwrap();
                        if let Some(w) = guard.as_mut() {
                            w.input.pulse(-0.9, 0, 230, false);
                            w.send_input(&slot_restore);
                        }
                        glib::ControlFlow::Continue
                    });
                }
                glib::ControlFlow::Continue
            });
            let slot_timeout = slot.clone();
            glib::timeout_add(std::time::Duration::from_millis(1500), move || {
                let mut guard = slot_timeout.lock().unwrap();
                if let Some(w) = guard.as_mut() {
                    let mut st = w.state.lock().unwrap();
                    if st.trial.is_some() {
                        st.trial = None;
                        protocol::event("trial-error", serde_json::json!({ "message": "No confirmed camera turn" }));
                    }
                }
                glib::ControlFlow::Continue
            });
        }
        Command::OverlaySelection { index } => {
            if let Some(w) = slot.lock().unwrap().as_mut() {
                if let Some(overlay) = w.overlay.as_mut() {
                    overlay.selection = index;
                }
            }
        }
        Command::OverlayHint => {
            if let Some(w) = slot.lock().unwrap().as_mut() {
                if let Some(overlay) = w.overlay.as_mut() {
                    overlay.show_startup_hint();
                }
            }
        }
        Command::Stop => quit.quit(),
    }
}

#[cfg(target_os = "linux")]
#[allow(clippy::too_many_arguments)]
fn start(
    slot: &Slot, quit: &glib::MainLoop,
    bitrate: u32, width: u32, height: u32, benchmark: bool, capture_diagnostics: bool,
    renderer: String, lsfg: bool, interactive: bool, russian: bool, xbox_palette: bool,
    compact_stats: bool, volume: u32, console_name: String, jitter_ms: Option<u32>, source: Source,
) {
    LIMIT_KBPS.store(bitrate, std::sync::atomic::Ordering::Relaxed);
    // ALLY_JITTER_MS overrides the setting for diagnostics.
    let jitter: u32 = std::env::var("ALLY_JITTER_MS").ok().and_then(|v| v.parse().ok()).or(jitter_ms).unwrap_or(20);
    let parent = std::env::var("GREENLIGHT_VIDEO_PARENT_XID").ok().and_then(|v| parse_xid(&v));
    let adopt = renderer == "vulkan" && parent.is_none();
    let mut x11_window = None;
    // Always create our own child (inside the GPUI parent when embedded):
    // ximagesink paints for the default visual, the GPUI parent is ARGB32.
    let window = if std::env::var_os("DISPLAY").is_some() {
        let fixed = matches!(&source, Source::Pc(pc) if pc.scaled);
        match x11::X11Window::create(width, height, parent, adopt, fixed, renderer == "vulkan") {
            Ok(mut created) => {
                if !adopt && std::env::var("GREENLIGHT_GAMESCOPE_FOCUS").as_deref() == Ok("1") {
                    created.assist_gamescope_focus();
                }
                let xid = if adopt { None } else { Some(created.xid as u64) };
                eprintln!("pw-x11 video child={:#x} parent={:?} adopt={}", created.xid, parent, adopt);
                if !adopt {
                    protocol::event("video-surface", serde_json::json!({ "width": width, "height": height }));
                }
                x11_window = Some(created);
                xid
            }
            Err(error) => { protocol::error(error); return; }
        }
    } else {
        None
    };
    let focus = std::env::var("ALLY_GAMESCOPE_CONTROL_DISPLAY").ok()
        .filter(|_| std::env::var("SteamAppId").ok().is_some_and(|v| v.parse::<u32>().is_ok_and(|id| id != 0)))
        .and_then(|display| x11::GamescopeFocus::new(&display).ok());
    let target_app = std::env::var("SteamAppId").ok().and_then(|v| v.parse().ok());
    let start_time = clock::now_ms();
    let transport = Arc::new(Mutex::new(transport::Transport::default()));
    let pipeline = gst::Pipeline::builder().name("greenlight-native").build();
    let rtc = if !matches!(source, Source::Xbox) { None } else {
        match webrtc::build(&pipeline, jitter, transport.clone(), capture_diagnostics) {
            Ok(rtc) => Some(Arc::new(Mutex::new(rtc))),
            Err(error) => { protocol::error(error); return; }
        }
    };
    let state = Arc::new(Mutex::new(state::State::new(width, height, benchmark, capture_diagnostics, transport.clone())));
    if compact_stats {
        state.lock().unwrap().diagnostics = true;
        protocol::event("stream-diagnostics", serde_json::json!({ "enabled": true }));
    }

    let mut input = input::Input::new(start_time);
    let preview = matches!(source, Source::Scene { .. });
    // The app takes the gamepad during the preview.
    if !preview { input.connect_gamepad(); }
    if let Some(x) = x11_window.as_ref() { x.sync(); }
    let overlay = if interactive {
        x11_window.as_ref()
            .and_then(|x| overlay::Overlay::new(x.display, x.xid as u32, matches!(source, Source::Pc(_))).ok())
            .map(|mut overlay| {
                overlay.russian = russian;
                overlay.xbox_palette = xbox_palette;
                overlay.console_name = console_name.clone();
                overlay
            })
    } else {
        None
    };
    {
        let mut st = state.lock().unwrap();
        st.lsfg = lsfg;
        st.russian = russian;
        if let Source::Scene { title, .. } = &source { st.preview = Some(title.clone()); }
    }

    // A PC stream takes the mouse of its video window.
    let pointer = if matches!(source, Source::Pc(_)) && x11_window.is_some() {
        x11::Pointer::new().inspect_err(|error| eprintln!("pw-mouse {error}")).ok()
    } else {
        None
    };
    if let Some((fd, alive)) = pointer.as_ref().map(x11::Pointer::watch) {
        let slot = slot.clone();
        glib::unix_fd_add_local(fd, glib::IOCondition::IN, move |_, _| {
            let mut guard = slot.lock().unwrap();
            // The fd is closed once its worker is replaced or gone.
            if alive.strong_count() == 0 { return glib::ControlFlow::Break; }
            let Some(w) = guard.as_mut() else { return glib::ControlFlow::Break };
            w.sync_pc_input();
            glib::ControlFlow::Continue
        });
    }

    // A PC stream sends keys as they come, not on the next geometry tick.
    if let Some((fd, alive)) = x11_window.as_ref().filter(|_| matches!(source, Source::Pc(_))).map(x11::X11Window::watch) {
        let slot = slot.clone();
        glib::unix_fd_add_local(fd, glib::IOCondition::IN, move |_, _| {
            let mut guard = slot.lock().unwrap();
            if alive.strong_count() == 0 { return glib::ControlFlow::Break; }
            let Some(w) = guard.as_mut() else { return glib::ControlFlow::Break };
            w.send_keys();
            glib::ControlFlow::Continue
        });
    }

    *slot.lock().unwrap() = Some(Worker {
        pipeline: pipeline.clone(), rtc: rtc.clone(), pc: None, state: state.clone(), video: None,
        x11: x11_window, overlay, focus, target_app,
        input, renderer: renderer.clone(), window, lsfg, volume,
        tracked: std::collections::HashSet::new(), quit: quit.clone(), start_time, input_retry: false,
        hud_active: false, preview, bus_watch: None, pointer, mouse: input::Mouse::default(), keys: input::Keys::new(std::env::var_os("ALLY_MOUSE_DEBUG").is_some()),
        focused: true,
    });

    // pad-added → branch attachment.
    if let Some(rtc) = &rtc {
        let slot = slot.clone();
        webrtc::wire_signals(rtc, move |pad| {
            let Some(caps) = pad.current_caps().or_else(|| Some(pad.query_caps(None))) else { return };
            let Some(media) = caps.structure(0).and_then(|s| s.get::<String>("media").ok()) else { return };
            let mut guard = slot.lock().unwrap();
            let Some(w) = guard.as_mut() else { return };
            if w.tracked.contains(&media) { return; }
            w.tracked.insert(media.clone());
            if media == "video" {
                match pipeline::attach_video(&w.pipeline, pad, w.state.clone(), &w.renderer, w.window, w.lsfg, w.quit.clone(), true, gamestream::Codec::H264, false) {
                    Ok(branch) => {
                        w.video = Some(branch);
                        if let Some(x11) = w.x11.as_mut() {
                            if x11.adopt_started.is_none() {
                                x11.adopt_started = Some(clock::now_ms());
                            }
                        }
                    }
                    Err(error) => { protocol::error(error); w.quit.quit(); }
                }
            } else if media == "audio" {
                if let Err(error) = pipeline::attach_audio(&w.pipeline, pad, w.volume, true) {
                    protocol::error(error);
                    w.quit.quit();
                }
            }
        });
    }
    if let Some(rtc) = rtc.as_ref().filter(|_| benchmark) {
        pipeline::wire_ingress(&rtc.lock().unwrap().element, state.clone());
        if capture_diagnostics {
            pipeline::wire_rtcp(&rtc.lock().unwrap().element, state.clone());
        }
    }

    // Bus errors are terminal, exactly like the Python worker.
    match pipeline::watch_errors(&pipeline, quit.clone()) {
        Ok(watch) => if let Some(w) = slot.lock().unwrap().as_mut() { w.bus_watch = Some(watch); },
        Err(error) => { protocol::error(error); quit.quit(); return; }
    }

    // A PC stream builds its whole pipeline and then plays it; an Xbox stream
    // plays at once and gets its branches when the tracks arrive.
    let Some(rtc) = rtc else {
        let started = match source {
            Source::Pc(pc) => pc::start(slot, &pipeline, pc, width, height, bitrate,
                std::time::Duration::from_millis(jitter.into()), quit.clone()),
            Source::Scene { fps, .. } => scene::start(slot, &pipeline, width, height, fps, russian, quit.clone()),
            Source::Xbox => Ok(()),
        };
        if let Err(error) = started {
            protocol::error(error);
            quit.quit();
        }
        return;
    };
    if let Err(error) = pipeline.set_state(gst::State::Playing) {
        protocol::error(format!("pipeline playing: {error}"));
        quit.quit();
        return;
    }
    if let Err(error) = webrtc::create_channels(&rtc) {
        protocol::error(error);
        quit.quit();
        return;
    }

    // Datachannel handlers: handshake + input loop start.
    {
        let channels = rtc.lock().unwrap().channels.clone();
        if let Some(message) = channels.get("message") {
            let channels_open = channels.clone();
            message.connect_closure("on-open", false, glib::closure!(move |_channel: gst_webrtc::WebRTCDataChannel| {
                handshake::handshake_open(&channels_open);
            }));
            let slot_string = slot.clone();
            let channels_string = channels.clone();
            message.connect_closure("on-message-string", false,
                glib::closure!(move |_channel: gst_webrtc::WebRTCDataChannel, data: String| {
                    on_ack(&slot_string, &channels_string, data.as_str(), width, height);
                }));
            let slot_data = slot.clone();
            let channels_data = channels.clone();
            message.connect_closure("on-message-data", false,
                glib::closure!(move |_channel: gst_webrtc::WebRTCDataChannel, data: glib::Bytes| {
                    if let Ok(text) = std::str::from_utf8(data.as_ref()) {
                        on_ack(&slot_data, &channels_data, text, width, height);
                    }
                }));
        }
    }

    webrtc::create_offer(&rtc, bitrate);
}

#[cfg(target_os = "linux")]
fn on_ack(slot: &Slot, channels: &handshake::Channels, data: &str, width: u32, height: u32) {
    if !handshake::is_handshake_ack(data) { return; }
    let mut guard = slot.lock().unwrap();
    let Some(w) = guard.as_mut() else { return };
    handshake::handshake_ack(channels, width, height, w.start_time);
    arm_input(slot, w);
}

/// Starts sending input: on every gamepad change and on an 8 ms tick.
#[cfg(target_os = "linux")]
fn arm_input(slot: &Slot, w: &mut Worker) {
    if !w.input.armed {
        w.input.armed = true;
        let timer_slot = slot.clone();
        glib::timeout_add(std::time::Duration::from_millis(8), move || {
            let mut guard = timer_slot.lock().unwrap();
            let Some(w) = guard.as_mut() else { return glib::ControlFlow::Break };
            if w.send_input(&timer_slot) { glib::ControlFlow::Continue } else { glib::ControlFlow::Break }
        });
        // A gamepad change goes out at once instead of at the next tick.
        if let Some((fd, device_alive)) = w.input.device_watch() {
            let slot = slot.clone();
            glib::unix_fd_add(fd, glib::IOCondition::IN, move |_, _| {
                let mut guard = slot.lock().unwrap();
                // The fd is closed once its device is lost or its worker replaced.
                if device_alive.strong_count() == 0 { return glib::ControlFlow::Break; }
                let Some(w) = guard.as_mut() else { return glib::ControlFlow::Break };
                if w.send_input(&slot) { glib::ControlFlow::Continue } else { glib::ControlFlow::Break }
            });
        }
    }
}

#[cfg(target_os = "linux")]
impl Worker {
    /// Sends the keys pressed in the video window to a PC stream, unless its menu is open.
    fn send_keys(&mut self) {
        let Some(x11) = self.x11.as_mut() else { return };
        let keys = x11.take_keys();
        if let Some(pc) = self.pc.as_ref().filter(|_| !self.input.route.overlay) {
            self.keys.send(pc.as_ref(), &keys);
        }
    }

    /// Gives the keys to a PC stream while it has them and Steam's on-screen
    /// keyboard is down. Sends the pointer events that arrived to the PC,
    /// then grabs the pointer while the stream has the mouse and lets it go
    /// otherwise.
    fn sync_pc_input(&mut self) {
        if let Some(pc) = self.pc.as_ref() {
            let typing = self.x11.as_ref().is_some_and(|x11| x11.keyboard);
            self.keys.hold(pc.as_ref(), self.focused && !self.input.route.overlay && !pc.ended() && !typing);
        }
        let Some(pointer) = self.pointer.as_mut() else { return };
        let events = pointer.events();
        let Some(pc) = self.pc.as_ref() else { return };
        // Steam's on-screen keyboard takes the pointer while it is up.
        let window = self.x11.as_ref().filter(|x11| x11.xid != 0 && !x11.keyboard).map(|x11| (x11.xid as u32, x11.size));
        let picture = { let st = self.state.lock().unwrap(); (st.width, st.height) };
        let wanted = self.focused && !self.input.route.overlay && !pc.ended();
        let hidden = self.mouse.mode == input::MouseMode::Game;
        // Raw motion arrives without the grab too: only a held grab makes it the stream's.
        let grabbed = pointer.hold(window.filter(|_| wanted).map(|(xid, _)| (xid, hidden)));
        let size = window.map_or((0, 0), |(_, size)| size);
        self.mouse.update(pc.as_ref(), grabbed, events, size, picture);
    }

    /// Sends the current pad state. Returns false when input is not armed or
    /// its channel is gone.
    fn send_input(&mut self, slot: &Slot) -> bool {
        let tick = if let Some(pc) = &self.pc {
            self.input.tick(pc.as_ref(), clock::now_ms())
        } else {
            let Some(rtc) = &self.rtc else { return false };
            let channels = rtc.lock().unwrap().channels.clone();
            let Some(channel) = channels.get("input") else { return false };
            self.input.tick(channel, clock::now_ms())
        };
        match tick {
            input::Tick::Sent(events) => {
                input::emit_route_events(events);
                if let Some(timing) = self.input.last_trial_timing.take() {
                    self.state.lock().unwrap().trial_input_timing = Some(timing);
                }
            }
            // Send the state this attempt skipped as soon as the gap allows.
            input::Tick::Deferred if !self.input_retry => {
                self.input_retry = true;
                let slot = slot.clone();
                glib::timeout_add_once(std::time::Duration::from_millis(1), move || {
                    if let Some(w) = slot.lock().unwrap().as_mut() {
                        w.input_retry = false;
                        w.send_input(&slot);
                    }
                });
            }
            input::Tick::Deferred | input::Tick::Opening => {}
            input::Tick::Closed => return false,
        }
        true
    }
}

/// Standalone menu controller: plain-text keys on stdout (src/deck.rs parses).
#[cfg(target_os = "linux")]
fn menu_input_mode() {
    let quit = glib::MainLoop::new(None, false);
    let target: Option<u32> = std::env::var("SteamAppId").ok().and_then(|v| v.parse().ok()).filter(|id| *id != 0);
    let focus = std::env::var("ALLY_GAMESCOPE_CONTROL_DISPLAY").ok()
        .filter(|_| target.is_some())
        .and_then(|display| x11::GamescopeFocus::new(&display).ok());
    let state = Arc::new(Mutex::new(MenuState {
        device: None, axes: std::collections::HashMap::new(), active: false, next_probe: 0.0,
    }));
    {
        let quit = quit.clone();
        std::thread::spawn(move || {
            use std::io::BufRead;
            let stdin = std::io::stdin();
            let mut line = String::new();
            let _ = stdin.lock().read_line(&mut line);
            glib::MainContext::default().invoke(move || quit.quit());
        });
    }
    {
        let state = state.clone();
        glib::timeout_add_local(std::time::Duration::from_millis(16), move || {
            let now = clock::now_ms();
            let mut st = state.lock().unwrap();
            if st.device.is_none() {
                if now < st.next_probe { return glib::ControlFlow::Continue; }
                st.next_probe = now + 1000.0;
                let mut input = input::Input::new(now);
                input.connect_gamepad();
                st.device = input.device.take();
                if st.device.is_none() { return glib::ControlFlow::Continue; }
                st.axes.clear();
                st.active = false;
            }
            let previous = st.active;
            let app = focus.as_ref().and_then(|f| f.focused_app());
            st.active = app.is_none() || app == target;
            let mut events: Vec<evdev::InputEvent> = Vec::new();
            let mut lost = false;
            let mut would_block = false;
            if let Some(device) = st.device.as_mut() {
                match device.fetch_events() {
                    Ok(iter) => events.extend(iter),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => would_block = true,
                    Err(_) => lost = true,
                }
            }
            if lost {
                st.device = None;
                return glib::ControlFlow::Continue;
            }
            if would_block { return glib::ControlFlow::Continue; }
            for event in events {
                let mut key: Option<&str> = None;
                match event.event_type() {
                    evdev::EventType::KEY if event.value() == 1 => {
                        key = match event.code() {
                            304 => Some("accept"), 305 => Some("back"), 307 => Some("quit"), 308 => Some("refresh"),
                            310 => Some("previous-tab"), 311 => Some("next-tab"), 314 => Some("search"), 315 => Some("settings"),
                            312 => Some("previous-letter"), 313 => Some("next-letter"),
                            544 => Some("up"), 545 => Some("down"), 546 => Some("left"), 547 => Some("right"),
                            _ => None,
                        };
                    }
                    // Analog triggers: a press past a quarter of the axis's range,
                    // released below an eighth.
                    evdev::EventType::ABSOLUTE if [2, 5].contains(&event.code()) => {
                        let held = st.axes.get(&event.code()).copied().unwrap_or(0) == 1;
                        let range = st.device.as_ref().and_then(|device| device.get_absinfo().ok())
                            .and_then(|mut axes| axes.find(|(code, _)| code.0 == event.code()))
                            .map_or(255, |(_, info)| info.maximum().max(1));
                        if !held && event.value() > range / 4 {
                            st.axes.insert(event.code(), 1);
                            key = Some(if event.code() == 2 { "previous-letter" } else { "next-letter" });
                        } else if held && event.value() <= range / 8 {
                            st.axes.insert(event.code(), 0);
                        }
                    }
                    evdev::EventType::ABSOLUTE if [0, 1, 16, 17].contains(&event.code()) => {
                        let value = if event.code() < 2 {
                            if event.value() < -18000 { -1 } else if event.value() > 18000 { 1 } else { 0 }
                        } else {
                            event.value()
                        };
                        if value != st.axes.get(&event.code()).copied().unwrap_or(0) {
                            let keys = if event.code() % 2 == 0 { ("left", "right") } else { ("up", "down") };
                            key = match value { -1 => Some(keys.0), 1 => Some(keys.1), _ => None };
                            st.axes.insert(event.code(), value);
                        }
                    }
                    _ => {}
                }
                if let Some(key) = key {
                    if previous && st.active {
                        use std::io::Write;
                        let mut out = std::io::stdout().lock();
                        let _ = writeln!(out, "{key}");
                        let _ = out.flush();
                    }
                }
            }
            glib::ControlFlow::Continue
        });
    }
    quit.run();
}

#[cfg(target_os = "linux")]
struct MenuState {
    device: Option<evdev::Device>,
    axes: std::collections::HashMap<u16, i32>,
    active: bool,
    next_probe: f64,
}

#[cfg(target_os = "linux")]
fn parse_xid(value: &str) -> Option<u64> {
    let value = value.trim();
    if let Some(hex) = value.strip_prefix("0x").or_else(|| value.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16).ok()
    } else {
        value.parse().ok()
    }
}