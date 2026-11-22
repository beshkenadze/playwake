//! Latency trials: numpy-free port of the Python worker's motion detection
//! (media/gstreamer_player.py:1169-1351). Sparse 110x320 luma/BG sampling,
//! horizontal shift correlation, 38-hex scene fingerprint (flat packbits,
//! fixing the Python per-row padding bug the host gate rejects).

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_video as gst_video;
use gstreamer_video::prelude::*;

pub struct TrialState {
    pub previous: Option<Sample>,
    pub candidate: Option<(f64, u64)>,
    pub frames: u32,
    pub started: Option<f64>,
    pub baseline_moved: bool,
    pub scene: Option<String>,
    pub restore_view: bool,
}

impl Default for TrialState {
    fn default() -> Self {
        Self { previous: None, candidate: None, frames: 0, started: None,
               baseline_moved: false, scene: None, restore_view: false }
    }
}

pub const ROWS: usize = 110;
pub const COLS: usize = 320;

pub struct Sample {
    pub pixels: Vec<i16>,
    pub channels: usize,
}

/// Sparse grid sample of a decoded frame (NV12 luma or BGRx first 2 channels).
pub fn sample_frame(buffer: &gst::Buffer, caps: &gst::Caps) -> Option<Sample> {
    let info = gst_video::VideoInfo::from_caps(caps).ok()?;
    let frame = gst_video::VideoFrame::from_buffer_readable(buffer.copy(), &info).ok()?;
    let (width, height) = (info.width() as usize, info.height() as usize);
    let stride = frame.plane_stride()[0] as usize;
    let data = frame.plane_data(0).ok()?;
    let format = info.format();
    let channels = if format == gst_video::VideoFormat::Nv12 { 1 } else { 2 };
    let mut pixels = Vec::with_capacity(ROWS * COLS * channels);
    for r in 0..ROWS {
        let y = (0.14 + (0.61 - 0.14) * r as f64 / ROWS as f64) * height as f64;
        let y = y as usize;
        for c in 0..COLS {
            let x = (0.10 + (0.90 - 0.10) * c as f64 / COLS as f64) * width as f64;
            let x = x as usize;
            let offset = y * stride + x * if channels == 1 { 1 } else { 4 };
            if offset + channels > data.len() { return None; }
            for ch in 0..channels {
                pixels.push(data[offset + ch] as i16);
            }
        }
    }
    Some(Sample { pixels, channels })
}

/// Sum of absolute differences for horizontal shifts -12..=12 over the
/// middle band, exactly the Python correlation window.
pub fn shift_errors(previous: &[i16], current: &[i16], channels: usize) -> [i64; 25] {
    let mut errors = [0i64; 25];
    let row_stride = COLS * channels;
    for r in (5..ROWS - 5).step_by(4) {
        for c in (13..COLS - 13).step_by(3) {
            let base = r * row_stride + c * channels;
            for (shift, error) in errors.iter_mut().enumerate() {
                let shift = shift as isize - 12;
                let sc = (c as isize + shift) as usize;
                let idx = r * row_stride + sc * channels;
                for ch in 0..channels {
                    *error += (previous[base + ch] - current[idx + ch]).abs() as i64;
                }
            }
        }
    }
    errors
}

/// Python turn predicate: noise floor 1600/channel and >=18% improvement.
pub fn is_turn(errors: &[i64; 25], channels: usize) -> Option<usize> {
    let zero = errors[12];
    if zero <= 1600 * channels as i64 { return None; }
    let best = (0..25).min_by_key(|i| errors[*i]).unwrap();
    if (zero - errors[best]) as f64 / zero as f64 >= 0.18 { Some(best) } else { None }
}

