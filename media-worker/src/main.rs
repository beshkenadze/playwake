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
mod route;
#[cfg(target_os = "linux")]
mod state;
#[cfg(target_os = "linux")]
mod transport;
#[cfg(target_os = "linux")]
mod trial;
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

#[cfg(target_os = "linux")]
struct Worker {
    pipeline: gst::Pipeline,
    rtc: Arc<Mutex<webrtc::Rtc>>,
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
    input_timer: bool,
    hud_active: bool,
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
        "opusdec", "audioconvert", "audioresample", "volume", "queue", "tee", "fakesink"];
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
                    let element = w.rtc.lock().unwrap().element.clone();
                    hud::refresh_hud(&w.state, &element, queue.as_ref());
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
                if let Some(x11) = w.x11.as_mut() {
                    match x11.tick(now) {
                        Ok(tick) => {
                            if tick.adopted {
                                protocol::event("vulkan-window-embedded", serde_json::json!({}));
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
                    let active = app.is_none() || app == Some(target);
                    let events = w.input.route.focus(active);
                    input::emit_route_events(events);
                }
            }
            glib::ControlFlow::Continue
        });
    }

    quit.run();

    if let Some(mut w) = slot.lock().unwrap().take() {
        if let Some(overlay) = w.overlay.as_mut() { overlay.close(); }
        if let Some(x11) = w.x11.as_mut() { x11.close(); }
        let _ = w.pipeline.set_state(gst::State::Null);
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn dispatch(slot: &Slot, quit: &glib::MainLoop, command: protocol::Command) {
    use protocol::Command;
    match command {
        Command::Start { bitrate, width, height, benchmark, capture_diagnostics, renderer, lsfg,
                         interactive, russian, xbox_palette, compact_stats, volume, console_name, .. } => {
            start(slot, quit, bitrate, width, height, benchmark, capture_diagnostics, renderer, lsfg,
                interactive, russian, xbox_palette, compact_stats, volume, console_name);
        }
        Command::Answer { sdp } => {
            if let Some(w) = slot.lock().unwrap().as_ref() {
                webrtc::set_answer(&w.rtc, &sdp);
            }
        }
        Command::Ice { candidates } => {
            if let Some(w) = slot.lock().unwrap().as_ref() {
                webrtc::add_ice(&w.rtc, &candidates);
            }
        }
        Command::Stats => {
            if let Some(w) = slot.lock().unwrap().as_ref() {
                let queue = w.video.as_ref().and_then(|v| v.queue.clone());
                let element = w.rtc.lock().unwrap().element.clone();
                let mut st = w.state.lock().unwrap();
                hud::emit_stats(&mut st, queue.as_ref(), Some(&element));
            }
        }
        Command::Pulse { axis, buttons, duration, trial } => {
            if let Some(w) = slot.lock().unwrap().as_mut() {
                w.input.pulse(axis, buttons, duration, trial);
            }
        }
        Command::Gamepad { axes, buttons, lt, rt } => {
            if let Some(w) = slot.lock().unwrap().as_mut() {
                w.input.set_gamepad(&axes, buttons, lt, rt);
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
            }
        }
        Command::OverlayClose => {
            if let Some(w) = slot.lock().unwrap().as_mut() {
                let events = w.input.route.close_overlay(false);
                input::emit_route_events(events);
            }
        }
        Command::XboxGuide => {
            if let Some(w) = slot.lock().unwrap().as_mut() {
                let events = w.input.route.close_overlay(true);
                input::emit_route_events(events);
            }
        }
        Command::InputFocus { active } => {
            if let Some(w) = slot.lock().unwrap().as_mut() {
                let events = w.input.route.focus(active);
                input::emit_route_events(events);
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
                if restore {
                    let slot_restore = slot_actuate.clone();
                    glib::timeout_add(std::time::Duration::from_millis(550), move || {
                        let mut guard = slot_restore.lock().unwrap();
                        if let Some(w) = guard.as_mut() {
                            w.input.pulse(-0.9, 0, 230, false);
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
        _ => {}
    }
}

#[cfg(target_os = "linux")]
#[allow(clippy::too_many_arguments)]
fn start(
    slot: &Slot, quit: &glib::MainLoop,
    bitrate: u32, width: u32, height: u32, benchmark: bool, capture_diagnostics: bool,
    renderer: String, lsfg: bool, interactive: bool, russian: bool, xbox_palette: bool,
    compact_stats: bool, volume: u32, console_name: String,
) {
    LIMIT_KBPS.store(bitrate, std::sync::atomic::Ordering::Relaxed);
    let jitter: u32 = std::env::var("ALLY_JITTER_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(20);
    let parent = std::env::var("GREENLIGHT_VIDEO_PARENT_XID").ok().and_then(|v| parse_xid(&v));
    let adopt = renderer == "vulkan" && parent.is_none();
    let mut x11_window = None;
    // Always create our own child (inside the GPUI parent when embedded):
    // ximagesink paints for the default visual, the GPUI parent is ARGB32.
    let window = if std::env::var_os("DISPLAY").is_some() {
        match x11::X11Window::create(width, height, parent, adopt) {
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
    let rtc = match webrtc::build(&pipeline, jitter, transport.clone(), capture_diagnostics) {
        Ok(rtc) => Arc::new(Mutex::new(rtc)),
        Err(error) => { protocol::error(error); return; }
    };
    let state = Arc::new(Mutex::new(state::State::new(width, height, benchmark, capture_diagnostics, transport.clone())));
    if compact_stats {
        state.lock().unwrap().diagnostics = true;
        protocol::event("stream-diagnostics", serde_json::json!({ "enabled": true }));
    }

    let mut input = input::Input::new(start_time);
    input.connect_gamepad();
    if let Some(x) = x11_window.as_ref() { x.sync(); }
    let overlay = if interactive {
        x11_window.as_ref()
            .and_then(|x| overlay::Overlay::new(x.display, x.xid as u32).ok())
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
    }

    *slot.lock().unwrap() = Some(Worker {
        pipeline: pipeline.clone(), rtc: rtc.clone(), state: state.clone(), video: None,
        x11: x11_window, overlay, focus, target_app,
        input, renderer: renderer.clone(), window, lsfg, volume,
        tracked: std::collections::HashSet::new(), quit: quit.clone(), start_time, input_timer: false,
        hud_active: false,
    });

    // pad-added → branch attachment.
    {
        let slot = slot.clone();
        webrtc::wire_signals(&rtc, move |pad| {
            let Some(caps) = pad.current_caps().or_else(|| Some(pad.query_caps(None))) else { return };
            let Some(media) = caps.structure(0).and_then(|s| s.get::<String>("media").ok()) else { return };
            let mut guard = slot.lock().unwrap();
            let Some(w) = guard.as_mut() else { return };
            if w.tracked.contains(&media) { return; }
            w.tracked.insert(media.clone());
            if media == "video" {
                match pipeline::attach_video(&w.pipeline, pad, w.state.clone(), &w.renderer, w.window, w.lsfg, w.quit.clone()) {
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
                if let Err(error) = pipeline::attach_audio(&w.pipeline, pad, w.volume) {
                    protocol::error(error);
                    w.quit.quit();
                }
            }
        });
    }
    if benchmark {
        pipeline::wire_ingress(&rtc.lock().unwrap().element, state.clone());
        if capture_diagnostics {
            pipeline::wire_rtcp(&rtc.lock().unwrap().element, state.clone());
        }
    }

    // Bus errors are terminal, exactly like the Python worker.
    {
        let quit = quit.clone();
        let _ = pipeline.bus().unwrap().add_watch_local(move |_bus, message| {
            if let gst::MessageView::Error(error) = message.view() {
                protocol::error(error.error().to_string());
                quit.quit();
            }
            glib::ControlFlow::Continue
        });
    }

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
    if !w.input_timer {
        w.input_timer = true;
        let slot = slot.clone();
        glib::timeout_add(std::time::Duration::from_millis(8), move || {
            let now = clock::now_ms();
            let mut guard = slot.lock().unwrap();
            let Some(w) = guard.as_mut() else { return glib::ControlFlow::Break };
            let channels = w.rtc.lock().unwrap().channels.clone();
            let Some(channel) = channels.get("input") else { return glib::ControlFlow::Break };
            let (continue_timer, events) = w.input.tick(channel, now);
            input::emit_route_events(events);
            if let Some(timing) = w.input.last_trial_timing.take() {
                w.state.lock().unwrap().trial_input_timing = Some(timing);
            }
            if continue_timer { glib::ControlFlow::Continue } else { glib::ControlFlow::Break }
        });
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
                            304 => Some("accept"), 305 => Some("back"), 308 => Some("refresh"),
                            310 => Some("previous-tab"), 311 => Some("next-tab"), 315 => Some("settings"),
                            544 => Some("up"), 545 => Some("down"), 546 => Some("left"), 547 => Some("right"),
                            _ => None,
                        };
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