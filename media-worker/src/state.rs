//! Shared worker state touched by streaming-thread pad probes (short mutex
//! holds, no IO) and read by main-loop timers for stats/HUD emission.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use gstreamer::prelude::*;
use crate::transport::Transport;
use crate::trial::TrialState;

#[derive(Clone, Debug, Default)]
pub struct Timing {
    pub received_at: f64,
    pub pre_jitter: bool,
    pub decoded_at: Option<f64>,
    pub sink_at: Option<f64>,
    pub capture_at: Option<f64>,
    pub sr_wall_gap_ms: Option<f64>,
}

#[derive(Default)]
pub struct HudTransport {
    pub rtt_ms: Option<f64>,
    pub jitter_ms: Option<f64>,
    pub rtp_lost: Option<i64>,
    pub pending: bool,
}

pub struct State {
    pub requested_width: u32,
    pub requested_height: u32,
    pub width: u32,
    pub height: u32,
    pub encoded_frames: u64,
    pub encoded_bytes: u64,
    pub decoded_frames: u64,
    pub rendered_frames: u64,
    pub sink_dropped: u64,
    pub queue_in: u64,
    pub queue_out: u64,
    pub frame_timing: HashMap<u64, Timing>,
    pub ingress: HashMap<u64, f64>,
    pub last_video_dims: Option<(u32, u32)>,
    pub last_video_timing_emit: f64,
    pub diagnostics: bool,
    pub hud_transport: HudTransport,
    pub hud_snapshot: Option<(f64, u64, u64)>,
    pub benchmark: bool,
    pub capture_diagnostics: bool,
    pub decoder_name: String,
    pub renderer_name: String,
    pub conversion: Option<String>,
    pub trial: Option<TrialState>,
    pub viewport: (u32, u32),
    pub hud_compact: String,
    pub hud_metrics: Vec<String>,
    pub lsfg: bool,
    pub russian: bool,
    pub trial_input_timing: Option<serde_json::Value>,
    pub transport: Arc<Mutex<Transport>>,
    pub queue_times: HashMap<u64, Stage>,
    pub stage_profile: Vec<(f64, f64, f64, f64, bool)>,
    pub motion_profile: Vec<f64>,
}

#[derive(Default, Clone, Copy)]
pub struct Stage {
    pub entered: f64,
    pub dequeued: Option<f64>,
    pub pre_upload: Option<f64>,
}

impl State {
    pub fn new(requested_width: u32, requested_height: u32, benchmark: bool, capture_diagnostics: bool,
               transport: Arc<Mutex<Transport>>) -> Self {
        Self {
            requested_width,
            requested_height,
            width: requested_width,
            height: requested_height,
            benchmark,
            capture_diagnostics,
            transport,
            ..Default::default()
        }
    }

    pub fn remember_timing(&mut self, pts: u64, timing: Timing) {
        self.frame_timing.insert(pts, timing);
        if self.frame_timing.len() > 180 {
            let oldest = self.frame_timing.keys().next().copied();
            if let Some(key) = oldest { self.frame_timing.remove(&key); }
        }
    }

    pub fn remember_ingress(&mut self, key: u64, now: f64) {
        self.ingress.entry(key).or_insert(now);
        if self.ingress.len() > 512 {
            let oldest = self.ingress.keys().next().copied();
            if let Some(key) = oldest { self.ingress.remove(&key); }
        }
    }

    pub fn queue_depth(&self, queue: Option<&gstreamer::Element>) -> u64 {
        queue.map(|q| q.property::<u32>("current-level-buffers") as u64).unwrap_or(0)
    }

    pub fn queue_drops(&self, depth: u64) -> u64 {
        self.queue_in.saturating_sub(self.queue_out).saturating_sub(depth)
    }
}

impl Default for State {
    fn default() -> Self {
        Self {
            requested_width: 1920,
            requested_height: 1080,
            width: 1920,
            height: 1080,
            encoded_frames: 0,
            encoded_bytes: 0,
            decoded_frames: 0,
            rendered_frames: 0,
            sink_dropped: 0,
            queue_in: 0,
            queue_out: 0,
            frame_timing: HashMap::new(),
            ingress: HashMap::new(),
            last_video_dims: None,
            last_video_timing_emit: 0.0,
            diagnostics: false,
            hud_transport: HudTransport::default(),
            hud_snapshot: None,
            benchmark: false,
            capture_diagnostics: false,
            decoder_name: String::new(),
            renderer_name: String::new(),
            conversion: None,
            trial: None,
            trial_input_timing: None,
            viewport: (1920, 1080),
            hud_compact: String::new(),
            hud_metrics: Vec::new(),
            lsfg: false,
            russian: false,
            transport: Arc::new(Mutex::new(Transport::default())),
            queue_times: HashMap::new(),
            stage_profile: Vec::new(),
            motion_profile: Vec::new(),
        }
    }
}