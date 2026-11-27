use crate::{cloud, journal, lsfg, session_store::Preferences};
use crate::xbox::{Console, Resolution, Result, Xbox};
use serde_json::{Value, json};
use std::{
    env, fs, io::{BufRead, BufReader, Write},
    path::PathBuf, process::{Child, Command, Stdio},
    sync::{Arc, atomic::{AtomicBool, Ordering}, mpsc},
    thread, time::{Duration, Instant},
};

#[derive(Clone, Copy)]
pub struct VideoWindow {
    pub xid: u64,
}

pub struct Player(Child);

impl Drop for Player {
    fn drop(&mut self) {
        if let Some(mut stdin) = self.0.stdin.take() {
            let _ = writeln!(stdin, "{{\"action\":\"stop\"}}");
        }
        for _ in 0..15 {
            if self.0.try_wait().ok().flatten().is_some() { return; }
            thread::sleep(Duration::from_millis(40));
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Worker {
    Python(PathBuf),
    Native(PathBuf),
}

fn classify(path: PathBuf) -> Result<Worker> {
    if path.extension().is_some_and(|ext| ext == "py") { return Ok(Worker::Python(path)); }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata = fs::metadata(&path)
            .map_err(|_| format!("Media worker is missing: {}", path.display()))?;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err(format!("Media worker is not executable: {}", path.display()).into());
        }
    }
    Ok(Worker::Native(path))
}

fn resolve_worker(override_path: Option<PathBuf>, sibling: Option<PathBuf>, source: PathBuf) -> Result<Worker> {
    if let Some(path) = override_path { return classify(path); }
    // Native worker first; the Python worker stays as a one-release fallback
    // for installs that predate the native bundle.
    if let Some(dir) = sibling.as_ref().and_then(|path| path.parent()) {
        let native = dir.join("playwake-media");
        if native.is_file() { return classify(native); }
    }
    if let Some(sibling) = sibling.filter(|path| path.is_file()) { return Ok(Worker::Python(sibling)); }
    if source.is_file() { Ok(Worker::Python(source)) } else { Err("GStreamer worker file is missing".into()) }
}

fn worker() -> Result<Worker> {
    if let Some(path) = env::var_os("ALLY_MEDIA_WORKER") { return classify(path.into()); }
    let exe = env::current_exe().map_err(|_| "Cannot locate the application")?;
    let sibling = exe.parent().map(|dir| dir.join("gstreamer_player.py"));
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("media/gstreamer_player.py");
    resolve_worker(None, sibling, source)
}

fn runtime_env(root: &std::path::Path, plugin_path: Option<std::ffi::OsString>, lib_path: Option<std::ffi::OsString>)
    -> Result<(std::ffi::OsString, std::ffi::OsString)> {
    let mut plugins = vec![root.join("gstreamer-1.0")];
    if let Some(value) = plugin_path { plugins.extend(env::split_paths(&value)); }
    let mut libs = vec![root.to_path_buf()];
    if let Some(value) = lib_path { libs.extend(env::split_paths(&value)); }
    Ok((env::join_paths(plugins).map_err(|_| "Invalid GStreamer plugin path")?,
        env::join_paths(libs).map_err(|_| "Invalid GStreamer library path")?))
}

fn apply_runtime_env(cmd: &mut Command) -> Result<()> {
    if let Some(root) = env::var_os("ALLY_GST_RUNTIME") {
        let root = PathBuf::from(root);
        if !root.is_dir() { return Err("GStreamer runtime directory is missing".into()); }
        let variable = if cfg!(target_os="windows") { "PATH" }
            else if cfg!(target_os="macos") { "DYLD_LIBRARY_PATH" } else { "LD_LIBRARY_PATH" };
        let (plugins, libs) = runtime_env(&root, env::var_os("GST_PLUGIN_PATH"), env::var_os(variable))?;
        cmd.env("GST_PLUGIN_PATH", plugins).env(variable, libs);
    }
    Ok(())
}

fn python_command(script: &std::path::Path) -> Result<Command> {
    let python = env::var_os("ALLY_GST_PYTHON")
        .unwrap_or_else(|| if cfg!(target_os="windows") { "python.exe".into() } else { "python3".into() });
    let mut cmd = Command::new(python);
    cmd.arg(script).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    apply_runtime_env(&mut cmd)?;
    Ok(cmd)
}

fn native_command(path: &std::path::Path) -> Result<Command> {
    let mut cmd = Command::new(path);
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    apply_runtime_env(&mut cmd)?;
    Ok(cmd)
}

fn worker_command() -> Result<Command> {
    match worker()? {
        Worker::Python(script) => python_command(&script),
        Worker::Native(path) => native_command(&path),
    }
}

pub fn menu_input(send: impl Fn(crate::deck::Key) + Send + 'static) -> Result<Player> {
    let mut cmd = match worker()? {
        Worker::Python(script) => python_command(&script)?,
        Worker::Native(path) => native_command(&path)?,
    };
    let mut child = cmd.arg("--menu-input").spawn().map(Player)
        .map_err(|_| "Could not start menu controller".to_string())?;
    let output = child.0.stdout.take().ok_or("Missing menu controller output")?;
    thread::spawn(move || {
        for line in BufReader::new(output).lines().map_while(std::result::Result::ok) {
            if let Some(key) = crate::deck::Key::parse(&line) { send(key); }
        }
    });
    Ok(child)
}

fn launch(window: Option<VideoWindow>, lsfg: Option<&lsfg::Runtime>, prefs: Preferences, interactive: bool) -> Result<Player> {
    launch_with(worker_command()?, window, lsfg, prefs, interactive)
}

fn launch_with(mut cmd: Command, window: Option<VideoWindow>, lsfg: Option<&lsfg::Runtime>, prefs: Preferences,
               interactive: bool) -> Result<Player> {
    if let Some(lsfg) = lsfg {
        lsfg.configure(&mut cmd, prefs)?;
    } else if interactive {
        cmd.env("DISABLE_LSFGVK", "1").env("DISABLE_LSFG", "1");
    }
    if let Some(window) = window {
        cmd.env("GREENLIGHT_VIDEO_PARENT_XID", window.xid.to_string());
    }
    cmd.spawn().map(Player).map_err(|_| "Could not launch the media worker. Check the runtime installation.".into())
}

fn command(player: &mut Player, message: Value) -> Result<()> {
    writeln!(player.0.stdin.as_mut().ok_or("Media input pipe closed")?, "{message}")
        .map_err(|_| "GStreamer worker stopped".to_string())
}

#[derive(Clone, Copy)]
pub enum Run {
    Interactive(Preferences),
    Timed(Duration),
    Latency,
    Transport,
    Vulkan,
}

pub enum StreamCommand {
    OpenOverlay,
    CloseOverlay,
    XboxGuide,
    ToggleDiagnostics,
    OverlaySelection(usize),
    InputFocus(bool),
    /// A character typed on this device, for the PC being streamed.
    Type(char),
    /// A key pressed and released, as a Windows virtual-key code.
    Key(u8),
    /// Switches the PC mouse between game and desktop mode.
    ToggleMouseMode,
}

/// The Windows virtual-key code of a keyboard key that types no character.
pub fn named_key(key: &str) -> Option<u8> {
    Some(match key {
        "backspace" => 0x08,
        "tab" => 0x09,
        "enter" | "return" => 0x0d,
        "escape" => 0x1b,
        "pageup" => 0x21,
        "pagedown" => 0x22,
        "end" => 0x23,
        "home" => 0x24,
        "left" => 0x25,
        "up" => 0x26,
        "right" => 0x27,
        "down" => 0x28,
        "delete" => 0x2e,
        _ => return None,
    })
}

pub enum StreamEvent {
    Status(String),
    VideoReady,
    Overlay(bool),
    OverlayKey(OverlayKey),
    /// The gamepad's quit chord.
    Quit,
    /// The gamepad's statistics chord.
    ToggleStats,
}

pub enum OverlayKey { Left, Right, Activate, Back, Select(usize) }

impl StreamCommand {
    /// The command as the media worker reads it.
    fn message(self) -> Value {
        match self {
            Self::OpenOverlay => json!({"action":"overlay-open"}),
            Self::CloseOverlay => json!({"action":"overlay-close"}),
            Self::XboxGuide => json!({"action":"xbox-guide"}),
            Self::ToggleDiagnostics => json!({"action":"diagnostics-toggle"}),
            Self::OverlaySelection(index) => json!({"action":"overlay-selection","index":index}),
            Self::InputFocus(active) => json!({"action":"input-focus","active":active}),
            Self::Type(character) => json!({"action":"type","text":character.to_string()}),
            Self::Key(code) => json!({"action":"key","code":code}),
            Self::ToggleMouseMode => json!({"action":"mouse-mode-toggle"}),
        }
    }
}

impl OverlayKey {
    /// The key in a `stream-overlay-key` event from the media worker.
    fn read(event: &Value) -> Option<Self> {
        match event.get("key").and_then(Value::as_str) {
            Some("left") => Some(Self::Left),
            Some("right") => Some(Self::Right),
            Some("activate") => Some(Self::Activate),
            Some("back") => Some(Self::Back),
            Some("select") => event.get("index").and_then(Value::as_u64)
                .filter(|index| *index < 6).map(|index| Self::Select(index as usize)),
            _ => None,
        }
    }
}

/// Streams a game from a PC until the stream ends or `cancel` is set. The
/// media worker starts the game on the PC and plays what it sends.
pub fn play_pc(game: crate::pc::Game, window: Option<VideoWindow>, cancel: Arc<AtomicBool>, prefs: Preferences,
               controls: mpsc::Receiver<StreamCommand>, notify: impl Fn(StreamEvent)) -> Result<()> {
    // The Python worker knows only Xbox streams and would wait for one.
    if matches!(worker()?, Worker::Python(_)) { return Err("PC streaming needs the playwake-media worker".into()); }
    let enable_lsfg = prefs.lsfg_enabled && window.is_some();
    let lsfg = if enable_lsfg { Some(lsfg::Runtime::discover()?) } else { None };
    let vulkan = enable_lsfg || env::var("ALLY_VIDEO_RENDERER").as_deref() == Ok("vulkan");
    // A smaller stream than the screen is scaled up by SteamOS.
    let panel = crate::pc::Panel::of(crate::device::current()).scaled_to(prefs.pc_stream_height).for_lsfg(enable_lsfg);
    journal::record("stream_requested", json!({"provider":"pc","bitrate_limit_kbps": prefs.pc_bitrate_mbps * 1000,
        "jitter_ms": prefs.jitter_ms,
        "width": panel.width, "height": panel.height, "fps": panel.fps}));
    let mut player = launch(window, lsfg.as_ref(), prefs, true)?;
    let output = player.0.stdout.take().ok_or("Missing GStreamer output pipe")?;
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(output).lines().map_while(std::result::Result::ok) {
            if let Ok(event) = serde_json::from_str::<Value>(&line) {
                if tx.send(event).is_err() { break; }
            }
        }
    });
    command(&mut player, json!({"action":"start","bitrate":prefs.pc_bitrate_mbps * 1000,"jitter_ms":prefs.jitter_ms,"width":panel.width,
        "height":panel.height,"renderer":if vulkan { "vulkan" } else { "auto" },"lsfg":enable_lsfg,"interactive":true,
        "lanczos_upscale":window.is_some() && prefs.lanczos_upscale,
        "russian":prefs.russian(),"xbox_palette":prefs.xbox_palette,"compact_stats":prefs.compact_stats,
        "control_layout": crate::device::current().id, "control_device": crate::device::current().name,
        "volume": if prefs.mute { 0 } else { prefs.volume },
        "console_name":game.pc(),"pc":game.start(panel, prefs.pc_hdr)}))?;
    let mut video = false;
    while !cancel.load(Ordering::Relaxed) {
        for control in controls.try_iter() { command(&mut player, control.message())?; }
        match rx.recv_timeout(Duration::from_millis(8)) {
            Ok(event) => match event.get("event").and_then(Value::as_str) {
                Some("stream-overlay") => {
                    if let Some(open) = event.get("open").and_then(Value::as_bool) { notify(StreamEvent::Overlay(open)); }
                }
                Some("stream-overlay-key") => {
                    if let Some(key) = OverlayKey::read(&event) { notify(StreamEvent::OverlayKey(key)); }
                }
                Some("stream-quit") => notify(StreamEvent::Quit),
                Some("stream-stats") => notify(StreamEvent::ToggleStats),
                Some("video") if !video => {
                    video = true;
                    notify(StreamEvent::VideoReady);
                    if window.is_some() { command(&mut player, json!({"action":"overlay-hint"}))?; }
                }
                Some("error") => {
                    let message = event.get("message").and_then(Value::as_str).unwrap_or("PC stream failed");
                    journal::record("gstreamer_error", json!({"message": message}));
                    return Err(message.to_owned());
                }
                _ => {}
            },
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            // The worker leaves when the PC ends the stream.
            Err(mpsc::RecvTimeoutError::Disconnected) => return left(player.0.wait()),
        }
    }
    Ok(())
}

