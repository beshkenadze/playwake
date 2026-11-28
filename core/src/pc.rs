//! PC game streaming from Sunshine hosts with Playwake's own client: finding
//! and pairing PCs, and listing what they can stream.

use crate::{device::Device, session_store};
use gamestream::{Codec, Config, Event, Host, Identity, Launch, Stream};
use serde::{Deserialize, Serialize};
use std::{sync::{Mutex, MutexGuard, mpsc}, thread, time::Duration};

pub type Result<T> = gamestream::Result<T>;

const DISCOVERY: Duration = Duration::from_millis(1500);
/// The directory of the covers in the cover cache.
pub static COVERS: crate::covers::Set = crate::covers::Set::new("pc");

/// What Playwake keeps between runs: its identity and the PCs it paired with.
/// The file holds the private key and is readable by the owner only.
#[derive(Default, Serialize, Deserialize)]
struct Saved {
    key: String,
    certificate: String,
    #[serde(default)]
    hosts: Vec<Pc>,
    /// Addresses of PCs that the search does not reach, such as a PC on
    /// another network.
    #[serde(default)]
    addresses: Vec<String>,
}

impl Saved {
    /// Every address to ask, once: the paired PCs, the given addresses, then
    /// what the search `found`.
    fn asked(&self, found: impl IntoIterator<Item = String>) -> Vec<String> {
        let mut asked = Vec::new();
        for address in self.hosts.iter().map(|pc| pc.address.clone()).chain(self.addresses.iter().cloned()).chain(found) {
            if !asked.contains(&address) { asked.push(address); }
        }
        asked
    }

    fn add_address(&mut self, address: &str) {
        if !self.addresses.iter().any(|known| known == address) { self.addresses.push(address.to_owned()); }
    }
}

/// A paired PC.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Pc {
    /// The PC's own ID. It stays the same when the address changes.
    id: String,
    pub name: String,
    address: String,
    /// PEM of the certificate the PC showed while pairing.
    certificate: String,
}

/// A PC on the network that Playwake is not paired with.
#[derive(Clone, Debug, PartialEq)]
pub struct Unpaired {
    id: String,
    pub name: String,
    pub address: String,
}

