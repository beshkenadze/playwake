//! The PC half of the C ABI: the PCs with Sunshine and their apps, pairing,
//! and a stream whose frames and sound go to the player's callbacks. The
//! core does the work; Swift decodes and shows.

use crate::bridge::{Emit, emit_event, lock, read_string};
use crate::input_route::PwPad;
use playwake_core::{covers, gamestream, library, nal, pc, route::Routed};
use serde_json::{Value, json};
use std::os::raw::c_char;
use std::sync::{Arc, Mutex};

/// One encoded frame: `data` holds length-prefixed NAL units; a key frame
/// also brings its parameter sets (`count` of them). Valid for the call only.
pub type VideoSink = extern "C" fn(data: *const u8, length: usize, sets: *const *const u8, set_lengths: *const usize,
    count: usize, hevc: bool, key: bool);
/// One Opus packet, 48 kHz stereo. Valid for the call only.
pub type AudioSink = extern "C" fn(data: *const u8, length: usize);

static CATALOG: Mutex<Option<pc::Catalog>> = Mutex::new(None);
static STREAM: Mutex<Option<Arc<gamestream::Stream>>> = Mutex::new(None);

/// A PC failure: shown, but it does not end a stream the way `error` does.
fn emit_error(callback: Emit, message: impl AsRef<str>) {
    emit_event(callback, json!({ "event": "pcError", "message": message.as_ref() }));
}

fn computer_json(computer: &pc::Computer) -> Value {
    match computer {
        pc::Computer::Paired(pc) => json!({ "id": pc.id(), "name": pc.name, "state": "paired" }),
        pc::Computer::Offline(pc) => json!({ "id": pc.id(), "name": pc.name, "state": "offline" }),
        pc::Computer::Unpaired(found) => json!({ "id": found.address, "name": found.name, "address": found.address,
            "state": "unpaired" }),
    }
}

fn games_json(games: &[pc::Game]) -> Value {
    let mut games: Vec<&pc::Game> = games.iter().collect();
    games.sort_by_cached_key(|game| library::library_order(&game.name));
    Value::Array(games.into_iter().map(|game| json!({
        "id": game.cover(),
        "name": game.name,
        "pc": game.pc_id(),
        "running": game.running,
        "cover": covers::file(&pc::COVERS, &game.cover()).map(|path| path.to_string_lossy().into_owned()),
        "letter": library::initial(&game.name).to_string(),
        "monogram": library::monogram(&game.name),
        "hue": library::hue(&game.name),
    })).collect())
}

fn catalog_json(catalog: &pc::Catalog) -> Value {
    json!({ "event": "pcs", "pcs": catalog.pcs.iter().map(computer_json).collect::<Vec<_>>(),
        "games": games_json(&catalog.games) })
}

/// Asks the network and the saved PCs; emits `pcs`, then `pcs` again as
/// covers arrive.
#[unsafe(no_mangle)]
pub extern "C" fn pw_pc_catalog(callback: Emit) {
    std::thread::spawn(move || refresh(callback));
}

/// Remembers a PC the network search does not find, once Sunshine answers
/// at `address`; then emits `pcs` as `pw_pc_catalog` does.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pw_pc_add(callback: Emit, address: *const c_char) {
    let address = unsafe { read_string(address) }.unwrap_or_default();
    std::thread::spawn(move || match pc::add(address.trim()) {
        Ok(()) => refresh(callback),
        Err(error) => emit_error(callback, error),
    });
}

fn refresh(callback: Emit) {
    match pc::catalog() {
        Ok(catalog) => {
            emit_event(callback, catalog_json(&catalog));
            let games = catalog.games.clone();
            *lock(&CATALOG) = Some(catalog);
            pc::fetch_covers(&games, || {
                if let Some(catalog) = lock(&CATALOG).as_ref() { emit_event(callback, catalog_json(catalog)); }
            });
        }
        Err(error) => emit_error(callback, error),
    }
}