/// What a worker that left by itself means: the stream ended, or it crashed.
fn left(status: std::io::Result<std::process::ExitStatus>) -> Result<()> {
    if status.is_ok_and(|status| status.success()) { Ok(()) } else { Err("The media worker stopped unexpectedly".into()) }
}

pub fn preview(window: VideoWindow, prefs: Preferences, cancel: Arc<AtomicBool>,
               notify: impl Fn(PreviewEvent)) -> Result<()> {
    let runtime = prefs.lsfg_enabled.then(lsfg::Runtime::discover).transpose()?;
    // As a stream with LSFG, with or without it: Vulkan draws to the X11
    // window itself. The GPU decodes, so the CPU has time to draw the scene.
    let mut cmd = worker_command()?;
    cmd.env("DISABLE_GAMESCOPE_WSI", "1").env("GST_VULKAN_WINDOW", "xcb")
        .env_remove("ALLY_DECODER").env_remove("ALLY_DECODER_OPTS");
    let mut player = launch_with(cmd, Some(window), runtime.as_ref(), prefs, true)?;
    let output = player.0.stdout.take().ok_or("Missing GStreamer output pipe")?;
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(output).lines().map_while(std::result::Result::ok) {
            if let Ok(event) = serde_json::from_str::<Value>(&line)
                && tx.send(event).is_err() { break; }
        }
    });
    // The screen's size at half its rate, as a PC stream with LSFG sends; LSFG doubles it.
    let panel = crate::pc::Panel::of(crate::device::current());
    let (width, height, fps) = (panel.width, panel.height, panel.fps / 2);
    command(&mut player, json!({"action":"preview","width":width,"height":height,"fps":fps,"lsfg":prefs.lsfg_enabled,
        "performance":prefs.lsfg_performance,"flow_scale":prefs.lsfg_flow_scale(),"russian":prefs.russian(),
        "xbox_palette":prefs.xbox_palette}))?;
    journal::record("lsfg_preview_started", json!({"width":width,"height":height,"lsfg":prefs.lsfg_enabled,
        "performance":prefs.lsfg_performance,"flow_scale":prefs.lsfg_flow_scale()}));
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut ready = false;
    while !cancel.load(Ordering::Relaxed) {
        if !ready && Instant::now() >= deadline { return Err("The LSFG preview did not start".into()); }
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(event) => match event["event"].as_str() {
                Some("video") if !ready => { ready = true; notify(PreviewEvent::Ready); }
                Some("preview-toggle") => notify(PreviewEvent::Toggle),
                Some("preview-back") => notify(PreviewEvent::Back),
                Some("preview-stats") => journal::record("lsfg_preview_sample", json!({
                    "lsfg":prefs.lsfg_enabled,"source_fps":event["sourceFps"].as_f64(),
                    "present_fps":event["presentFps"].as_f64(),"display_hz":event["displayHz"].as_u64(),
                })),
                Some("error") => return Err(event["message"].as_str().unwrap_or("The LSFG preview failed").into()),
                _ => {},
            },
            Err(mpsc::RecvTimeoutError::Disconnected) => return Err("The LSFG preview closed".into()),
            Err(mpsc::RecvTimeoutError::Timeout) => {},
        }
    }
    Ok(())
}

