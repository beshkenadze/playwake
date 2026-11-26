//! C ABI consumed by the iOS app. All strings are UTF-8 and NUL terminated.
//! Event callbacks receive a pointer that is only valid for the duration of
//! the call; the Swift layer copies it immediately.

use crate::{cloud, handshake, library, sdp, session_store, wire, xbox};
use serde_json::{Value, json};
use std::ffi::{CStr, CString, c_char};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

pub type Emit = extern "C" fn(*const c_char);

struct Active {
    streaming: xbox::Streaming,
    path: String,
    cancel: Arc<AtomicBool>,
}

static ACCOUNT: OnceLock<Mutex<Option<Arc<xbox::Xbox>>>> = OnceLock::new();
static ACTIVE: OnceLock<Mutex<Option<Active>>> = OnceLock::new();
static SIGN_IN: OnceLock<Mutex<Option<Arc<AtomicBool>>>> = OnceLock::new();
static STREAM_CANCEL: OnceLock<Mutex<Option<Arc<AtomicBool>>>> = OnceLock::new();

fn account_slot() -> &'static Mutex<Option<Arc<xbox::Xbox>>> {
    ACCOUNT.get_or_init(|| Mutex::new(None))
}

fn active_slot() -> &'static Mutex<Option<Active>> {
    ACTIVE.get_or_init(|| Mutex::new(None))
}

fn sign_in_slot() -> &'static Mutex<Option<Arc<AtomicBool>>> {
    SIGN_IN.get_or_init(|| Mutex::new(None))
}

fn stream_cancel_slot() -> &'static Mutex<Option<Arc<AtomicBool>>> {
    STREAM_CANCEL.get_or_init(|| Mutex::new(None))
}

