use serde::{Deserialize, Serialize};
use std::{env, fs, io, path::PathBuf};

#[derive(Deserialize, Serialize)]
pub struct Session {
    client_id: String,
    pub refresh_token: String,
    #[serde(default)]
    pub catalog_key: Option<String>,
}

pub(super) fn path() -> io::Result<PathBuf> {
    if let Some(path) = env::var_os("ALLY_SESSION_FILE").filter(|path| !path.is_empty()) {
        return Ok(path.into());
    }
    let root = if cfg!(target_os = "windows") {
        env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        env::var_os("HOME").map(|home| PathBuf::from(home).join("Library/Application Support"))
    } else {
        env::var_os("XDG_STATE_HOME").filter(|root| !root.is_empty()).map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
    };
    root.map(|root| root.join("ally-remote-play/session.json"))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Application state directory unavailable"))
}

pub fn load(client_id: &str) -> io::Result<Option<Session>> {
    let path = path()?;
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !metadata.is_file() || metadata.len() > 64 * 1024 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "Invalid session file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "Session file is accessible to other users"));
        }
    }
    let session: Session = serde_json::from_reader(fs::File::open(path)?)?;
    Ok((session.client_id == client_id && !session.refresh_token.is_empty()).then_some(session))
}

pub fn save(client_id: &str, refresh_token: &str, catalog_key: &str) -> io::Result<()> {
    write_json(path()?, &Session { client_id: client_id.into(), refresh_token: refresh_token.into(),
        catalog_key: Some(catalog_key.into()) })
}

#[derive(Clone, Copy, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Language {
    #[default]
    System,
    English,
    Russian,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(default)]
pub struct Preferences {
    pub lsfg_enabled: bool,
    pub lsfg_performance: bool,
    pub lsfg_flow_percent: u8,
    pub lanczos_upscale: bool,
    pub height: u32,
    pub bitrate_mbps: u32,
    /// The bitrate asked from a PC, in Mbit/s. A PC is on the local network,
    /// which carries far more than a cloud stream needs.
    pub pc_bitrate_mbps: u32,
    pub compact_stats: bool,
    pub auto_connect: bool,
    pub xbox_palette: bool,
    pub language: Language,
    pub volume: u32,
    pub mute: bool,
    /// How long incoming media waits for late packets, in milliseconds.
    pub jitter_ms: u32,
    /// HDR for PC streams, where the screen shows it and the PC encodes it.
    pub pc_hdr: bool,
    /// The height of a PC stream, 0 for the screen's own. A smaller stream is
    /// scaled up to the screen by SteamOS, with the filter chosen there.
    pub pc_stream_height: u32,
}

/// The stream heights besides the screen's that the settings offer for a PC.
/// The PC's virtual display takes any of them.
pub const PC_STREAM_HEIGHTS: [u32; 9] = [2160, 1440, 1200, 1080, 900, 800, 720, 600, 540];

/// The PC stream height one step down (`direction` 1) or up (-1) on a screen
/// `screen` pixels high, stopping at the ends; 0 is the screen's own height.
pub fn next_pc_stream_height(current: u32, direction: isize, screen: u32) -> u32 {
    // From the largest picture to the smallest, with the screen in its place.
    let mut choices: Vec<(u32, u32)> = PC_STREAM_HEIGHTS.into_iter().filter(|height| *height != screen)
        .map(|height| (height, height)).chain([(screen, 0)]).collect();
    choices.sort_by(|a, b| b.0.cmp(&a.0));
    let index = choices.iter().position(|(_, choice)| *choice == current)
        .unwrap_or_else(|| choices.iter().position(|(_, choice)| *choice == 0).unwrap_or(0));
    choices[(index as isize + direction).clamp(0, choices.len() as isize - 1) as usize].1
}

/// The jitter buffer lengths the settings offer, in milliseconds.
pub const JITTER_CHOICES: [u32; 7] = [5, 10, 15, 20, 25, 30, 50];

impl Default for Preferences {
    fn default() -> Self {
        Self { lsfg_enabled: false, lsfg_performance: true, lsfg_flow_percent: 100,
            lanczos_upscale: false,
            height: 1080, bitrate_mbps: 20, pc_bitrate_mbps: 20, compact_stats: false,
            auto_connect: false, xbox_palette: false, language: Language::System, volume: 100, mute: false,
            jitter_ms: 20, pc_hdr: true, pc_stream_height: 0 }
    }
}

impl Preferences {
    pub fn lsfg_flow_scale(self) -> &'static str {
        if self.lsfg_flow_percent == 75 { "0.75" } else { "1.0" }
    }

    pub fn russian(self) -> bool {
        match self.language {
            Language::Russian => true,
            Language::English => false,
            Language::System => ["LC_ALL", "LC_MESSAGES", "LANG"].iter()
                .find_map(|var| env::var(var).ok().filter(|value| !value.is_empty()))
                .is_some_and(|value| value.starts_with("ru")),
        }
    }
}

pub fn load_preferences() -> Preferences {
    let mut prefs: Preferences = path().ok()
        .and_then(|path| fs::File::open(path.with_extension("preferences.json")).ok())
        .and_then(|file| serde_json::from_reader(file).ok())
        .unwrap_or_default();
    prefs.height = supported_stream_height(prefs.height);
    prefs.bitrate_mbps = prefs.bitrate_mbps.clamp(5, 40);
    prefs.pc_bitrate_mbps = next_pc_bitrate(prefs.pc_bitrate_mbps, 0);
    if !PC_STREAM_HEIGHTS.contains(&prefs.pc_stream_height) { prefs.pc_stream_height = 0; }
    prefs.volume = prefs.volume.min(100);
    prefs.jitter_ms = nearest_jitter(prefs.jitter_ms);
    prefs
}