pub enum PreviewEvent { Ready, Toggle, Back }

#[derive(Clone)]
pub enum Target {
    Home(Console),
    Cloud(cloud::Game),
    Pc(crate::pc::Game),
}

pub fn play(account: Arc<Xbox>, target: Target, resolution: Resolution, window: Option<VideoWindow>,
            cancel: Arc<AtomicBool>, run: Run, controls: Option<mpsc::Receiver<StreamCommand>>,
            notify: impl Fn(StreamEvent)) -> Result<()> {
    let status = |message| notify(StreamEvent::Status(message));
    status("Preparing Xbox session…".into());
    let (requested_width, requested_height) = resolution.dimensions();
    let benchmark = matches!(run, Run::Latency | Run::Transport | Run::Vulkan);
    let interactive = matches!(run, Run::Interactive(_));
    let prefs = match run { Run::Interactive(prefs) => prefs, _ => Preferences::default() };
    let enable_lsfg = interactive && prefs.lsfg_enabled;
    let lsfg = if enable_lsfg {
        if window.is_none() { return Err("LSFG requires an X11 or XWayland window".into()); }
        Some(lsfg::Runtime::discover()?)
    } else { None };
    let vulkan = enable_lsfg || matches!(run, Run::Vulkan) || env::var("ALLY_VIDEO_RENDERER").as_deref() == Ok("vulkan");
    let renderer = if vulkan { "vulkan" } else { "auto" };
    let is_cloud = matches!(&target, Target::Cloud(..));
    let bitrate = (!is_cloud).then_some(if interactive { prefs.bitrate_mbps * 1000 } else if benchmark { 20000 } else { 20480 });
    let provider = if is_cloud { "cloud" } else { "remote_play" };
    journal::record("stream_requested", json!({"provider":provider,"bitrate_limit_kbps": bitrate, "width": requested_width, "height": requested_height,
        "jitter_ms": interactive.then_some(prefs.jitter_ms),
        "renderer": renderer, "lsfg_requested": enable_lsfg || (!interactive && vulkan
            && env::var("LSFGVK_ENV").as_deref() == Ok("1") && env::var("DISABLE_LSFGVK").as_deref() != Ok("1"))}));
    let (mut xbox, path, title) = match target {
        // A PC stream has no Xbox session; `play_pc` runs it.
        Target::Pc(_) => return Err("A PC stream does not go through Xbox".into()),
        Target::Home(console) => {
            let xbox = account.home()?;
            let path = xbox.reserve(&console, resolution)?;
            let title = if console.name.is_empty() { console.model } else { console.name };
            (xbox, path, title)
        }
        Target::Cloud(game) => {
            let (xbox, _) = cloud::connect(&account)?;
            let path = cloud::reserve(&xbox, &game, resolution)?;
            (xbox, path, game.name)
        }
    };
    let result = (|| {
        let config = xbox.provision(&path, &cancel, &status)?;
        journal::record("stream_provisioned", json!({"provider":provider,"stun_configured":config.stun.is_some()}));
        let mut player = launch(window, lsfg.as_ref(), prefs, interactive)?;
        let output = player.0.stdout.take().ok_or("Missing GStreamer output pipe")?;
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(output).lines().map_while(std::result::Result::ok) {
                if let Ok(event) = serde_json::from_str::<Value>(&line) {
                    if tx.send(event).is_err() { break; }
                }
            }
        });
        command(&mut player, json!({"action":"start","bitrate":bitrate,"width":requested_width,
            "height":requested_height,"jitter_ms":interactive.then_some(prefs.jitter_ms),"benchmark":benchmark,"capture_diagnostics":matches!(run, Run::Transport),
            "renderer":renderer,"lsfg":enable_lsfg,"interactive":interactive,"cloud":is_cloud,"stun":config.stun,
            "lanczos_upscale":interactive && window.is_some() && prefs.lanczos_upscale,
            "russian":prefs.russian(),"xbox_palette":prefs.xbox_palette,"compact_stats":prefs.compact_stats,
            "control_layout": crate::device::current().id, "control_device": crate::device::current().name,
            "volume": if prefs.mute { 0 } else { prefs.volume },
            "console_name":title}))?;
        status("Negotiating the GStreamer connection…".into());
        let start = Instant::now();
        let mut keepalive = Instant::now();
        let mut last_stats = Instant::now();
        let mut controller = crate::input::Controller::new();
        let mut answer_set = false;
        let mut candidates = None;
        let mut first_video: Option<Instant> = None;
        let mut stable_since: Option<Instant> = None;
        let mut next_trial = Instant::now();
        let mut attempts = 0;
        let mut samples = 0;
        let warmup = Duration::from_secs(if vulkan { 8 } else { 4 });
        while !cancel.load(Ordering::Relaxed) {
            if let Some(controls) = &controls {
                for control in controls.try_iter() {
                    command(&mut player, control.message())?;
                }
            }
            if matches!(run, Run::Timed(limit) if first_video.is_some_and(|time| time.elapsed() >= limit)) {
                break;
            }
            if matches!(run, Run::Latency) && (samples >= 8 || attempts >= 12) && Instant::now() >= next_trial {
                break;
            }
            let benchmark_limit = if matches!(run, Run::Transport | Run::Vulkan) { 40 } else { 34 };
            if benchmark && first_video.is_some_and(|time| time.elapsed() >= Duration::from_secs(benchmark_limit)) {
                break;
            }
            if benchmark && attempts < 12 && samples < 8
                && stable_since.is_some_and(|time| time.elapsed() >= warmup)
                && Instant::now() >= next_trial {
                command(&mut player, json!({"action":"trial","restore_view":true}))?;
                attempts += 1;
                next_trial = Instant::now() + Duration::from_millis(1800);
            }
            if first_video.is_none() && start.elapsed() > Duration::from_secs(35) {
                journal::record("first_video_timeout", json!({}));
                return Err("GStreamer did not receive video before the timeout".into());
            }
            if keepalive.elapsed() >= config.heartbeat {
                xbox.keepalive(&path)?;
                keepalive = Instant::now();
            }
            if first_video.is_some() && last_stats.elapsed() >= Duration::from_secs(5) {
                command(&mut player, json!({"action":"stats"}))?;
                last_stats = Instant::now();
            }
            if let Some(state) = controller.poll() { command(&mut player, state)?; }
            let poll_ms = if interactive || cfg!(any(target_os = "windows", target_os = "macos")) { 8 } else { 150 };
            match rx.recv_timeout(Duration::from_millis(poll_ms)) {
                Ok(event) => match event.get("event").and_then(Value::as_str) {
                    Some("offer") => {
                        let offer = event.get("sdp").and_then(Value::as_str).ok_or("GStreamer returned no SDP offer")?;
                        let answer = xbox.sdp(&path, offer)?;
                        command(&mut player, json!({"action":"answer","sdp":answer}))?;
                    }
                    Some("answer-set") => {
                        answer_set = true;
                        if is_cloud { journal::record("cloud_sdp_accepted", json!({})); }
                    }
                    Some("connection") if is_cloud => {
                        let state = event.get("state").and_then(Value::as_str)
                            .filter(|state| matches!(*state, "new" | "connecting" | "connected" | "disconnected" | "failed" | "closed"));
                        journal::record("cloud_peer_state", json!({"state":state}));
                    }
                    Some("ice-connection" | "cloud-ice-applied" | "cloud-answer-info") if is_cloud => {
                        let state = event.get("state").and_then(Value::as_str)
                            .filter(|state| matches!(*state, "new" | "checking" | "connected" | "completed" | "disconnected" | "failed" | "closed"));
                        journal::record("cloud_webrtc_stage", json!({
                            "kind": event.get("event"), "state":state, "applied":event.get("count").and_then(Value::as_u64),
                            "video":event.get("video").and_then(Value::as_u64),
                            "inactive":event.get("inactive").and_then(Value::as_u64),
                            "sendonly":event.get("sendonly").and_then(Value::as_u64),
                            "bundle":event.get("bundle").and_then(Value::as_bool),
                            "embedded_candidates":event.get("embeddedCandidates").and_then(Value::as_u64),
                            "ice_lite":event.get("iceLite").and_then(Value::as_bool),
                            "audio_port_open":event.get("audioPortOpen").and_then(Value::as_bool),
                            "video_port_open":event.get("videoPortOpen").and_then(Value::as_bool),
                            "data_port_open":event.get("dataPortOpen").and_then(Value::as_bool),
                        }));
                    }
                    Some("cloud-ice-rejected") if is_cloud => journal::record("cloud_ice_rejected", json!({
                        "reason":event.get("reason").and_then(Value::as_str)
                            .filter(|reason| matches!(*reason, "unknown_mline" | "address_format" | "candidate_syntax" | "other")),
                        "address_family":event.get("addressFamily").and_then(Value::as_u64),
                        "media_line":event.get("mediaLine").and_then(Value::as_u64),
                        "field_count":event.get("fieldCount").and_then(Value::as_u64),
                        "candidate_type":event.get("candidateType").and_then(Value::as_str)
                            .filter(|kind| matches!(*kind, "host" | "srflx" | "prflx" | "relay")),
                    })),
                    Some("input-ready") if is_cloud => journal::record("cloud_input_ready", json!({})),
                    Some("candidates") => candidates = event.get("candidates").cloned(),
                    Some("lsfg-ready") => journal::record("lsfg_layer_loaded", json!({
                        "multiplier": 2, "performance_mode": prefs.lsfg_performance,
                        "flow_scale": if prefs.lsfg_flow_percent == 75 { 0.75 } else { 1.0 },
                    })),
                    Some("vulkan-window-embedded") => journal::record("vulkan_window_embedded", json!({})),
                    Some("stream-overlay-handoff") => journal::record("stream_overlay_handoff", json!({
                        "active": event.get("active").and_then(Value::as_bool),
                    })),
                    Some("stream-overlay-hint") => journal::record("stream_overlay_hint", json!({
                        "visible": event.get("visible").and_then(Value::as_bool),
                    })),
                    Some("stream-overlay") => {
                        if let Some(open) = event.get("open").and_then(Value::as_bool) {
                            journal::record("stream_overlay", json!({"open":open}));
                            notify(StreamEvent::Overlay(open));
                        }
                    }
                    Some("stream-overlay-key") => {
                        if let Some(key) = OverlayKey::read(&event) { notify(StreamEvent::OverlayKey(key)); }
                    }
                    Some("stream-quit") => notify(StreamEvent::Quit),
                    Some("stream-stats") => notify(StreamEvent::ToggleStats),
                    Some("stream-diagnostics") => journal::record("stream_diagnostics", json!({
                        "enabled": event.get("enabled").and_then(Value::as_bool),
                    })),
                    Some("hud-stats") => journal::record("stream_diagnostics_sample", json!({
                        "width": event.get("width").and_then(Value::as_u64),
                        "height": event.get("height").and_then(Value::as_u64),
                        "viewport_width": event.get("viewportWidth").and_then(Value::as_u64),
                        "viewport_height": event.get("viewportHeight").and_then(Value::as_u64),
                        "decoded_fps": event.get("decodedFps").and_then(Value::as_f64),
                        "present_fps": event.get("presentFps").and_then(Value::as_f64),
                        "display_hz": event.get("displayHz").and_then(Value::as_u64),
                        "video_mbps": event.get("videoMbps").and_then(Value::as_f64),
                        "limit_kbps": event.get("limitKbps").and_then(Value::as_u64),
                        "rtt_ms": event.get("rttMs").and_then(Value::as_f64),
                        "video_client_ms": event.get("videoClientMs").and_then(Value::as_f64),
                        "jitter_ms": event.get("jitterMs").and_then(Value::as_f64),
                        "rtp_lost": event.get("rtpLost").and_then(Value::as_i64),
                        "queue_depth": event.get("queueDepth").and_then(Value::as_u64),
                        "estimated_queue_drops": event.get("queueDrops").and_then(Value::as_u64),
                        "sink_drops": event.get("sinkDrops").and_then(Value::as_u64),
                    })),
                    Some("video-surface") => journal::record("video_surface", json!({
                        "width": event.get("width").and_then(Value::as_u64),
                        "height": event.get("height").and_then(Value::as_u64),
                    })),
                    Some("video-scaling") => {
                        let scaling = event.get("method").and_then(Value::as_str)
                            .filter(|method| matches!(*method, "lanczos" | "fit"));
                        journal::record("video_scaling", json!({"method":scaling,
                            "source_width":event["sourceWidth"].as_u64(),"source_height":event["sourceHeight"].as_u64(),
                            "output_width":event["outputWidth"].as_u64(),"output_height":event["outputHeight"].as_u64(),
                        }));
                    }
                    Some("stream-input-focus") => journal::record("stream_input_focus", json!({
                        "active":event.get("active").and_then(Value::as_bool),
                    })),
                    Some("stream-input-rearmed") => journal::record("stream_input_rearmed", json!({})),
                    Some("xbox-guide-pulse") => journal::record("xbox_guide_pulse", json!({
                        "pressed":event.get("pressed").and_then(Value::as_bool),
                    })),
                    Some("stream-input-audit") => journal::record("stream_input_audit", json!({
                        "buttons":event.get("buttons").and_then(Value::as_u64),
                        "axis_active":event.get("axisActive").and_then(Value::as_bool),
                        "trigger_active":event.get("triggerActive").and_then(Value::as_bool),
                    })),
                    Some("video") => {
                        if first_video.is_none() {
                            notify(StreamEvent::VideoReady);
                            if interactive && window.is_some() {
                                command(&mut player, json!({"action":"overlay-hint"}))?;
                            }
                        }
                        first_video.get_or_insert_with(Instant::now);
                        let width = event.get("width").and_then(Value::as_u64).unwrap_or_default();
                        let height = event.get("height").and_then(Value::as_u64).unwrap_or_default();
                        stable_since = (height == u64::from(requested_height) && width == u64::from(requested_width))
                            .then(Instant::now);
                        let decoder = event.get("decoder").and_then(Value::as_str).unwrap_or("GStreamer");
                        let decoder = match decoder {
                            "vah264dec" | "avdec_h264" | "vtdec" | "vtdec_hw" | "d3d11h264dec" | "d3d12h264dec" => decoder,
                            _ => "unknown",
                        };
                        let sink = match event.get("renderer").and_then(Value::as_str) {
                            Some(name @ ("vulkansink" | "ximagesink" | "autovideosink")) => Some(name),
                            _ => None,
                        };
                        let conversion = match event.get("conversion").and_then(Value::as_str) {
                            Some(name @ ("cpu_bgra" | "gpu_nv12")) => Some(name),
                            _ => None,
                        };
                        journal::record("video_active", json!({"provider":provider,"width": width, "height": height, "decoder": decoder,
                            "requested_width": requested_width, "requested_height": requested_height, "renderer":sink,
                            "conversion":conversion,
                            "motion_probe": event.get("motionProbe").and_then(Value::as_str).filter(|value| matches!(*value, "inline" | "parallel"))}));
                        status(format!("{width}×{height} · {decoder}"));
                    }
                    Some("stats") => {
                        let count = |key| event.get(key).and_then(Value::as_u64).unwrap_or_default();
                        journal::record("video_stats", json!({
                            "requested_height": requested_height, "width": count("width"), "height": count("height"),
                            "worker_time_ms": event.get("time").and_then(Value::as_f64),
                            "encoded_bytes": count("encodedBytes"), "decoded_frames": count("decodedFrames"),
                            "rendered_frames": count("renderedFrames"), "sink_dropped": count("sinkDropped"),
                            "queue_input": count("queueInput"), "queue_output": count("queueOutput"), "queue_depth": count("queueDepth"),
                        }));
                    }
                    Some("vulkan-stage-profile") => {
                        let samples: Vec<_> = event.get("samples").and_then(Value::as_array).into_iter().flatten()
                            .filter_map(Value::as_array).filter(|sample| sample.len() == 5
                                && sample[..4].iter().all(Value::is_number) && sample[4].is_boolean()).collect();
                        journal::record("vulkan_stage_profile", json!({
                            "worker_time_ms": event.get("time").and_then(Value::as_f64),
                            "width": event.get("width").and_then(Value::as_u64),
                            "height": event.get("height").and_then(Value::as_u64),
                            "samples": samples,
                            "parallel_motion_ms": event.get("motion").and_then(Value::as_array)
                                .into_iter().flatten().filter_map(Value::as_f64).collect::<Vec<_>>(),
                        }));
                    }
                    Some("capture-negotiation") => {
                        journal::record("capture_time_negotiation", json!({
                            "absolute_capture_time": event.get("capture").and_then(Value::as_bool),
                            "absolute_send_time": event.get("sendTime").and_then(Value::as_bool),
                        }));
                    }
                    Some("transport-stats") => {
                        let candidate = match event.get("candidateType").and_then(Value::as_str) {
                            Some(value @ ("host" | "srflx" | "prflx" | "relay")) => Some(value),
                            _ => None,
                        };
                        journal::record("transport_stats", json!({
                            "selected_pair": event.get("selectedPair").and_then(Value::as_bool),
                            "candidate_type": candidate,
                            "rtt_ms": event.get("rttMs").and_then(Value::as_f64),
                            "video_rtp_packets": event.get("videoRtpPackets").and_then(Value::as_u64),
                            "capture_timestamps": event.get("captureTimestamps").and_then(Value::as_u64),
                            "capture_clock_offset": event.get("captureClockOffset").and_then(Value::as_bool),
                            "capture_clock_offset_max_ms": event.get("captureClockOffsetMaxMs").and_then(Value::as_f64),
                            "video_sender_reports": event.get("videoSenderReports").and_then(Value::as_u64),
                            "latest_sender_report_gap_ms": event.get("latestSenderReportGapMs").and_then(Value::as_f64),
                        }));
                    }
                    Some("trial") if benchmark && stable_since.is_some() => {
                        if let Some(measurement) = latency_sample(&event, requested_height) {
                            journal::record("latency_sample", measurement);
                            samples += 1;
                        }
                    }
                    Some("input-timing") if benchmark && stable_since.is_some() => {
                        if let Some(elapsed) = elapsed_between(&event, "polledAt", "sentAt") {
                            journal::record("latency_input", json!({
                                "requested_height": requested_height, "poll_to_send_ms": elapsed,
                                "queued_before_bytes": event.get("queuedBeforeBytes").and_then(Value::as_u64),
                            }));
                        }
                    }
                    Some("video-timing") if benchmark && stable_since.is_some_and(|time| time.elapsed() >= Duration::from_secs(4)) => {
                        if let Some(elapsed) = elapsed_between(&event, "receivedAt", "sinkAt") {
                            journal::record("latency_video", json!({
                                "requested_height": requested_height, "receive_to_sink_ms": elapsed,
                                "pre_jitter": event.get("preJitter").and_then(Value::as_bool) == Some(true),
                                "capture_to_first_rtp_wall_ms": elapsed_between_signed(&event, "captureAt", "receivedAt"),
                                "sender_report_wall_gap_ms": event.get("srWallGapMs").and_then(Value::as_f64),
                            }));
                        }
                    }
                    Some("trial-error") if benchmark => {
                        let reason = match event.get("message").and_then(Value::as_str) {
                            Some("Hold the camera still and try again") => "unstable_scene",
                            Some("No confirmed camera turn") => "no_turn_detected",
                            Some("Native benchmarking requires NumPy") => return Err("Latency benchmark requires NumPy".into()),
                            _ => "trial_failed",
                        };
                        journal::record("latency_sample_rejected", json!({"requested_height": requested_height, "reason": reason}));
                    }
                    Some("error") => {
                        journal::record("gstreamer_error", json!({
                            "message": event.get("message").and_then(Value::as_str).unwrap_or("GStreamer playback failed"),
                        }));
                        return Err(event.get("message").and_then(Value::as_str)
                            .unwrap_or("GStreamer playback failed").to_owned());
                    }
                    _ => {}
                },
                Err(mpsc::RecvTimeoutError::Timeout) => {},
                Err(mpsc::RecvTimeoutError::Disconnected) => return Err("GStreamer worker closed".into()),
            }
            if answer_set && candidates.is_some() {
                let remote = xbox.ice(&path, candidates.take().unwrap_or_default())?;
                if is_cloud { journal::record("cloud_ice_accepted", json!({"candidate_count":remote.as_array().map(Vec::len)})); }
                command(&mut player, json!({"action":"ice","candidates":remote}))?;
            }
        }
        if benchmark {
            journal::record("latency_run_finished", json!({
                "requested_height": requested_height, "attempts": attempts, "samples": samples,
            }));
        }
        Ok(())
    })();
    xbox.stop(&path);
    result
}