pub(crate) fn lock<T>(slot: &'static Mutex<T>) -> std::sync::MutexGuard<'static, T> {
    slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(crate) fn emit_event(callback: Emit, value: Value) {
    if let Ok(text) = CString::new(value.to_string()) {
        callback(text.as_ptr());
    }
}

pub(crate) fn emit_error(callback: Emit, message: impl AsRef<str>) {
    emit_event(callback, json!({ "event": "error", "message": message.as_ref() }));
}

fn json_pointer(value: Value) -> *mut c_char {
    CString::new(value.to_string()).unwrap_or_default().into_raw()
}

pub(crate) unsafe fn read_string(pointer: *const c_char) -> Option<String> {
    if pointer.is_null() {
        return None;
    }
    unsafe { CStr::from_ptr(pointer) }.to_str().ok().map(str::to_owned)
}

fn resolution(height: u32) -> xbox::Resolution {
    xbox::Resolution::CHOICES
        .iter()
        .map(|(choice, _)| *choice)
        .find(|choice| choice.dimensions().1 == height)
        .unwrap_or_default()
}

fn current_account() -> Option<Arc<xbox::Xbox>> {
    lock(account_slot()).clone()
}

fn active_stream() -> Option<(xbox::Streaming, String)> {
    lock(active_slot()).as_ref().map(|active| (active.streaming.clone(), active.path.clone()))
}

fn console_json(console: &xbox::Console) -> Value {
    json!({ "id": console.id, "name": console.name, "model": console.model, "power": console.power })
}

/// The catalog in letter order, with what a tile shows while its cover is
/// missing.
fn games_json(games: &[cloud::Game]) -> Value {
    let mut games: Vec<&cloud::Game> = games.iter().collect();
    games.sort_by_cached_key(|game| library::library_order(&game.name));
    Value::Array(games.into_iter().map(|game| json!({
        "id": game.id,
        "name": game.name,
        "cover": game.cover,
        "letter": library::initial(&game.name).to_string(),
        "monogram": library::monogram(&game.name),
        "hue": library::hue(&game.name),
    })).collect())
}

/// The positions in `names_json` (a JSON array of names) that match
/// `query`, best first; every position for an empty query. Release the
/// result with `pw_free_string`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pw_library_search(names_json: *const c_char, query: *const c_char) -> *mut c_char {
    let names: Vec<String> = unsafe { read_string(names_json) }
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default();
    let query = unsafe { read_string(query) }.unwrap_or_default();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    json_pointer(json!(library::library_view(&names, &query)))
}

// MARK: - Lifecycle

#[unsafe(no_mangle)]
pub extern "C" fn pw_core_version() -> *const c_char {
    c"0.1.0".as_ptr()
}

/// Point the reused desktop session/cache code at the iOS Documents directory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pw_init(session_path: *const c_char) -> i32 {
    let Some(path) = (unsafe { read_string(session_path) }) else { return -1 };
    unsafe { std::env::set_var("ALLY_SESSION_FILE", path) };
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn pw_free_string(pointer: *mut c_char) {
    if !pointer.is_null() {
        drop(unsafe { CString::from_raw(pointer) });
    }
}

// MARK: - Account

/// Restores a saved refresh token and emits `account` or `signedOut`.
#[unsafe(no_mangle)]
pub extern "C" fn pw_restore(callback: Emit) {
    thread::spawn(move || match xbox::restore_sign_in() {
        Ok(Some(account)) => {
            *lock(account_slot()) = Some(Arc::new(account));
            emit_event(callback, json!({ "event": "account" }));
        }
        Ok(None) => emit_event(callback, json!({ "event": "signedOut" })),
        Err(message) => emit_error(callback, message),
    });
}

/// Starts the Microsoft device-code flow. Emits `device`, then `account` and
/// `done`, or `error` and `done`.
#[unsafe(no_mangle)]
pub extern "C" fn pw_sign_in(callback: Emit) {
    let mut slot = lock(sign_in_slot());
    if slot.is_some() {
        drop(slot);
        emit_error(callback, "Sign-in already in progress");
        return;
    }
    let cancel = Arc::new(AtomicBool::new(false));
    *slot = Some(cancel.clone());
    drop(slot);

    thread::spawn(move || {
        let result = (|| {
            let client = xbox::client()?;
            let client_id = xbox::public_client_id();
            let device = xbox::begin_sign_in(&client, &client_id)?;
            let (url, prefilled) = device.verification_link();
            let code = device.user_code.clone();
            emit_event(callback, json!({
                "event": "device", "url": url.to_string(), "code": code, "prefilled": prefilled,
            }));
            let account = xbox::finish_sign_in(client, client_id, device, &cancel)?;
            *lock(account_slot()) = Some(Arc::new(account));
            Ok::<(), String>(())
        })();
        *lock(sign_in_slot()) = None;
        match result {
            Ok(()) => {
                emit_event(callback, json!({ "event": "account" }));
                emit_event(callback, json!({ "event": "done" }));
            }
            Err(message) => {
                emit_error(callback, message);
                emit_event(callback, json!({ "event": "done" }));
            }
        }
    });
}

#[unsafe(no_mangle)]
pub extern "C" fn pw_cancel_sign_in() {
    if let Some(cancel) = lock(sign_in_slot()).as_ref() {
        cancel.store(true, Ordering::Relaxed);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn pw_sign_out(callback: Emit) {
    if let Some(cancel) = lock(sign_in_slot()).as_ref() {
        cancel.store(true, Ordering::Relaxed);
    }
    if let Some(cancel) = lock(stream_cancel_slot()).as_ref() {
        cancel.store(true, Ordering::Relaxed);
    }
    pw_stop();
    *lock(account_slot()) = None;
    match session_store::clear() {
        Ok(()) => emit_event(callback, json!({ "event": "signedOut" })),
        Err(error) => emit_error(callback, format!("Could not sign out: {error}")),
    }
}

// MARK: - Catalogs

#[unsafe(no_mangle)]
pub extern "C" fn pw_consoles(callback: Emit) {
    let Some(account) = current_account() else {
        emit_error(callback, "Not signed in");
        return;
    };
    thread::spawn(move || match account.consoles() {
        Ok(consoles) => {
            let list: Vec<_> = consoles.iter().map(console_json).collect();
            emit_event(callback, json!({ "event": "consoles", "consoles": list }));
            emit_event(callback, json!({ "event": "done" }));
        }
        Err(message) => {
            emit_error(callback, message);
            emit_event(callback, json!({ "event": "done" }));
        }
    });
}

#[unsafe(no_mangle)]
pub extern "C" fn pw_cloud_catalog(callback: Emit) {
    let Some(account) = current_account() else {
        emit_error(callback, "Not signed in");
        return;
    };
    thread::spawn(move || {
        if let Some(games) = cloud::cached_catalog(&account) {
            emit_event(callback, json!({ "event": "cloud", "cached": true, "games": games_json(&games) }));
        }
        match cloud::catalog(&account) {
            Ok(games) => emit_event(callback, json!({ "event": "cloud", "cached": false, "games": games_json(&games) })),
            Err(message) => emit_error(callback, message),
        }
        emit_event(callback, json!({ "event": "done" }));
    });
}

// MARK: - Streaming

fn start_stream(callback: Emit, mut prepare: impl FnMut(Emit, Arc<AtomicBool>) -> Result<(xbox::Streaming, String, xbox::SessionConfig), String> + Send + 'static) {
    if lock(active_slot()).is_some() {
        emit_error(callback, "A stream is already active");
        return;
    }
    let cancel = Arc::new(AtomicBool::new(false));
    *lock(stream_cancel_slot()) = Some(cancel.clone());
    thread::spawn(move || {
        let result = prepare(callback, cancel.clone());
        match result {
            Ok((streaming, path, config)) => {
                emit_event(callback, json!({
                    "event": "ready",
                    "stun": config.stun,
                    "heartbeatMs": config.heartbeat.as_millis() as u64,
                }));
                *lock(active_slot()) = Some(Active { streaming: streaming.clone(), path: path.clone(), cancel: cancel.clone() });
                let heartbeat = config.heartbeat;
                thread::spawn(move || {
                    while !cancel.load(Ordering::Relaxed) {
                        let mut waited = Duration::ZERO;
                        while waited < heartbeat && !cancel.load(Ordering::Relaxed) {
                            thread::sleep(Duration::from_millis(250));
                            waited += Duration::from_millis(250);
                        }
                        if cancel.load(Ordering::Relaxed) { break; }
                        if streaming.keepalive(&path).is_err() { break; }
                    }
                });
                emit_event(callback, json!({ "event": "done" }));
            }
            Err(message) => {
                *lock(stream_cancel_slot()) = None;
                emit_error(callback, message);
                emit_event(callback, json!({ "event": "done" }));
            }
        }
    });
}

/// Reserve and provision by index into the console list.
#[unsafe(no_mangle)]
pub extern "C" fn pw_stream_home(callback: Emit, console_index: usize, height: u32) {
    let Some(account) = current_account() else {
        emit_error(callback, "Not signed in");
        return;
    };
    start_stream(callback, move |emit, cancel| {
        let stream = account.home()?;
        let consoles = stream.consoles()?;
        let console = consoles.get(console_index).ok_or("Console not found")?;
        emit_event(emit, json!({ "event": "status", "message": "Starting Remote Play…" }));
        let path = stream.reserve(console, resolution(height))?;
        emit_event(emit, json!({ "event": "status", "message": "Waiting for the console…" }));
        let mut stream = stream;
        let config = stream.provision(&path, &cancel, |message| {
            emit_event(emit, json!({ "event": "status", "message": message }));
        })?;
        Ok((stream, path, config))
    });
}

/// Reserve and provision the cloud game with this ID.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pw_stream_cloud(callback: Emit, game_id: *const c_char, height: u32) {
    let Some(account) = current_account() else {
        emit_error(callback, "Not signed in");
        return;
    };
    let Some(game_id) = (unsafe { read_string(game_id) }) else {
        emit_error(callback, "Game not found");
        return;
    };
    start_stream(callback, move |emit, cancel| {
        emit_event(emit, json!({ "event": "status", "message": "Loading Xbox Cloud catalog…" }));
        let games = cloud::catalog(&account)?;
        let game = games.iter().find(|game| game.id == game_id).ok_or("Game not found")?;
        let (stream, _market) = cloud::connect(&account)?;
        emit_event(emit, json!({ "event": "status", "message": "Starting the game…" }));
        let path = cloud::reserve(&stream, game, resolution(height))?;
        let mut stream = stream;
        let config = stream.provision(&path, &cancel, |message| {
            emit_event(emit, json!({ "event": "status", "message": message }));
        })?;
        Ok((stream, path, config))
    });
}

#[unsafe(no_mangle)]
pub extern "C" fn pw_stop() {
    if let Some(cancel) = lock(stream_cancel_slot()).take() {
        cancel.store(true, Ordering::Relaxed);
    }
    if let Some(active) = lock(active_slot()).take() {
        active.cancel.store(true, Ordering::Relaxed);
        active.streaming.stop(&active.path);
    }
}

// MARK: - SDP / ICE

fn result_string(result: Result<Value, String>) -> *mut c_char {
    json_pointer(match result {
        Ok(value) => json!({ "ok": true, "value": value }),
        Err(message) => json!({ "ok": false, "error": message }),
    })
}

/// Exchange the local offer for the console's answer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pw_sdp(offer: *const c_char) -> *mut c_char {
    let Some(offer) = (unsafe { read_string(offer) }) else {
        return result_string(Err("Invalid SDP".into()));
    };
    let Some((stream, path)) = active_stream() else {
        return result_string(Err("No active stream".into()));
    };
    match stream.sdp(&path, &offer) {
        Ok(answer) => json_pointer(json!({ "ok": true, "sdp": answer })),
        Err(message) => result_string(Err(message)),
    }
}

/// Send local candidates (`[{candidate,sdpMid,sdpMLineIndex}]`) and return any
/// remote candidates the console produced.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pw_ice(candidates: *const c_char) -> *mut c_char {
    let Some(text) = (unsafe { read_string(candidates) }) else {
        return result_string(Err("Invalid ICE payload".into()));
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return result_string(Err("Invalid ICE JSON".into()));
    };
    let Some((stream, path)) = active_stream() else {
        return result_string(Err("No active stream".into()));
    };
    match stream.ice(&path, value) {
        Ok(reply) => json_pointer(json!({ "ok": true, "candidates": reply })),
        Err(message) => result_string(Err(message)),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn pw_keepalive() {
    if let Some((stream, path)) = active_stream() {
        let _ = stream.keepalive(&path);
    }
}

// MARK: - Wire helpers

/// `b=AS`/abs-capture-time munging. Returns `{"sdp": "...", "captureExtension": n}`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pw_munge_offer(text: *const c_char, bitrate: u32, capture: bool) -> *mut c_char {
    let Some(text) = (unsafe { read_string(text) }) else { return std::ptr::null_mut() };
    let (sdp, extension) = sdp::munge_offer(&text, bitrate, capture);
    json_pointer(json!({ "sdp": sdp, "captureExtension": extension }))
}

/// Handshake messages as `[{"label": "control", "json": "..."}]`.
#[unsafe(no_mangle)]
pub extern "C" fn pw_handshake(width: u32, height: u32) -> *mut c_char {
    let messages: Vec<_> = handshake::messages(width, height)
        .into_iter()
        .map(|(label, value)| json!({ "label": label, "json": value.to_string() }))
        .collect();
    json_pointer(json!({
        "open": handshake::open_message().to_string(),
        "messages": messages,
    }))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pw_is_handshake_ack(text: *const c_char) -> i32 {
    match unsafe { read_string(text) } {
        Some(text) if handshake::is_ack(&text) => 1,
        _ => 0,
    }
}

/// Write the 38-byte input packet into `out`. Returns the required length.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pw_input_packet(
    sequence: u32, time: f64, buttons: u16,
    lx: i16, ly: i16, rx: i16, ry: i16, lt: u16, rt: u16,
    out: *mut u8, capacity: usize,
) -> usize {
    let packet = wire::input_packet(sequence, time, buttons, [lx, ly, rx, ry], lt, rt);
    if out.is_null() || capacity < packet.len() {
        return packet.len();
    }
    unsafe { std::ptr::copy_nonoverlapping(packet.as_ptr(), out, packet.len()) };
    packet.len()
}

/// Write the 15-byte channel-open packet into `out`. Returns the required length.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pw_connect_packet(start_time: f64, out: *mut u8, capacity: usize) -> usize {
    let packet = wire::connect_packet(start_time);
    if out.is_null() || capacity < packet.len() {
        return packet.len();
    }
    unsafe { std::ptr::copy_nonoverlapping(packet.as_ptr(), out, packet.len()) };
    packet.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn game(name: &str) -> cloud::Game {
        cloud::Game { id: format!("id-{name}"), name: name.into(), cover: None }
    }

    #[test]
    fn the_catalog_goes_out_in_letter_order_with_its_placeholder() {
        let mut zork = game("Zork");
        zork.cover = Some("https://store-images.s-microsoft.com/zork".into());
        // A quoted title sorts last by its characters, but is filed under its letter.
        let games = games_json(&[zork, game("“Quake”"), game("7 Days to Die"), game("apex"), game("No Man's Sky")]);
        let names: Vec<&str> = games.as_array().unwrap().iter().map(|game| game["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["7 Days to Die", "apex", "No Man's Sky", "“Quake”", "Zork"]);
        assert_eq!(games[4]["id"], "id-Zork");
        assert_eq!(games[4]["cover"], "https://store-images.s-microsoft.com/zork");
        assert!(games[3]["cover"].is_null());
        assert_eq!(games[0]["letter"], "#");
        assert_eq!(games[2]["letter"], "N");
        assert_eq!(games[2]["monogram"], "NM");
        assert_eq!(games[2]["hue"], library::hue("No Man's Sky"));
    }

    #[test]
    fn the_search_answers_positions_best_first() {
        let names = c"[\"Balatro\",\"No Man's Sky\",\"Diablo IV\",\"HxAxLxO\",\"Halo\"]";
        let search = |query: &std::ffi::CStr| {
            let answer = unsafe { pw_library_search(names.as_ptr(), query.as_ptr()) };
            let text = unsafe { CStr::from_ptr(answer) }.to_str().unwrap().to_owned();
            pw_free_string(answer);
            text
        };
        assert_eq!(search(c"nms"), "[1]");
        // Both match; the closer match comes first, whatever the input order.
        assert_eq!(search(c"halo"), "[4,3]");
        assert_eq!(search(c""), "[0,1,2,3,4]");
        assert_eq!(search(c"zzz"), "[]");
    }
}