/// The jitter buffer one step up (`direction` 1) or down (-1), stopping at the ends.
pub fn next_jitter(current: u32, direction: isize) -> u32 {
    let index = JITTER_CHOICES.iter().position(|choice| *choice == current).unwrap_or(3);
    JITTER_CHOICES[(index as isize + direction).clamp(0, JITTER_CHOICES.len() as isize - 1) as usize]
}

/// The PC bitrate `steps` steps of 5 Mbit/s up or down, within 5 to 150.
pub fn next_pc_bitrate(current: u32, steps: isize) -> u32 {
    (current as isize + 5 * steps).clamp(5, 150) as u32
}

/// The offered jitter buffer length closest to `saved`.
pub fn nearest_jitter(saved: u32) -> u32 {
    JITTER_CHOICES.into_iter().min_by_key(|choice| choice.abs_diff(saved)).unwrap_or(20)
}

fn supported_stream_height(saved: u32) -> u32 {
    // The retired 800p and 900p aliases could leave Xbox at 1440p.
    match saved {
        720 | 800 | 900 => 720,
        1440 => 1440,
        _ => 1080,
    }
}

pub fn save_preferences(prefs: Preferences) -> io::Result<()> {
    write_json(path()?.with_extension("preferences.json"), &prefs)
}

pub(super) fn write_json(path: PathBuf, data: &impl Serialize) -> io::Result<()> {
    let parent = path.parent().filter(|path| !path.as_os_str().is_empty()).unwrap_or_else(|| std::path::Path::new("."));
    let mut dir = fs::DirBuilder::new();
    dir.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut dir, 0o700);
    dir.create(parent)?;
    let file = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer(file.as_file(), data)?;
    file.as_file().sync_all()?;
    file.persist(path)?;
    Ok(())
}

pub fn clear() -> io::Result<()> {
    match fs::remove_file(path()?) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_saved_jitter_buffer_snaps_to_an_offered_length() {
        assert_eq!(nearest_jitter(20), 20);
        assert_eq!(nearest_jitter(0), 5);
        assert_eq!(nearest_jitter(42), 50);
        assert_eq!(nearest_jitter(1000), 50);
        // Preferences saved before the setting existed keep today's 20 ms.
        let prefs: Preferences = serde_json::from_str(r#"{"bitrate_mbps":20}"#).unwrap();
        assert_eq!(prefs.jitter_ms, 20);
    }

    #[test]
    fn the_jitter_setting_steps_through_the_offered_lengths_and_stops_at_the_ends() {
        assert_eq!(next_jitter(20, 1), 25);
        assert_eq!(next_jitter(30, 1), 50);
        assert_eq!(next_jitter(50, 1), 50);
        assert_eq!(next_jitter(10, -1), 5);
        assert_eq!(next_jitter(5, -1), 5);
        // A value that is not offered starts from the default.
        assert_eq!(next_jitter(17, 1), 25);
    }

    #[test]
    fn the_pc_stream_height_steps_through_heights_around_the_screen() {
        assert_eq!(next_pc_stream_height(0, 1, 800), 720);
        assert_eq!(next_pc_stream_height(720, 1, 800), 600);
        assert_eq!(next_pc_stream_height(540, 1, 800), 540);
        assert_eq!(next_pc_stream_height(720, -1, 800), 0);
        // Above the screen too: the PC's virtual display takes any size.
        assert_eq!(next_pc_stream_height(0, -1, 800), 900);
        assert_eq!(next_pc_stream_height(2160, -1, 800), 2160);
        assert_eq!(next_pc_stream_height(0, 1, 1080), 900);
        assert_eq!(next_pc_stream_height(0, -1, 1080), 1200);
        assert_eq!(next_pc_stream_height(1080, 1, 800), 900);
        // A height that is not offered starts from the screen's own.
        assert_eq!(next_pc_stream_height(1000, 1, 800), 720);
        let prefs: Preferences = serde_json::from_str(r#"{"bitrate_mbps":20}"#).unwrap();
        assert_eq!(prefs.pc_stream_height, 0);
    }

    #[test]
    fn the_pc_bitrate_steps_by_five_within_its_limits_and_starts_at_twenty() {
        assert_eq!(next_pc_bitrate(50, 1), 55);
        assert_eq!(next_pc_bitrate(150, 1), 150);
        assert_eq!(next_pc_bitrate(5, -1), 5);
        assert_eq!(next_pc_bitrate(0, 0), 5);
        assert_eq!(next_pc_bitrate(4000, 0), 150);
        // Preferences saved before the setting existed, with a low cloud limit.
        let prefs: Preferences = serde_json::from_str(r#"{"bitrate_mbps":10}"#).unwrap();
        assert_eq!((prefs.bitrate_mbps, prefs.pc_bitrate_mbps), (10, 20));
    }

    #[test]
    fn reads_existing_lsfg_only_preferences() {
        let prefs: Preferences = serde_json::from_str(r#"{"lsfg_enabled":true}"#).unwrap();
        assert!(prefs.lsfg_enabled);
        assert!(prefs.lsfg_performance);
        assert_eq!(prefs.lsfg_flow_scale(), "1.0");
        assert_eq!(prefs.height, 1080);
        assert_eq!(prefs.bitrate_mbps, 20);
        assert_eq!(prefs.volume, 100);
        assert!(!prefs.auto_connect);
    }

    #[test]
    fn old_custom_resolutions_use_the_lower_supported_xbox_preset() {
        for saved in [800, 900] {
            assert_eq!(supported_stream_height(saved), 720);
        }
        for supported in [720, 1080, 1440] {
            assert_eq!(supported_stream_height(supported), supported);
        }
    }
}