fn latency_sample(event: &Value, height: u32) -> Option<Value> {
    let time = |key| event.get(key).and_then(Value::as_f64);
    let polled = time("polledAt")?;
    let sent = time("sentAt")?;
    let received = time("receivedAt")?;
    let sink = time("sinkAt")?;
    let scene = event.get("scene").and_then(Value::as_str)
        .filter(|scene| scene.len() == 38 && scene.bytes().all(|byte| byte.is_ascii_hexdigit()))?;
    (polled <= sent && sent <= received && received <= sink).then(|| json!({
        "requested_height": height, "poll_to_send_ms": sent - polled,
        "receive_to_sink_ms": sink - received, "poll_to_turn_ms": sink - polled,
        "pre_jitter": event.get("preJitter").and_then(Value::as_bool) == Some(true),
        "queued_before_bytes": event.get("queuedBeforeBytes").and_then(Value::as_u64),
        "queued_after_bytes": event.get("queuedAfterBytes").and_then(Value::as_u64),
        "scene": scene,
        "frame_pts_ns": event.get("framePts").and_then(Value::as_u64),
        "send_to_capture_wall_ms": elapsed_between_signed(event, "sentAt", "captureAt"),
        "capture_to_first_rtp_wall_ms": elapsed_between_signed(event, "captureAt", "receivedAt"),
        "send_to_first_rtp_ms": received - sent,
        "sender_report_wall_gap_ms": event.get("srWallGapMs").and_then(Value::as_f64),
    }))
}

