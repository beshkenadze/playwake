//! Game covers kept on disk, so the library shows them without waiting for
//! the network.

use crate::session_store;
use std::{collections::HashSet, fs, io::Read, path::{Path, PathBuf}, sync::Mutex};

const MAX_BYTES: u64 = 2 * 1024 * 1024;
/// The page of the library: the grid redraws after this many new covers.
const BATCH: usize = 9;

/// The covers of one source, in a directory of their own.
pub struct Set {
    name: &'static str,
    /// One fetch at a time: a later list must not lose its covers to an
    /// earlier fetch's pruning, and two fetches must not share a file.
    fetching: Mutex<()>,
}

impl Set {
    pub const fn new(name: &'static str) -> Self {
        Self { name, fetching: Mutex::new(()) }
    }

    fn dir(&self) -> Option<PathBuf> {
        Some(session_store::path().ok()?.parent()?.join("covers").join(self.name))
    }
}

/// The file name of a cover: its URL without the scheme, with every byte
/// other than a letter, digit or dot written as `_` and two hex digits.
fn name(url: &str) -> Option<String> {
    let (_, rest) = url.split_once("://")?;
    let name: String = rest.bytes().map(|b| if b.is_ascii_alphanumeric() || b == b'.' { char::from(b).to_string() }
        else { format!("_{b:02x}") }).collect();
    // With ".part" the name stays under the 255 bytes file systems allow.
    (!name.is_empty() && name.len() <= 240 && !name.starts_with('.')).then_some(name)
}

/// The cover on disk for `key`, if it was fetched. A key is the cover's URL,
/// or another `scheme://` name for a cover that is not on the web.
pub fn file(set: &Set, key: &str) -> Option<PathBuf> {
    let path = set.dir()?.join(name(key)?);
    path.is_file().then_some(path)
}

/// Fetches with `load` the covers that are not on disk yet, calls `progress`
/// as they arrive, then removes the covers no longer listed.
pub fn fetch(set: &Set, keys: &[&str], load: impl FnMut(&str) -> Option<Vec<u8>>, progress: impl Fn()) {
    let Ok(_one) = set.fetching.lock() else { return };
    if let Some(dir) = set.dir() { fetch_in(&dir, keys, load, progress); }
}

fn fetch_in(dir: &Path, keys: &[&str], mut load: impl FnMut(&str) -> Option<Vec<u8>>, progress: impl Fn()) {
    if fs::create_dir_all(dir).is_err() { return; }
    let mut fetched = 0;
    for key in keys {
        let Some(name) = name(key) else { continue };
        let path = dir.join(&name);
        if path.is_file() { continue; }
        let Some(bytes) = load(key).filter(|bytes| !bytes.is_empty() && bytes.len() as u64 <= MAX_BYTES) else { continue };
        let part = dir.join(format!("{name}.part"));
        if fs::write(&part, bytes).and_then(|()| fs::rename(&part, &path)).is_err() {
            let _ = fs::remove_file(&part);
            continue;
        }
        fetched += 1;
        if fetched % BATCH == 0 { progress(); }
    }
    if fetched % BATCH != 0 { progress(); }
    prune(dir, &keys.iter().filter_map(|key| name(key)).collect());
}

/// A cover from the web.
pub fn download(client: &reqwest::blocking::Client, url: &str) -> Option<Vec<u8>> {
    let response = client.get(url).send().ok()?;
    let image = response.headers().get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok()).is_some_and(|value| value.starts_with("image/"));
    if !response.status().is_success() || !image { return None; }
    let mut bytes = Vec::new();
    response.take(MAX_BYTES + 1).read_to_end(&mut bytes).ok()?;
    Some(bytes)
}

fn prune(dir: &Path, wanted: &HashSet<String>) {
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        if !wanted.contains(entry.file_name().to_string_lossy().as_ref()) { let _ = fs::remove_file(entry.path()); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cover_file_is_named_after_its_url_and_stays_in_its_directory() {
        assert_eq!(name("https://store-images.s-microsoft.com/image/apps.1.poster?w=1").as_deref(),
            Some("store_2dimages.s_2dmicrosoft.com_2fimage_2fapps.1.poster_3fw_3d1"));
        assert_ne!(name("https://a/b-c"), name("https://a/b_c"));
        assert!(name("https://../../etc/passwd").is_none());
        assert!(name("no scheme").is_none());
        assert!(name("https://store-images.s-microsoft.com/image/apps.3823.14330850369313893.a687698c-b891-44f6-9576-fe28978ce915.5aada4fa-d850-4eb5-9100-33a81a5cde09?w=144&h=216").is_some());
        assert!(name(&format!("https://{}", "a".repeat(241))).is_none());
    }

    #[test]
    fn fetch_loads_only_missing_covers_and_drops_unlisted_ones() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("kept"), b"old").unwrap();
        fs::write(dir.path().join("stale"), b"old").unwrap();
        let mut asked = Vec::new();
        let progress = std::cell::Cell::new(0);
        fetch_in(dir.path(), &["x://kept", "x://new", "x://missing"], |key| {
            asked.push(key.to_owned());
            (key == "x://new").then(|| b"image".to_vec())
        }, || progress.set(progress.get() + 1));
        assert_eq!(asked, ["x://new", "x://missing"]);
        assert_eq!(fs::read(dir.path().join("new")).unwrap(), b"image");
        assert_eq!(fs::read(dir.path().join("kept")).unwrap(), b"old");
        assert!(!dir.path().join("stale").exists() && !dir.path().join("missing").exists());
        assert_eq!(progress.get(), 1);
    }

    #[test]
    fn prune_keeps_only_the_listed_covers() {
        let dir = tempfile::tempdir().unwrap();
        for file in ["kept", "gone", "kept.part"] { fs::write(dir.path().join(file), b"x").unwrap(); }
        prune(dir.path(), &HashSet::from(["kept".to_owned()]));
        let mut left: Vec<_> = fs::read_dir(dir.path()).unwrap().map(|entry| entry.unwrap().file_name()).collect();
        left.sort();
        assert_eq!(left, ["kept"]);
    }
}
