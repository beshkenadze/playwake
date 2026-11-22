//! JSON-lines stdio protocol shared with the host (`src/media.rs`).
//! Commands arrive one object per line on stdin; events leave one object per
//! line on stdout with a top-level `event` tag. Field names are byte-identical
//! to the Python worker's protocol.

use serde::Deserialize;
use serde_json::{Value, json};
use std::io::Write;

#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "kebab-case")]
pub enum Command {
    Start {
        bitrate: u32,
        #[serde(default = "default_width")]
        width: u32,
        #[serde(default = "default_height")]
        height: u32,
        #[serde(default)]
        benchmark: bool,
        #[serde(default)]
        capture_diagnostics: bool,
        #[serde(default = "default_renderer")]
        renderer: String,
        #[serde(default)]
        lsfg: bool,
        #[serde(default)]
        interactive: bool,
        #[serde(default)]
        russian: bool,
        #[serde(default)]
        xbox_palette: bool,
        #[serde(default)]
        compact_stats: bool,
        #[serde(default = "default_volume")]
        volume: u32,
        #[serde(default = "default_console_name")]
        console_name: String,
    },
    Answer {
        sdp: String,
    },
    Ice {
        candidates: Vec<Value>,
    },
    Pulse {
        #[serde(default)]
        axis: f64,
        #[serde(default)]
        buttons: u16,
        #[serde(default = "default_pulse_duration")]
        duration: u64,
        #[serde(default)]
        trial: bool,
    },
    Trial {
        #[serde(default)]
        restore_view: bool,
    },
    Gamepad {
        axes: Vec<i16>,
        buttons: u16,
        #[serde(default)]
        lt: u16,
        #[serde(default)]
        rt: u16,
    },
    OverlayHint,
    OverlayOpen,
    OverlayClose,
    XboxGuide,
    DiagnosticsToggle,
    OverlaySelection {
        index: usize,
    },
    InputFocus {
        active: bool,
    },
    Stats,
    Stop,
}

fn default_width() -> u32 { 1920 }
fn default_height() -> u32 { 1080 }
fn default_renderer() -> String { "auto".into() }
fn default_volume() -> u32 { 100 }
fn default_console_name() -> String { "Xbox".into() }
fn default_pulse_duration() -> u64 { 230 }

pub fn parse(line: &str) -> Option<Command> {
    serde_json::from_str(line).ok()
}

pub fn emit(value: Value) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{value}");
    let _ = out.flush();
}

pub fn event(name: &str, fields: Value) {
    let mut object = match fields {
        Value::Object(map) => map,
        _ => serde_json::Map::new(),
    };
    object.insert("event".into(), json!(name));
    emit(Value::Object(object));
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn error(message: impl AsRef<str>) {
    event("error", json!({ "message": message.as_ref() }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_command_matches_host_payload() {
        // Exact shape written by src/media.rs play() start command.
        let line = json!({
            "action": "start", "bitrate": 20000, "width": 1920, "height": 1080,
            "benchmark": false, "capture_diagnostics": false, "renderer": "auto",
            "lsfg": false, "interactive": true, "cloud": false, "stun": null,
            "lanczos_upscale": false, "russian": false, "xbox_palette": true,
            "compact_stats": true, "control_layout": "ally", "control_device": "ROG Ally",
            "volume": 100, "console_name": "Xbox"
        });
        let Command::Start { bitrate, width, height, interactive, compact_stats, volume, .. } =
            parse(&line.to_string()).expect("start parses") else { panic!("wrong variant") };
        assert_eq!((bitrate, width, height, volume), (20000, 1920, 1080, 100));
        assert!(interactive && compact_stats);
    }

    #[test]
    fn start_defaults_fill_missing_options() {
        let Command::Start { width, height, renderer, volume, console_name, benchmark, .. } =
            parse(r#"{"action":"start","bitrate":10000}"#).expect("minimal start") else { panic!() };
        assert_eq!((width, height, volume), (1920, 1080, 100));
        assert_eq!(renderer, "auto");
        assert_eq!(console_name, "Xbox");
        assert!(!benchmark);
    }

    #[test]
    fn kebab_case_actions_parse() {
        assert!(matches!(parse(r#"{"action":"overlay-open"}"#), Some(Command::OverlayOpen)));
        assert!(matches!(parse(r#"{"action":"diagnostics-toggle"}"#), Some(Command::DiagnosticsToggle)));
        assert!(matches!(parse(r#"{"action":"input-focus","active":false}"#), Some(Command::InputFocus { active: false })));
        assert!(matches!(parse(r#"{"action":"stop"}"#), Some(Command::Stop)));
    }

    #[test]
    fn unknown_action_is_ignored() {
        assert!(parse(r#"{"action":"does-not-exist"}"#).is_none());
        assert!(parse("not json").is_none());
    }

    #[test]
    fn pulse_defaults() {
        let Some(Command::Pulse { axis, buttons, duration, trial }) = parse(r#"{"action":"pulse"}"#) else { panic!() };
        assert_eq!((axis, buttons, duration, trial), (0.0, 0, 230, false));
    }
}