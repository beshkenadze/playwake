use crate::{journal, lsfg};
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
    pub top_inset: u32,
}

struct Player(Child);

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

fn player_script() -> Result<PathBuf> {
    if let Some(path) = env::var_os("ALLY_MEDIA_WORKER") { return Ok(path.into()); }
    let exe = env::current_exe().map_err(|_| "Cannot locate the application")?;
    let sibling = exe.parent().ok_or("Cannot locate media files")?.join("gstreamer_player.py");
    if sibling.is_file() { return Ok(sibling); }
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("media/gstreamer_player.py");
    if source.is_file() { Ok(source) } else { Err("GStreamer worker file is missing".into()) }
}

fn launch(window: Option<VideoWindow>, lsfg: Option<&lsfg::Runtime>, interactive: bool) -> Result<Player> {
    let python = env::var_os("ALLY_GST_PYTHON")
        .unwrap_or_else(|| if cfg!(target_os="windows") { "python.exe".into() } else { "python3".into() });
    let mut cmd = Command::new(python);
    cmd.arg(player_script()?).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    if let Some(lsfg) = lsfg {
        lsfg.configure(&mut cmd)?;
    } else if interactive {
        cmd.env("DISABLE_LSFGVK", "1").env("DISABLE_LSFG", "1");
    }
    if let Some(window) = window {
        cmd.env("GREENLIGHT_VIDEO_PARENT_XID", window.xid.to_string())
            .env("GREENLIGHT_VIDEO_PARENT_INSET", window.top_inset.to_string());
    }
    if let Some(root) = env::var_os("ALLY_GST_RUNTIME") {
        let root = PathBuf::from(root);
        if !root.is_dir() { return Err("GStreamer runtime directory is missing".into()); }
        let mut plugins = vec![root.join("gstreamer-1.0")];
        if let Some(value) = env::var_os("GST_PLUGIN_PATH") { plugins.extend(env::split_paths(&value)); }
        cmd.env("GST_PLUGIN_PATH", env::join_paths(plugins).map_err(|_| "Invalid GStreamer plugin path")?);
        let variable = if cfg!(target_os="windows") { "PATH" }
            else if cfg!(target_os="macos") { "DYLD_LIBRARY_PATH" } else { "LD_LIBRARY_PATH" };
        let mut libs = vec![root];
        if let Some(value) = env::var_os(variable) { libs.extend(env::split_paths(&value)); }
        cmd.env(variable, env::join_paths(libs).map_err(|_| "Invalid GStreamer library path")?);
    }
    cmd.spawn().map(Player).map_err(|_| "Could not launch Python/GStreamer. Check the runtime installation.".into())
}

fn command(player: &mut Player, message: Value) -> Result<()> {
    writeln!(player.0.stdin.as_mut().ok_or("Media input pipe closed")?, "{message}")
        .map_err(|_| "GStreamer worker stopped".to_string())
}

#[derive(Clone, Copy)]
pub enum Run {
    Interactive { lsfg: bool },
    Timed(Duration),
    Latency,
    Transport,
    Vulkan,
}

pub enum StreamCommand {
    OpenOverlay,
    CloseOverlay,
    XboxGuide,
    InputFocus(bool),
}

pub enum StreamEvent {
    Status(String),
    Overlay(bool),
    OverlayKey(OverlayKey),
}

pub enum OverlayKey { Left, Right, Activate, Back }

pub fn play(xbox: Arc<Xbox>, console: Console, resolution: Resolution, window: Option<VideoWindow>,
            cancel: Arc<AtomicBool>, run: Run, controls: Option<mpsc::Receiver<StreamCommand>>,
            notify: impl Fn(StreamEvent)) -> Result<()> {
    let status = |message| notify(StreamEvent::Status(message));
    status("Preparing Xbox session…".into());
    let (requested_width, requested_height) = resolution.dimensions();
    let benchmark = matches!(run, Run::Latency | Run::Transport | Run::Vulkan);
    let interactive = matches!(run, Run::Interactive { .. });
    let enable_lsfg = matches!(run, Run::Interactive { lsfg: true });
    let lsfg = if enable_lsfg {
        if window.is_none() { return Err("LSFG requires an X11 or XWayland window".into()); }
        Some(lsfg::Runtime::discover()?)
    } else { None };
    let vulkan = enable_lsfg || matches!(run, Run::Vulkan) || env::var("ALLY_VIDEO_RENDERER").as_deref() == Ok("vulkan");
    let renderer = if vulkan { "vulkan" } else { "auto" };
    let bitrate = if benchmark || enable_lsfg { 20000 } else { 20480 };
    journal::record("stream_requested", json!({"bitrate_limit_kbps": bitrate, "width": requested_width, "height": requested_height,
        "renderer": renderer, "lsfg_requested": enable_lsfg || (!interactive && vulkan
            && env::var("LSFGVK_ENV").as_deref() == Ok("1") && env::var("DISABLE_LSFGVK").as_deref() != Ok("1"))}));
    let path = xbox.reserve(&console, resolution)?;
    let result = (|| {
        xbox.provision(&path, &cancel)?;
        journal::record("stream_provisioned", json!({}));
        let mut player = launch(window, lsfg.as_ref(), interactive)?;
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
            "height":requested_height,"benchmark":benchmark,"capture_diagnostics":matches!(run, Run::Transport),
            "renderer":renderer,"lsfg":enable_lsfg}))?;
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
                    let message = match control {
                        StreamCommand::OpenOverlay => json!({"action":"overlay-open"}),
                        StreamCommand::CloseOverlay => json!({"action":"overlay-close"}),
                        StreamCommand::XboxGuide => json!({"action":"xbox-guide"}),
                        StreamCommand::InputFocus(active) => json!({"action":"input-focus","active":active}),
                    };
                    command(&mut player, message)?;
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
            if keepalive.elapsed() >= Duration::from_secs(30) {
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
                    Some("answer-set") => answer_set = true,
                    Some("candidates") => candidates = event.get("candidates").cloned(),
                    Some("lsfg-ready") => journal::record("lsfg_layer_loaded", json!({
                        "multiplier": 2, "performance_mode": true, "flow_scale": 1.0,
                    })),
                    Some("vulkan-window-embedded") => journal::record("vulkan_window_embedded", json!({})),
                    Some("stream-overlay") => {
                        if let Some(open) = event.get("open").and_then(Value::as_bool) {
                            journal::record("stream_overlay", json!({"open":open}));
                            notify(StreamEvent::Overlay(open));
                        }
                    }
                    Some("stream-overlay-key") => {
                        let key = match event.get("key").and_then(Value::as_str) {
                            Some("left") => Some(OverlayKey::Left),
                            Some("right") => Some(OverlayKey::Right),
                            Some("activate") => Some(OverlayKey::Activate),
                            Some("back") => Some(OverlayKey::Back),
                            _ => None,
                        };
                        if let Some(key) = key { notify(StreamEvent::OverlayKey(key)); }
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
                        journal::record("video_active", json!({"width": width, "height": height, "decoder": decoder,
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
                        journal::record("gstreamer_error", json!({}));
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
    player_script().is_ok_and(|path| fs::metadata(path).is_ok_and(|metadata| metadata.is_file()))
}