/// A PC as the PC tab lists it.
#[derive(Clone, Debug, PartialEq)]
pub enum Computer {
    Paired(Pc),
    Unpaired(Unpaired),
    /// A paired PC that did not answer: it is off or on another network.
    Offline(Pc),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Game {
    pub name: String,
    pc: Pc,
    app: u32,
    /// The PC runs this app now.
    pub running: bool,
    /// The PC can encode HDR.
    hdr: bool,
    /// The other app the user agreed to end if its PC still runs it.
    end_other: Option<u32>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Catalog {
    /// The apps of the paired PCs that answered.
    pub games: Vec<Game>,
    pub pcs: Vec<Computer>,
}

/// What choosing an app does, as in Moonlight's app grid.
#[derive(Debug, PartialEq)]
pub enum Choice {
    /// The PC runs it: offer to resume or quit it.
    Running(Game),
    /// Starting it ends the other app the PC runs: ask first.
    Replace(Game, Game),
    Start(Game),
}

/// What the stream is sized for. The PC sets its display from these.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Panel {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

impl Panel {
    pub fn of(device: &Device) -> Self {
        let (width, height, fps) = match device.name {
            "Steam Deck OLED" => (1280, 800, 90),
            "Steam Deck LCD" => (1280, 800, 60),
            "ROG Ally" | "ROG Ally X" => (1920, 1080, 120),
            "Legion Go" => (2560, 1600, 144),
            "Legion Go S" => (1920, 1200, 120),
            _ => (1920, 1080, 60),
        };
        Self { width, height, fps }
    }

    /// The stream `height` pixels high in this screen's shape, or the screen
    /// itself for 0. The width is a multiple of 8, which video encoders want.
    pub fn scaled_to(self, height: u32) -> Self {
        if height == 0 || height == self.height { return self; }
        let width = ((height as f64 * self.width as f64 / self.height as f64 / 8.0).round() as u32) * 8;
        Self { width, height, ..self }
    }

    /// LSFG shows two frames for each one sent, so the PC sends half the
    /// screen's rate; more would be thrown away before they are shown.
    pub fn for_lsfg(self, lsfg: bool) -> Self {
        if lsfg { Self { fps: self.fps / 2, ..self } } else { self }
    }
}

impl Pc {
    pub fn id(&self) -> &str {
        &self.id
    }
}

impl Game {
    /// The app's ID on its PC, which a refresh keeps.
    pub fn app_id(&self) -> u32 {
        self.app
    }

    pub fn pc(&self) -> &str {
        &self.pc.name
    }

    /// The ID of the game's PC.
    pub fn pc_id(&self) -> &str {
        &self.pc.id
    }

    /// The key of the game's cover in the cover cache.
    pub fn cover(&self) -> String {
        format!("gamestream://{}/{}", self.pc.id, self.app)
    }

    /// The same game, started even if that ends `other` on its PC.
    pub fn ending(self, other: &Game) -> Self {
        Self { end_other: Some(other.app), ..self }
    }

    /// The same game without that consent: it covers one start only.
    pub fn asking_again(self) -> Self {
        Self { end_other: None, ..self }
    }

    /// What a player needs to start this stream; with HDR if the user wants
    /// it and the PC can encode it.
    pub fn start(&self, panel: Panel, hdr: bool) -> StreamRequest {
        let saved = load();
        StreamRequest {
            address: self.pc.address.clone(), certificate: self.pc.certificate.clone(),
            client_key: saved.key, client_certificate: saved.certificate,
            app: self.app, fps: panel.fps, end_other: self.end_other, hdr: hdr && self.hdr,
            // Only a smaller stream is left to SteamOS to scale up.
            scaled: panel.height < Panel::of(crate::device::current()).height,
        }
    }
}

/// A paired Sunshine host and the app to stream from it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StreamRequest {
    pub address: String,
    /// PEM of the certificate the host showed while pairing.
    pub certificate: String,
    /// PEM of this client's key and certificate.
    pub client_key: String,
    pub client_certificate: String,
    pub app: u32,
    pub fps: u32,
    /// The other app the user agreed to end if the PC still runs it.
    #[serde(default)]
    pub end_other: Option<u32>,
    /// The user wants HDR and the PC can encode it.
    #[serde(default)]
    pub hdr: bool,
    /// The stream is smaller than the screen and SteamOS scales it up.
    #[serde(default)]
    pub scaled: bool,
}

/// The codec of a PC stream and whether it asks for HDR. HDR needs 10-bit
/// HEVC; an SDR stream is H.264. `forced` names the codec of an SDR stream
/// instead. `hdr`: the request wants HDR and the player can show it.
pub fn video_format(forced: Option<&str>, hdr: bool) -> (Codec, bool) {
    match forced {
        Some("hevc") => (Codec::Hevc, false),
        Some("h264") => (Codec::H264, false),
        _ if hdr => (Codec::Hevc, true),
        _ => (Codec::H264, false),
    }
}

/// Starts the request's app on its PC and the stream from it, in the codec
/// and HDR mode `video_format` chose. `full_range` where the player converts
/// the picture itself. The player decodes and shows what the events carry.
pub fn connect(request: &StreamRequest, width: u32, height: u32, bitrate_kbps: u32, codec: Codec, hdr: bool,
    full_range: bool) -> Result<(Stream, mpsc::Receiver<Event>)>
{
    let identity = Identity::from_pem(&request.client_key, &request.client_certificate)?;
    let mut host = Host::new(&request.address);
    host.certificate = Some(request.certificate.clone());
    let info = host.info(&identity)?;
    if !info.paired { return Err("This PC is no longer paired with Playwake".into()); }
    if hdr && !info.encodes_hdr() { return Err("This PC cannot encode HDR".into()); }
    let launch = Launch { app: request.app, width, height, fps: request.fps, hdr, host_audio: false,
        end_other: request.end_other };
    let session = host.launch(&identity, &info, &launch)?;
    let config = Config { width, height, fps: request.fps, bitrate_kbps, codec, hdr, full_range };
    Stream::start(&host, &session, &config)
}

/// A routed pad state in the PC protocol's buttons, ranges and stick signs.
pub fn gamepad(state: &crate::route::Routed) -> gamestream::Gamepad {
    use gamestream::*;
    const BUTTONS: [(u16, u32); 15] = [
        (2, BUTTON_GUIDE), (4, BUTTON_START), (8, BUTTON_BACK), (16, BUTTON_A), (32, BUTTON_B),
        (64, BUTTON_X), (128, BUTTON_Y), (256, BUTTON_UP), (512, BUTTON_DOWN), (1024, BUTTON_LEFT),
        (2048, BUTTON_RIGHT), (4096, BUTTON_LEFT_BUMPER), (8192, BUTTON_RIGHT_BUMPER),
        (16384, BUTTON_LEFT_STICK), (32768, BUTTON_RIGHT_STICK),
    ];
    let [left_x, left_y, right_x, right_y] = state.axes;
    Gamepad {
        buttons: BUTTONS.iter().filter(|(xbox, _)| state.buttons & xbox != 0).map(|(_, pc)| pc).sum(),
        left_trigger: (state.lt >> 8) as u8,
        right_trigger: (state.rt >> 8) as u8,
        // Routed sticks point down for positive Y, as evdev's do; the host expects up.
        left_stick: (left_x, left_y.saturating_neg()),
        right_stick: (right_x, right_y.saturating_neg()),
    }
}

impl Catalog {
    /// Another app that runs on the PC of `game`. Starting `game` would end it.
    fn running_instead_of(&self, game: &Game) -> Option<&Game> {
        self.games.iter().find(|other| other.running && other.pc.id == game.pc.id && other.app != game.app)
    }