/// Pairs with the unpaired PC at `address`: emits `pcPin` with the PIN to
/// enter in Sunshine, then `pcPaired` or `error`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pw_pc_pair(callback: Emit, address: *const c_char) {
    let address = unsafe { read_string(address) }.unwrap_or_default();
    let found = lock(&CATALOG).as_ref().and_then(|catalog| catalog.pcs.iter().find_map(|computer| match computer {
        pc::Computer::Unpaired(found) if found.address == address => Some(found.clone()),
        _ => None,
    }));
    let Some(found) = found else { return emit_error(callback, "No unpaired PC at that address") };
    let pin = gamestream::pin();
    emit_event(callback, json!({ "event": "pcPin", "pin": pin, "name": found.name }));
    std::thread::spawn(move || match pc::pair(&found, &pin) {
        Ok(()) => emit_event(callback, json!({ "event": "pcPaired", "name": found.name })),
        Err(error) => emit_error(callback, error),
    });
}

/// Streams the game with this ID (`id` from `pcs`) at `width`×`height` and
/// `fps`. Emits `status`, `pcStreaming`, `pcHdr` and `pcEnded`; frames and
/// sound go to `video` and `audio` from the stream's own thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pw_pc_stream(callback: Emit, game: *const c_char, width: u32, height: u32, fps: u32,
    bitrate_kbps: u32, hdr: bool, video: VideoSink, audio: AudioSink)
{
    let id = unsafe { read_string(game) }.unwrap_or_default();
    let game = lock(&CATALOG).as_ref().and_then(|catalog| catalog.games.iter().find(|game| game.cover() == id).cloned());
    let Some(game) = game else { return emit_error(callback, "Game not found") };
    if lock(&STREAM).is_some() { return emit_error(callback, "A stream is already active") }
    std::thread::spawn(move || {
        emit_event(callback, json!({ "event": "status", "message": format!("Starting {}…", game.name) }));
        let request = game.start(pc::Panel { width, height, fps }, hdr);
        let (codec, hdr) = pc::video_format(None, request.hdr);
        let (stream, events) = match pc::connect(&request, width, height, bitrate_kbps, codec, hdr, false) {
            Ok(started) => started,
            Err(error) => return emit_event(callback, json!({ "event": "pcEnded", "error": error })),
        };
        let stream = Arc::new(stream);
        *lock(&STREAM) = Some(stream.clone());
        emit_event(callback, json!({ "event": "pcStreaming", "hevc": codec == gamestream::Codec::Hevc, "hdr": hdr }));
        let mut ended = Ok(());
        for event in events {
            match event {
                gamestream::Event::Video(frame) => {
                    let unit = nal::access_unit(&frame.data, codec);
                    let sets: Vec<*const u8> = unit.parameter_sets.iter().map(|set| set.as_ptr()).collect();
                    let lengths: Vec<usize> = unit.parameter_sets.iter().map(Vec::len).collect();
                    video(unit.data.as_ptr(), unit.data.len(), sets.as_ptr(), lengths.as_ptr(), sets.len(),
                        codec == gamestream::Codec::Hevc, frame.key);
                }
                gamestream::Event::Audio(packet) => audio(packet.as_ptr(), packet.len()),
                gamestream::Event::Hdr(display) => emit_event(callback, json!({ "event": "pcHdr", "hdr": display.is_some() })),
                gamestream::Event::Rumble(..) => {}
                gamestream::Event::Ended(result) => { ended = result; break }
            }
        }
        lock(&STREAM).take();
        emit_event(callback, json!({ "event": "pcEnded", "error": ended.err() }));
    });
}

/// Sends a routed pad state (see `pw_route_input`) to the PC stream.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pw_pc_gamepad(pad: *const PwPad) {
    let Some(pad) = (unsafe { pad.as_ref() }) else { return };
    let routed = Routed { axes: pad.axes, buttons: pad.buttons, lt: pad.lt, rt: pad.rt };
    if let Some(stream) = lock(&STREAM).as_ref() { stream.gamepad(&pc::gamepad(&routed)); }
}

/// Ends the PC stream; `pcEnded` follows.
#[unsafe(no_mangle)]
pub extern "C" fn pw_pc_stop() {
    if let Some(stream) = lock(&STREAM).as_ref() { stream.stop(); }
}
