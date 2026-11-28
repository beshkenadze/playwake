//! The PC half of the C ABI: the PCs with Sunshine and their apps, pairing,
//! and a stream whose frames and sound go to the player's callbacks. The
//! core does the work; Swift decodes and shows.

use crate::bridge::{Emit, emit_event, lock, read_string};
use crate::input_route::PwPad;
use playwake_core::{covers, gamestream, library, nal, pc, route::Routed};
use serde_json::{Value, json};
use std::os::raw::c_char;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// One encoded frame: `data` holds length-prefixed NAL units; a key frame
/// also brings its parameter sets (`count` of them). Valid for the call only.
pub type VideoSink = extern "C" fn(data: *const u8, length: usize, sets: *const *const u8, set_lengths: *const usize,
    count: usize, hevc: bool, key: bool);
/// One Opus packet, 48 kHz stereo. Valid for the call only.
pub type AudioSink = extern "C" fn(data: *const u8, length: usize);

static CATALOG: Mutex<Option<pc::Catalog>> = Mutex::new(None);
static SLOT: Mutex<Slot> = Mutex::new(Slot { session: 0, active: false, stopping: false, stream: None });
/// How long `pw_pc_stop` waits for the stream's thread to finish.
const STOP_WAIT: Duration = Duration::from_secs(3);

/// The one PC stream. It is taken before the PC is asked to launch, so a
/// stop during the launch still ends it and a second start is refused; each
/// stream publishes and clears only its own session.
struct Slot {
    session: u64,
    active: bool,
    stopping: bool,
    stream: Option<Arc<gamestream::Stream>>,
}

impl Slot {
    /// A new session, unless a stream is still running or stopping.
    fn begin(&mut self) -> Option<u64> {
        if self.active { return None }
        self.session += 1;
        *self = Slot { session: self.session, active: true, stopping: false, stream: None };
        Some(self.session)
    }

    /// Hands the connected stream to the slot; false when the session was
    /// stopped meanwhile, and the caller ends the stream itself.
    fn publish(&mut self, session: u64, stream: &Arc<gamestream::Stream>) -> bool {
        let live = self.session == session && self.active && !self.stopping;
        if live { self.stream = Some(stream.clone()); }
        live
    }

    fn finish(&mut self, session: u64) {
        if self.session == session { *self = Slot { session, active: false, stopping: false, stream: None }; }
    }

    /// Marks the session stopping; the stream to end, if it is connected.
    fn stop(&mut self) -> Option<Arc<gamestream::Stream>> {
        if self.active { self.stopping = true; }
        self.stream.clone()
    }
}

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