/// Flat packbits of the 10x15 tile-gradient bits -> 19 bytes -> 38 hex chars
/// (host latency_sample gate requires exactly 38).
pub fn scene_fingerprint(pixels: &[i16], channels: usize) -> String {
    // luminance per sample
    let mut luma = vec![0f64; ROWS * COLS];
    for i in 0..ROWS * COLS {
        let mut sum = 0f64;
        for ch in 0..channels { sum += pixels[i * channels + ch] as f64; }
        luma[i] = sum / channels as f64;
    }
    // tiles: reshape(10, 11, 16, 20).mean(axis=(1,3)) -> (10,16)
    let mut tiles = [[0f64; 16]; 10];
    for tr in 0..10 {
        for tc in 0..16 {
            let mut sum = 0f64;
            for r in 0..11 {
                for c in 0..20 {
                    sum += luma[(tr * 11 + r) * COLS + tc * 20 + c];
                }
            }
            tiles[tr][tc] = sum / 220.0;
        }
    }
    let mut bits = Vec::with_capacity(150);
    for tr in 0..10 {
        for tc in 0..15 {
            bits.push(tiles[tr][tc + 1] > tiles[tr][tc]);
        }
    }
    let mut bytes = vec![0u8; 19];
    for (i, bit) in bits.iter().enumerate() {
        if *bit { bytes[i / 8] |= 1 << (7 - i % 8); }
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat(channels: usize) -> Vec<i16> {
        vec![7; ROWS * COLS * channels]
    }

    #[test]
    fn static_frame_is_not_a_turn() {
        let a = flat(1);
        let errors = shift_errors(&a, &a, 1);
        assert_eq!(errors[12], 0);
        assert!(is_turn(&errors, 1).is_none());
    }

    #[test]
    fn shifted_frame_detected_with_best_shift() {
        let channels = 1;
        let mut a = vec![0i16; ROWS * COLS * channels];
        let mut b = vec![0i16; ROWS * COLS * channels];
        // vertical stripe pattern shifted by 4 columns in b
        for r in 0..ROWS {
            for c in 0..COLS {
                let v = if (c / 16) % 2 == 0 { 200 } else { 20 };
                a[r * COLS + c] = v;
                let sc = c as isize - 4;
                let v2 = if sc >= 0 && ((sc as usize / 16) % 2 == 0) { 200 } else { 20 };
                b[r * COLS + c] = v2;
            }
        }
        let errors = shift_errors(&a, &b, channels);
        let best = is_turn(&errors, channels).expect("turn detected");
        assert_eq!(best, 12 + 4);
    }

    #[test]
    fn noise_below_floor_is_not_a_turn() {
        let a = flat(1);
        let mut b = flat(1);
        for v in b.iter_mut() { *v += 1; }
        let errors = shift_errors(&a, &b, 1);
        assert!(is_turn(&errors, 1).is_none());
    }

    #[test]
    fn scene_fingerprint_is_38_hex() {
        let pixels = flat(2);
        let scene = scene_fingerprint(&pixels, 2);
        assert_eq!(scene.len(), 38);
        assert!(scene.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn scene_fingerprint_changes_with_content() {
        let a = flat(1);
        let mut b = flat(1);
        for r in 0..ROWS {
            for c in 0..COLS {
                b[r * COLS + c] = (c * 255 / COLS) as i16;
            }
        }
        assert_ne!(scene_fingerprint(&a, 1), scene_fingerprint(&b, 1));
    }

    #[test]
    fn shift_errors_zero_shift_matches_identical() {
        let a = flat(2);
        let errors = shift_errors(&a, &a, 2);
        assert!(errors.iter().all(|e| *e == 0));
    }
}
/// Sink-pad trial observation: baseline fingerprint, turn detection and
/// trial completion event (Python present_frame trial branch).
pub fn observe_sink_frame(state: &mut crate::state::State, buffer: &gst::Buffer, caps: &gst::Caps,
                          now: f64, pts: u64) {
    let Some(trial) = state.trial.as_mut() else { return };
    let Some(sample) = sample_frame(buffer, caps) else { return };
    trial.frames += 1;
    let turn = trial.previous.as_ref().and_then(|previous| {
        if previous.channels != sample.channels || previous.pixels.len() != sample.pixels.len() { return None; }
        let errors = shift_errors(&previous.pixels, &sample.pixels, sample.channels);
        is_turn(&errors, sample.channels)
    });
    if trial.started.is_none() {
        trial.baseline_moved |= turn.is_some();
        trial.scene = Some(scene_fingerprint(&sample.pixels, sample.channels));
        trial.previous = Some(sample);
        return;
    }
    let Some(best) = turn else { trial.previous = Some(sample); return; };
    if best >= 12 {
        trial.candidate = None;
        trial.previous = Some(sample);
        return;
    }
    let input_timing = state.trial_input_timing.clone();
    if let Some(input_timing) = input_timing {
        let first = trial.candidate;
        if let Some((observed_at, first_pts)) = first {
            if now - observed_at <= 85.0 {
                let timing = state.frame_timing.get(&first_pts).cloned();
                let scene = trial.scene.clone().unwrap_or_default();
                state.trial = None;
                state.trial_input_timing = None;
                let mut event = input_timing.as_object().cloned().unwrap_or_default();
                if let Some(timing) = timing {
                    event.insert("receivedAt".into(), serde_json::json!(timing.received_at));
                    event.insert("preJitter".into(), serde_json::json!(timing.pre_jitter));
                    event.insert("captureAt".into(), serde_json::json!(timing.capture_at));
                    event.insert("srWallGapMs".into(), serde_json::json!(timing.sr_wall_gap_ms));
                }
                event.insert("sinkAt".into(), serde_json::json!(observed_at));
                event.insert("scene".into(), serde_json::json!(scene));
                event.insert("framePts".into(), serde_json::json!(first_pts));
                crate::protocol::event("trial", serde_json::Value::Object(event));
                return;
            }
        }
        trial.candidate = Some((now, pts));
    }
    trial.previous = Some(sample);
}