fn elapsed_between(event: &Value, start: &str, end: &str) -> Option<f64> {
    let elapsed = elapsed_between_signed(event, start, end)?;
    (elapsed >= 0.0).then_some(elapsed)
}

fn elapsed_between_signed(event: &Value, start: &str, end: &str) -> Option<f64> {
    Some(event.get(end)?.as_f64()? - event.get(start)?.as_f64()?)
}
pub fn focus_gamescope(xid: u64) {
    if !cfg!(target_os="linux") || env::var("ALLY_GAMESCOPE_FOCUS").as_deref() != Ok("1") { return; }
    thread::spawn(move || {
        let name = "GAMESCOPECTRL_BASELAYER_WINDOW";
        let xid = xid.to_string();
        let _ = Command::new("xprop").args(["-id", &xid, "-f", "STEAM_GAME", "32c", "-set", "STEAM_GAME", "413095"]).output();
        let _ = Command::new("xprop").args(["-root", "-f", name, "32c", "-set", name, &xid]).output();
    });
}

pub fn runtime_available() -> bool {
    match worker() {
        Ok(Worker::Python(path)) => path.is_file(),
        Ok(Worker::Native(_)) => true,
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("temp dir")
    }

    #[test]
    fn typing_reaches_the_worker_as_text_and_named_keys_as_key_codes() {
        assert_eq!(StreamCommand::Type('я').message(), json!({"action":"type","text":"я"}));
        assert_eq!(StreamCommand::Key(0x0d).message(), json!({"action":"key","code":13}));
        assert_eq!(named_key("enter"), Some(0x0d));
        assert_eq!(named_key("backspace"), Some(0x08));
        assert_eq!(named_key("left"), Some(0x25));
        // A key that types a character has no code of its own here.
        assert_eq!(named_key("a"), None);
        assert_eq!(named_key("space"), None);
    }

    #[test]
    fn the_pc_mouse_item_of_the_stream_menu_reaches_the_worker() {
        assert!(matches!(OverlayKey::read(&json!({"key":"select","index":5})), Some(OverlayKey::Select(5))));
        assert!(OverlayKey::read(&json!({"key":"select","index":6})).is_none());
        assert_eq!(StreamCommand::ToggleMouseMode.message(), json!({"action":"mouse-mode-toggle"}));
    }

    #[test]
    fn a_worker_that_crashed_is_not_a_stream_that_ended() {
        let exit = |script: &str| Command::new("sh").args(["-c", script]).status();
        assert_eq!(left(exit("exit 0")), Ok(()));
        assert!(left(exit("exit 1")).is_err());
        assert!(left(exit("kill -9 $$")).is_err());
        assert!(left(Err(std::io::Error::other("gone"))).is_err());
    }

    #[test]
    fn override_py_is_python_even_when_missing() {
        let dir = temp_dir();
        let script = dir.path().join("worker.py");
        assert_eq!(resolve_worker(Some(script.clone()), None, dir.path().join("none.py")).unwrap(),
            Worker::Python(script));
    }

    #[cfg(unix)]
    #[test]
    fn override_native_requires_exec_bit() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir();
        let bin = dir.path().join("playwake-media");
        fs::write(&bin, b"#!/bin/sh\n").unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o644)).unwrap();
        let error = resolve_worker(Some(bin.clone()), None, dir.path().join("none.py")).unwrap_err();
        assert!(error.contains("not executable"), "{error}");
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(resolve_worker(Some(bin.clone()), None, dir.path().join("none.py")).unwrap(),
            Worker::Native(bin));
    }

    #[cfg(unix)]
    #[test]
    fn override_native_missing_file_errors() {
        let dir = temp_dir();
        let error = resolve_worker(Some(dir.path().join("absent")), None, dir.path().join("none.py")).unwrap_err();
        assert!(error.contains("missing"), "{error}");
    }

    #[test]
    fn sibling_python_wins_over_source() {
        let dir = temp_dir();
        let sibling = dir.path().join("gstreamer_player.py");
        let source = dir.path().join("source.py");
        fs::write(&sibling, b"").unwrap();
        fs::write(&source, b"").unwrap();
        assert_eq!(resolve_worker(None, Some(sibling.clone()), source).unwrap(), Worker::Python(sibling));
    }

    #[test]
    fn source_python_used_without_sibling() {
        let dir = temp_dir();
        let source = dir.path().join("source.py");
        fs::write(&source, b"").unwrap();
        assert_eq!(resolve_worker(None, Some(dir.path().join("absent.py")), source.clone()).unwrap(),
            Worker::Python(source));
    }

    #[test]
    fn missing_everything_errors() {
        let dir = temp_dir();
        assert!(resolve_worker(None, Some(dir.path().join("absent.py")), dir.path().join("none.py")).is_err());
    }

    #[test]
    fn runtime_env_prepends_and_preserves() {
        let dir = temp_dir();
        let root = dir.path().join("runtime");
        fs::create_dir(&root).unwrap();
        let sep = if cfg!(windows) { ";" } else { ":" };
        let existing = std::ffi::OsString::from(format!("/old/plugins{sep}/more"));
        let (plugins, libs) = runtime_env(&root, Some(existing.clone()), Some(existing)).unwrap();
        let plugins = plugins.to_string_lossy().into_owned();
        let libs = libs.to_string_lossy().into_owned();
        assert!(plugins.starts_with(&root.join("gstreamer-1.0").to_string_lossy().into_owned()), "{plugins}");
        assert!(plugins.contains("/old/plugins"), "{plugins}");
        assert!(plugins.ends_with("/more"), "{plugins}");
        assert!(libs.starts_with(&root.to_string_lossy().into_owned()), "{libs}");
        assert!(libs.ends_with("/more"), "{libs}");
    }
}