/// Whether a PC is paired, from the saved pairings: no network, so it can
/// choose the first screen.
#[unsafe(no_mangle)]
pub extern "C" fn pw_pc_has_paired() -> bool {
    pc::has_paired()
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
            // Installed before it is announced, so a start from the event finds its game.
            let games = catalog.games.clone();
            let announced = catalog_json(&catalog);
            *lock(&CATALOG) = Some(catalog);
            emit_event(callback, announced);
            pc::fetch_covers(&games, || {
                let announced = lock(&CATALOG).as_ref().map(catalog_json);
                if let Some(announced) = announced { emit_event(callback, announced); }
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
/// `fps`. Returns the stream's session, or 0 when it cannot start (then
/// `pcError` says why). Emits `status`, `pcStreaming`, `pcHdr` and `pcEnded`,
/// each with its `session`; frames and sound go to `video` and `audio` from
/// the stream's own thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pw_pc_stream(callback: Emit, game: *const c_char, width: u32, height: u32, fps: u32,
    bitrate_kbps: u32, hdr: bool, video: VideoSink, audio: AudioSink) -> u64
{
    let id = unsafe { read_string(game) }.unwrap_or_default();
    let game = lock(&CATALOG).as_ref().and_then(|catalog| catalog.games.iter().find(|game| game.cover() == id).cloned());
    let Some(game) = game else { emit_error(callback, "Game not found"); return 0 };
    let Some(session) = lock(&SLOT).begin() else {
        emit_error(callback, "The last stream is still stopping");
        return 0;
    };
    std::thread::spawn(move || {
        emit_event(callback, json!({ "event": "status", "message": format!("Starting {}…", game.name) }));
        let request = game.start(pc::Panel { width, height, fps }, hdr);
        let (codec, hdr) = pc::video_format(None, request.hdr);
        let ended = match pc::connect(&request, width, height, bitrate_kbps, codec, hdr, false) {
            Err(error) => Err(error),
            Ok((stream, events)) => {
                let stream = Arc::new(stream);
                if lock(&SLOT).publish(session, &stream) {
                    emit_event(callback, json!({ "event": "pcStreaming", "session": session,
                        "hevc": codec == gamestream::Codec::Hevc, "hdr": hdr }));
                    play(callback, session, codec, events, video, audio)
                } else {
                    // Stopped while the PC was launching the app.
                    stream.stop();
                    Ok(())
                }
            }
        };
        lock(&SLOT).finish(session);
        emit_event(callback, json!({ "event": "pcEnded", "session": session, "error": ended.err() }));
    });
    session
}

/// Hands the stream's frames and sound to the player until it ends.
fn play(callback: Emit, session: u64, codec: gamestream::Codec, events: std::sync::mpsc::Receiver<gamestream::Event>,
    video: VideoSink, audio: AudioSink) -> pc::Result<()>
{
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
            gamestream::Event::Hdr(display) =>
                emit_event(callback, json!({ "event": "pcHdr", "session": session, "hdr": display.is_some() })),
            gamestream::Event::Rumble(..) => {}
            gamestream::Event::Ended(result) => return result,
        }
    }
    Ok(())
}

/// The connected PC stream, if any, with its session.
pub(crate) fn stream() -> Option<(u64, Arc<gamestream::Stream>)> {
    let slot = lock(&SLOT);
    slot.stream.clone().map(|stream| (slot.session, stream))
}

/// Sends a routed pad state (see `pw_route_input`) to the PC stream.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pw_pc_gamepad(pad: *const PwPad) {
    let Some(pad) = (unsafe { pad.as_ref() }) else { return };
    let routed = Routed { axes: pad.axes, buttons: pad.buttons, lt: pad.lt, rt: pad.rt };
    if let Some((_, stream)) = stream() { stream.gamepad(&pc::gamepad(&routed)); }
}

/// Ends the PC stream, also one still launching, and waits up to 3 s for its
/// thread to finish: after it returns, no frame or sound of it arrives.
/// `pcEnded` follows.
#[unsafe(no_mangle)]
pub extern "C" fn pw_pc_stop() {
    let stream = lock(&SLOT).stop();
    if let Some(stream) = stream { stream.stop(); }
    let started = Instant::now();
    while lock(&SLOT).active && started.elapsed() < STOP_WAIT {
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(test)]
mod tests {
    use super::Slot;

    fn slot() -> Slot { Slot { session: 0, active: false, stopping: false, stream: None } }

    #[test]
    fn one_stream_at_a_time_until_it_finishes() {
        let mut slot = slot();
        let first = slot.begin().unwrap();
        assert_eq!(slot.begin(), None);
        slot.finish(first);
        assert_eq!(slot.begin(), Some(first + 1));
    }

    #[test]
    fn a_stop_during_the_launch_refuses_the_stream_that_connects_later() {
        let mut slot = slot();
        let session = slot.begin().unwrap();
        assert!(slot.stop().is_none());
        // The slot stays taken until the launching thread finishes.
        assert_eq!(slot.begin(), None);
        assert!(slot.active && slot.stopping);
        slot.finish(session);
        assert!(!slot.active);
    }

    #[test]
    fn an_old_session_cannot_finish_or_publish_into_a_new_one() {
        let mut slot = slot();
        let old = slot.begin().unwrap();
        slot.finish(old);
        let new = slot.begin().unwrap();
        slot.finish(old);
        assert!(slot.active, "the old session ended the new one");
        assert_eq!(slot.session, new);
    }
}