    /// The paired PC with this ID, if it answered.
    pub fn paired(&self, id: &str) -> Option<&Pc> {
        self.pcs.iter().find_map(|computer| match computer {
            Computer::Paired(pc) if pc.id == id => Some(pc),
            _ => None,
        })
    }

    /// The apps of the PC with this ID.
    pub fn apps(&self, id: &str) -> Vec<&Game> {
        self.games.iter().filter(|game| game.pc.id == id).collect()
    }

    pub fn choose(&self, id: &str, index: usize) -> Option<Choice> {
        let game = (*self.apps(id).get(index)?).clone();
        Some(if game.running {
            Choice::Running(game)
        } else if let Some(running) = self.running_instead_of(&game) {
            Choice::Replace(game, running.clone())
        } else {
            Choice::Start(game)
        })
    }
}

impl Catalog {
    /// One paired PC that answered with these apps; its ID is its name. For
    /// the frontends' tests.
    #[doc(hidden)]
    pub fn one_pc(name: &str, apps: &[&str]) -> Self {
        let pc = Pc { id: name.into(), name: name.into(), address: String::new(), certificate: String::new() };
        let games = apps.iter().zip(1..).map(|(title, app)| Game { name: (*title).into(), pc: pc.clone(), app,
            running: false, hdr: false, end_other: None }).collect();
        Self { games, pcs: vec![Computer::Paired(pc)] }
    }
}

fn saved_path() -> Option<std::path::PathBuf> {
    Some(session_store::path().ok()?.with_extension("pc.json"))
}

fn load() -> Saved {
    parse(saved_path().and_then(|path| std::fs::read(path).ok()))
}

/// The saved PCs from the file's bytes; none from a missing or unreadable file.
fn parse(bytes: Option<Vec<u8>>) -> Saved {
    bytes.and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default()
}

/// Whether a PC is paired, from the saved pairings alone: no network.
pub fn has_paired() -> bool {
    !load().hosts.is_empty()
}

fn store(saved: &Saved) -> Result<()> {
    let path = saved_path().ok_or("No place to save the paired PCs")?;
    session_store::write_json(path, saved).map_err(|error| format!("Could not save the paired PCs: {error}"))
}

/// The client identity, made and saved on first use.
fn identity(saved: &mut Saved) -> Result<Identity> {
    if let Ok(identity) = Identity::from_pem(&saved.key, &saved.certificate) { return Ok(identity); }
    let identity = Identity::generate()?;
    saved.key = identity.key_pem()?;
    saved.certificate = identity.certificate_pem()?;
    saved.hosts.clear();
    store(saved)?;
    Ok(identity)
}

/// What one address answers: the paired PC there and its games, or the name
/// of a PC that is not paired. The PC is recognised by its ID, so a paired PC
/// is found again at a new address.
fn ask(address: &str, paired: &[Pc], identity: &Identity) -> Option<std::result::Result<(Pc, Vec<Game>), Unpaired>> {
    let mut host = Host::new(address);
    let info = host.info(identity).ok()?;
    let unpaired = Unpaired { id: info.unique_id.clone(), name: info.name.clone(), address: address.to_owned() };
    let Some(known) = paired.iter().find(|pc| pc.id == info.unique_id) else { return Some(Err(unpaired)) };
    host.certificate = Some(known.certificate.clone());
    // Only the paired answer says which app runs.
    let state = host.info(identity).ok()?;
    if !state.paired { return Some(Err(unpaired)); }
    let pc = Pc { name: info.name, address: address.to_owned(), ..known.clone() };
    let games = host.apps(identity).ok()?.into_iter()
        .map(|app| Game { name: app.title, pc: pc.clone(), running: app.id == state.current_app, app: app.id,
            hdr: state.encodes_hdr(), end_other: None })
        .collect();
    Some(Ok((pc, games)))
}

/// Takes the answers of all addresses into a catalog. A PC that answers at
/// several addresses counts once, as paired if one answer says so; a saved PC
/// that does not answer is offline.
fn collect(answers: Vec<std::result::Result<(Pc, Vec<Game>), Unpaired>>, saved: &[Pc]) -> Catalog {
    let mut catalog = Catalog::default();
    let mut seen = Vec::new();
    let (paired, unpaired): (Vec<_>, Vec<_>) = answers.into_iter().partition(std::result::Result::is_ok);
    for (pc, games) in paired.into_iter().flatten() {
        if seen.contains(&pc.id) { continue; }
        seen.push(pc.id.clone());
        catalog.games.extend(games);
        catalog.pcs.push(Computer::Paired(pc));
        // Apps in the order of their names, for the library and its letters.
        catalog.games.sort_by_cached_key(|game| crate::library::library_order(&game.name));
    }
    for pc in unpaired.into_iter().filter_map(std::result::Result::err) {
        if seen.contains(&pc.id) { continue; }
        seen.push(pc.id.clone());
        catalog.pcs.push(Computer::Unpaired(pc));
    }
    catalog.pcs.extend(saved.iter().filter(|pc| !seen.contains(&pc.id)).cloned().map(Computer::Offline));
    catalog
}

/// Brings the saved PCs to the name and address they answered from, and
/// keeps the rest, such as a pairing renewed meanwhile. Returns whether one
/// changed.
fn follow(saved: &mut [Pc], pcs: &[Computer]) -> bool {
    let mut changed = false;
    for pc in pcs {
        let Computer::Paired(pc) = pc else { continue };
        let moved = |known: &&mut Pc| known.id == pc.id && (known.name != pc.name || known.address != pc.address);
        if let Some(known) = saved.iter_mut().find(moved) {
            known.name = pc.name.clone();
            known.address = pc.address.clone();
            changed = true;
        }
    }
    changed
}

/// The paired PCs and their games, the unpaired PCs found on the network, and
/// the paired PCs that did not answer.
pub fn catalog() -> Result<Catalog> {
    let mut saved = load();
    let identity = identity(&mut saved)?;
    let addresses = saved.asked(gamestream::discover(DISCOVERY).into_iter().map(|found| found.to_string()));
    // A PC that is off answers only after a timeout, so ask all at once.
    let answers = thread::scope(|scope| {
        let asked: Vec<_> = addresses.iter()
            .map(|address| { let (paired, identity) = (&saved.hosts, &identity); scope.spawn(move || ask(address, paired, identity)) })
            .collect();
        asked.into_iter().filter_map(|answer| answer.join().ok().flatten()).collect()
    });
    let catalog = collect(answers, &saved.hosts);
    // A PC may have been paired or added while the PCs were asked.
    let _saving = saving();
    let mut current = load();
    if follow(&mut current.hosts, &catalog.pcs) { store(&current)?; }
    Ok(catalog)
}

/// Fetches the covers that the PCs keep for `games`.
pub fn fetch_covers(games: &[Game], progress: impl Fn()) {
    let saved = load();
    let Ok(identity) = Identity::from_pem(&saved.key, &saved.certificate) else { return };
    let keys: Vec<String> = games.iter().map(Game::cover).collect();
    let keys: Vec<&str> = keys.iter().map(String::as_str).collect();
    // Each PC is asked for its HTTPS port once; a PC that does not answer is skipped from then on.
    let mut hosts: Vec<(&str, Option<Host>)> = Vec::new();
    crate::covers::fetch(&COVERS, &keys, |key| {
        let game = games.iter().find(|game| game.cover() == key)?;
        let index = hosts.iter().position(|(id, _)| *id == game.pc.id).unwrap_or_else(|| {
            let mut host = Host::new(&game.pc.address);
            host.certificate = Some(game.pc.certificate.clone());
            let answered = host.info(&identity).is_ok_and(|info| info.paired);
            hosts.push((&game.pc.id, answered.then_some(host)));
            hosts.len() - 1
        });
        let asset = hosts[index].1.as_ref()?.asset(&identity, game.app);
        // A PC that stops answering would make every later cover wait out a timeout.
        if asset.is_err() { hosts[index].1 = None; }
        asset.ok()
    }, progress);
}

/// Pairs with a PC. Blocks until the user has entered `pin` in Sunshine.
pub fn pair(pc: &Unpaired, pin: &str) -> Result<()> {
    let identity = identity(&mut load())?;
    let mut host = Host::new(&pc.address);
    gamestream::pair(&mut host, &identity, pin)?;
    // The ID names the pairing it replaces, so it is read over the connection
    // that the new certificate protects, not from a plain answer.
    let confirmed = host.info(&identity)?;
    if !confirmed.paired { return Err("The PC did not confirm the pairing".into()); }
    let id = confirmed.unique_id;
    let certificate = host.certificate.ok_or("The PC sent no certificate")?;
    // Pairing takes as long as the user needs for the PIN, and two PCs can be
    // paired at once: read the saved PCs only now, and one writer at a time.
    let _saving = saving();
    let mut saved = load();
    remember(&mut saved.hosts, Pc { id, name: pc.name.clone(), address: pc.address.clone(), certificate });
    store(&saved)
}

fn saving() -> MutexGuard<'static, ()> {
    static SAVING: Mutex<()> = Mutex::new(());
    SAVING.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Remembers the address of a PC that the search does not find, once a PC
/// with Sunshine answers there.
pub fn add(address: &str) -> Result<()> {
    let identity = identity(&mut load())?;
    Host::new(address).info(&identity).map_err(|error| format!("No PC with Sunshine answered at {address}: {error}"))?;
    let _saving = saving();
    let mut saved = load();
    saved.add_address(address);
    store(&saved)
}

/// Ends `game` on its PC, and nothing else the PC may run by now.
pub fn quit(game: &Game) -> Result<()> {
    let identity = identity(&mut load())?;
    let mut host = Host::new(&game.pc.address);
    host.certificate = Some(game.pc.certificate.clone());
    let info = host.info(&identity)?;
    if !info.paired { return Err("This PC is not paired".into()); }
    if ends(info.current_app, game.app)? { host.cancel(&identity)?; }
    Ok(())
}

/// Whether quitting `app` ends what the PC runs: nothing to end when it runs
/// nothing, and another app is not the one the user agreed to quit.
fn ends(running: u32, app: u32) -> Result<bool> {
    match running {
        0 => Ok(false),
        running if running == app => Ok(true),
        _ => Err("The PC runs another app now".into()),
    }
}

/// Adds a newly paired PC, in place of an earlier pairing with the same PC.
fn remember(hosts: &mut Vec<Pc>, pc: Pc) {
    hosts.retain(|known| known.id != pc.id);
    hosts.push(pc);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn hdr_is_asked_for_only_with_ten_bit_hevc() {
        assert_eq!(video_format(None, false), (Codec::H264, false));
        assert_eq!(video_format(None, true), (Codec::Hevc, true));
        // A forced codec means an SDR stream, also where HDR is possible.
        assert_eq!(video_format(Some("hevc"), true), (Codec::Hevc, false));
        assert_eq!(video_format(Some("h264"), true), (Codec::H264, false));
    }

    #[test]
    fn maps_the_xbox_state_to_the_gamepad() {
        let state = crate::route::Routed { axes: [100, -200, i16::MIN, i16::MAX], buttons: 16 | 8 | 256 | 2 | 32768, lt: 65534, rt: 300 };
        let pad = gamepad(&state);
        assert_eq!(pad.buttons, gamestream::BUTTON_A | gamestream::BUTTON_BACK | gamestream::BUTTON_UP
            | gamestream::BUTTON_GUIDE | gamestream::BUTTON_RIGHT_STICK);
        assert_eq!((pad.left_trigger, pad.right_trigger), (255, 1));
        assert_eq!(pad.left_stick, (100, 200));
        assert_eq!(pad.right_stick, (i16::MIN, -i16::MAX));
        // Pushing a stick fully down must not wrap around to up.
        let down = crate::route::Routed { axes: [0, i16::MIN, 0, 0], ..crate::route::Routed { axes: [0; 4], buttons: 0, lt: 0, rt: 0 } };
        assert_eq!(gamepad(&down).left_stick, (0, i16::MAX));
    }


    fn game(name: &str, pc: &str) -> Game {
        Game { name: name.into(), pc: paired(pc, "10.0.0.1"), app: 1, running: false, hdr: false, end_other: None }
    }

    #[test]
    fn a_pc_counts_as_paired_only_from_a_readable_saved_pairing() {
        let saved = json!({"key": "", "certificate": "",
            "hosts": [{"id": "den", "name": "den", "address": "10.0.0.1", "certificate": ""}]});
        assert!(!parse(Some(serde_json::to_vec(&saved).unwrap())).hosts.is_empty());
        assert!(parse(Some(br#"{"key": "", "certificate": ""}"#.to_vec())).hosts.is_empty());
        assert!(parse(Some(b"{".to_vec())).hosts.is_empty());
        assert!(parse(None).hosts.is_empty());
    }

    #[test]
    fn a_given_address_is_asked_like_a_found_one_and_only_once() {
        let saved: Saved = serde_json::from_value(json!({"key": "", "certificate": "",
            "hosts": [{"id": "den", "name": "den", "address": "10.0.0.1", "certificate": ""}],
            "addresses": ["192.0.2.10", "10.0.0.1"]})).unwrap();
        assert_eq!(saved.asked(["192.0.2.10".to_owned(), "10.0.0.7".to_owned()]),
            ["10.0.0.1", "192.0.2.10", "10.0.0.7"]);
        // A file written before the addresses existed still loads.
        let mut old: Saved = serde_json::from_value(json!({"key": "", "certificate": ""})).unwrap();
        assert!(old.asked([]).is_empty());
        // An added address is kept once and asked from then on.
        old.add_address("192.0.2.10");
        old.add_address("192.0.2.10");
        assert_eq!(old.addresses, ["192.0.2.10"]);
        assert_eq!(old.asked([]), ["192.0.2.10"]);
    }

    #[test]
    fn starting_a_game_would_end_only_another_app_on_the_same_pc() {
        let app = |name: &str, pc: &str, app: u32, running: bool| Game { app, running, ..game(name, pc) };
        let catalog = Catalog { pcs: Vec::new(), games: vec![
            app("Desktop", "den", 1, false), app("Big Picture", "den", 2, true), app("Desktop", "attic", 1, false)] };
        let (den, attic) = (&catalog.games[0], &catalog.games[2]);
        assert_eq!(catalog.choose("id-den", 0), Some(Choice::Replace(den.clone(), catalog.games[1].clone())));
        // The running app offers resume and quit; a PC that runs nothing just starts.
        assert_eq!(catalog.choose("id-den", 1), Some(Choice::Running(catalog.games[1].clone())));
        assert_eq!(catalog.choose("id-attic", 0), Some(Choice::Start(attic.clone())));
        assert_eq!(catalog.choose("id-attic", 1), None);
        let panel = Panel { width: 1, height: 1, fps: 60 };
        // The consent names the app it was given for, and a reconnect drops it.
        let agreed = catalog.games[0].clone().ending(&catalog.games[1]);
        assert_eq!(agreed.start(panel, false).end_other, Some(2));
        assert_eq!(catalog.games[0].start(panel, false).end_other, None);
        assert_eq!(agreed.asking_again().start(panel, false).end_other, None);
        // HDR is asked for only when the user wants it and the PC can encode it.
        let capable = Game { hdr: true, ..game("Desktop", "den") };
        assert!(capable.start(panel, true).hdr);
        assert!(!capable.start(panel, false).hdr);
        assert!(!catalog.games[0].start(panel, true).hdr);
    }

    fn paired(name: &str, address: &str) -> Pc {
        Pc { id: format!("id-{name}"), name: name.into(), address: address.into(), certificate: String::new() }
    }

    #[test]
    fn pairing_again_replaces_the_pairing_and_keeps_the_other_pcs() {
        let mut hosts = vec![paired("den", "10.0.0.1"), paired("attic", "10.0.0.2")];
        remember(&mut hosts, paired("den", "10.0.0.7"));
        remember(&mut hosts, paired("shed", "10.0.0.3"));
        assert_eq!(hosts, [paired("attic", "10.0.0.2"), paired("den", "10.0.0.7"), paired("shed", "10.0.0.3")]);
    }

    #[test]
    fn a_paired_pc_is_followed_to_its_new_address_and_counted_once() {
        let saved = vec![paired("den", "10.0.0.1"), paired("attic", "10.0.0.2")];
        let moved = paired("den", "10.0.0.9");
        let games = |pc: &Pc| vec![Game { pc: pc.clone(), ..game("Desktop", "den") }];
        let new = || Unpaired { id: "id-new".into(), name: "new".into(), address: "10.0.0.3".into() };
        // The den and new PCs answer on two network interfaces, den without its
        // pairing on one of them; the attic PC is off.
        let answers = vec![
            Err(Unpaired { id: "id-den".into(), ..new() }),
            Ok((moved.clone(), games(&moved))),
            Ok((paired("den", "10.0.0.10"), games(&paired("den", "10.0.0.10")))),
            Err(new()),
            Err(Unpaired { address: "10.0.0.11".into(), ..new() }),
        ];
        let catalog = collect(answers, &saved);
        assert_eq!(catalog.games, games(&moved));
        assert_eq!(catalog.pcs, [Computer::Paired(moved.clone()), Computer::Unpaired(new()),
            Computer::Offline(paired("attic", "10.0.0.2"))]);
        // A paired PC that answers without its pairing is not offline too.
        let revoked = Unpaired { id: "id-attic".into(), ..new() };
        assert_eq!(collect(vec![Err(revoked.clone())], &saved).pcs,
            [Computer::Unpaired(revoked), Computer::Offline(paired("den", "10.0.0.1"))]);
        // The saved PC follows its new address; a pairing renewed during the
        // search keeps its new certificate.
        let mut current = saved.clone();
        current[0].certificate = "renewed".into();
        assert!(follow(&mut current, &catalog.pcs));
        assert_eq!(current, [Pc { certificate: "renewed".into(), ..moved.clone() }, paired("attic", "10.0.0.2")]);
        // Nothing to save when the PC answers where it was.
        assert!(!follow(&mut current, &catalog.pcs));
    }

    #[test]
    fn quitting_ends_only_the_app_the_user_chose() {
        assert_eq!(ends(7, 7), Ok(true));
        assert_eq!(ends(0, 7), Ok(false));
        assert!(ends(9, 7).is_err());
    }

    #[test]
    fn lsfg_halves_the_rate_the_pc_sends() {
        let panel = Panel { width: 1280, height: 800, fps: 90 };
        assert_eq!(panel.for_lsfg(true).fps, 45);
        assert_eq!(panel.for_lsfg(false).fps, 90);
    }

    #[test]
    fn an_app_grid_shows_one_pc_and_marks_the_running_app() {
        let mut catalog = Catalog { games: vec![game("Desktop", "den"), Game { running: true, ..game("Hades", "den") },
            game("Desktop", "attic")], pcs: vec![Computer::Paired(paired("den", "10.0.0.1")), Computer::Offline(paired("attic", "10.0.0.2"))] };
        let apps: Vec<(&str, bool)> = catalog.apps("id-den").iter().map(|game| (game.name.as_str(), game.running)).collect();
        assert_eq!(apps, [("Desktop", false), ("Hades", true)]);
        // Only a PC that answered with its pairing can be opened.
        assert!(catalog.paired("id-den").is_some());
        assert!(catalog.paired("id-attic").is_none());
        catalog.pcs.clear();
        assert!(catalog.paired("id-den").is_none());
    }

    #[test]
    fn sizes_the_stream_for_the_device() {
        let panel = |name| Panel::of(&Device { id: "", name });
        assert_eq!(panel("ROG Ally"), Panel { width: 1920, height: 1080, fps: 120 });
        assert_eq!(panel("Steam Deck OLED"), Panel { width: 1280, height: 800, fps: 90 });
        assert_eq!(panel("Gamepad"), Panel { width: 1920, height: 1080, fps: 60 });
    }

    #[test]
    fn a_smaller_stream_keeps_the_screens_shape() {
        let deck = Panel { width: 1280, height: 800, fps: 90 };
        assert_eq!(deck.scaled_to(720), Panel { width: 1152, height: 720, fps: 90 });
        assert_eq!(deck.scaled_to(0), deck);
        assert_eq!(deck.scaled_to(800), deck);
        assert_eq!(deck.scaled_to(1080), Panel { width: 1728, height: 1080, fps: 90 });
        let ally = Panel { width: 1920, height: 1080, fps: 120 };
        assert_eq!(ally.scaled_to(800), Panel { width: 1424, height: 800, fps: 120 });
    }

    #[test]
    fn the_saved_file_keeps_identity_and_pcs() {
        let saved = Saved { key: "k".into(), certificate: "c".into(),
            hosts: vec![Pc { id: "i".into(), name: "den".into(), address: "192.168.1.9".into(), certificate: "h".into() }],
            addresses: Vec::new() };
        let text = serde_json::to_string(&saved).unwrap();
        let back: Saved = serde_json::from_str(&text).unwrap();
        assert_eq!((back.key.as_str(), back.certificate.as_str()), ("k", "c"));
        assert_eq!(back.hosts, saved.hosts);
        // A file from before any pairing has no host list.
        assert!(serde_json::from_str::<Saved>(r#"{"key":"k","certificate":"c"}"#).unwrap().hosts.is_empty());
    }
}
