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
    pub height: u32,
    pub bitrate_mbps: u32,
    pub compact_stats: bool,
    pub auto_connect: bool,
    pub xbox_palette: bool,
    pub language: Language,
    pub volume: u32,
    pub mute: bool,
}

impl Default for Preferences {
    fn default() -> Self {
        Self { lsfg_enabled: false, height: 1080, bitrate_mbps: 20, compact_stats: false,
            auto_connect: false, xbox_palette: false, language: Language::System, volume: 100, mute: false }
    }
}

impl Preferences {
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
    prefs.volume = prefs.volume.min(100);
    prefs
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
    fn reads_existing_lsfg_only_preferences() {
        let prefs: Preferences = serde_json::from_str(r#"{"lsfg_enabled":true}"#).unwrap();
        assert!(prefs.lsfg_enabled);
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
