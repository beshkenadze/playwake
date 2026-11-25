//! PC game streaming from Sunshine hosts with Playwake's own client: finding
//! and pairing PCs, and listing what they can stream.

use crate::{device::Device, session_store};
use gamestream::{Host, Identity};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{sync::Mutex, thread, time::Duration};

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
    pub name: String,
    pub address: String,
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
    pub games: Vec<Game>,
    pub unpaired: Vec<Unpaired>,
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
}

impl Game {
    /// The tile text; the PC is named when the catalog spans several.
    pub fn label(&self, several_pcs: bool, running: &str) -> String {
        let name = if several_pcs { format!("{} · {}", self.name, self.pc.name) } else { self.name.clone() };
        if self.running { format!("{name} · {running}") } else { name }
    }

    pub fn pc(&self) -> &str {
        &self.pc.name
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

    /// What the media worker needs to start this stream; with HDR if the
    /// user wants it and the PC can encode it.
    pub fn start(&self, panel: Panel, hdr: bool) -> Value {
        let saved = load();
        json!({
            "address": self.pc.address, "certificate": self.pc.certificate,
            "client_key": saved.key, "client_certificate": saved.certificate,
            "app": self.app, "fps": panel.fps, "end_other": self.end_other, "hdr": hdr && self.hdr,
        })
    }
}

impl Catalog {
    /// Another app that runs on the PC of `game`. Starting `game` would end it.
    pub fn running_instead_of(&self, game: &Game) -> Option<&Game> {
        self.games.iter().find(|other| other.running && other.pc.id == game.pc.id && other.app != game.app)
    }

    pub fn several_pcs(&self) -> bool {
        self.games.iter().any(|game| game.pc != self.games[0].pc)
    }
}

fn saved_path() -> Option<std::path::PathBuf> {
    Some(session_store::path().ok()?.with_extension("pc.json"))
}

fn load() -> Saved {
    saved_path().and_then(|path| std::fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
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
    let unpaired = Unpaired { name: info.name.clone(), address: address.to_owned() };
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

/// Takes the answers of all addresses into a catalog and brings the saved PCs
/// up to date. A PC that answers at several addresses counts once. Returns
/// whether a saved PC changed.
fn collect(answers: Vec<std::result::Result<(Pc, Vec<Game>), Unpaired>>, saved: &mut [Pc]) -> (Catalog, bool) {
    let mut catalog = Catalog::default();
    let mut seen = Vec::new();
    let mut changed = false;
    for answer in answers {
        match answer {
            Ok((pc, games)) => {
                if seen.contains(&pc.id) { continue; }
                seen.push(pc.id.clone());
                catalog.games.extend(games);
                if let Some(known) = saved.iter_mut().find(|known| known.id == pc.id && **known != pc) {
                    *known = pc;
                    changed = true;
                }
            }
            Err(unpaired) => catalog.unpaired.push(unpaired),
        }
    }
    (catalog, changed)
}

/// The paired PCs' games and the unpaired PCs found on the network. A PC that
/// is off is left out.
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
    let (catalog, changed) = collect(answers, &mut saved.hosts);
    if changed { store(&saved)?; }
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
    static SAVING: Mutex<()> = Mutex::new(());
    let _saving = SAVING.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut saved = load();
    remember(&mut saved.hosts, Pc { id, name: pc.name.clone(), address: pc.address.clone(), certificate });
    store(&saved)
}

/// Adds a newly paired PC, in place of an earlier pairing with the same PC.
fn remember(hosts: &mut Vec<Pc>, pc: Pc) {
    hosts.retain(|known| known.id != pc.id);
    hosts.push(pc);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn game(name: &str, pc: &str) -> Game {
        Game { name: name.into(), pc: paired(pc, "10.0.0.1"), app: 1, running: false, hdr: false, end_other: None }
    }

    #[test]
    fn a_given_address_is_asked_like_a_found_one_and_only_once() {
        let saved: Saved = serde_json::from_value(json!({"key": "", "certificate": "",
            "hosts": [{"id": "den", "name": "den", "address": "10.0.0.1", "certificate": ""}],
            "addresses": ["192.168.1.159", "10.0.0.1"]})).unwrap();
        assert_eq!(saved.asked(["192.168.1.159".to_owned(), "10.0.0.7".to_owned()]),
            ["10.0.0.1", "192.168.1.159", "10.0.0.7"]);
        // A file written before the addresses existed still loads.
        let old: Saved = serde_json::from_value(json!({"key": "", "certificate": ""})).unwrap();
        assert!(old.asked([]).is_empty());
    }

    #[test]
    fn starting_a_game_would_end_only_another_app_on_the_same_pc() {
        let app = |name: &str, pc: &str, app: u32, running: bool| Game { app, running, ..game(name, pc) };
        let catalog = Catalog { unpaired: Vec::new(), games: vec![
            app("Desktop", "den", 1, false), app("Big Picture", "den", 2, true), app("Desktop", "attic", 1, false)] };
        let name = |game: &Game| catalog.running_instead_of(game).map(|other| other.name.as_str());
        assert_eq!(name(&catalog.games[0]), Some("Big Picture"));
        // Rejoining the running app, or a game on a PC that runs nothing, ends nothing.
        assert_eq!(name(&catalog.games[1]), None);
        assert_eq!(name(&catalog.games[2]), None);
        let panel = Panel { width: 1, height: 1, fps: 60 };
        // The consent names the app it was given for, and a reconnect drops it.
        let agreed = catalog.games[0].clone().ending(&catalog.games[1]);
        assert_eq!(agreed.start(panel, false)["end_other"], 2);
        assert!(catalog.games[0].start(panel, false)["end_other"].is_null());
        assert!(agreed.asking_again().start(panel, false)["end_other"].is_null());
        // HDR is asked for only when the user wants it and the PC can encode it.
        let capable = Game { hdr: true, ..game("Desktop", "den") };
        assert_eq!(capable.start(panel, true)["hdr"], true);
        assert_eq!(capable.start(panel, false)["hdr"], false);
        assert_eq!(catalog.games[0].start(panel, true)["hdr"], false);
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
        let mut saved = vec![paired("den", "10.0.0.1"), paired("attic", "10.0.0.2")];
        let moved = paired("den", "10.0.0.9");
        let games = |pc: &Pc| vec![Game { pc: pc.clone(), ..game("Desktop", "den") }];
        // The den PC answers on two network interfaces; the attic PC is off.
        let answers = vec![
            Ok((moved.clone(), games(&moved))),
            Ok((paired("den", "10.0.0.10"), games(&paired("den", "10.0.0.10")))),
            Err(Unpaired { name: "new".into(), address: "10.0.0.3".into() }),
        ];
        let (catalog, changed) = collect(answers, &mut saved);
        assert!(changed);
        assert_eq!(catalog.games, games(&moved));
        assert_eq!(catalog.unpaired, [Unpaired { name: "new".into(), address: "10.0.0.3".into() }]);
        assert_eq!(saved, [moved.clone(), paired("attic", "10.0.0.2")]);
        // Nothing to save when the PC answers where it was.
        assert!(!collect(vec![Ok((moved.clone(), games(&moved)))], &mut saved).1);
    }

    #[test]
    fn names_the_pc_only_when_there_are_several() {
        let mut catalog = Catalog { games: vec![game("Desktop", "den"), game("Hades", "den")], unpaired: Vec::new() };
        assert!(!catalog.several_pcs());
        assert_eq!(catalog.games[0].label(catalog.several_pcs(), "running"), "Desktop");
        catalog.games.push(Game { running: true, ..game("Desktop", "attic") });
        assert!(catalog.several_pcs());
        assert_eq!(catalog.games[2].label(catalog.several_pcs(), "running"), "Desktop · attic · running");
        assert!(!Catalog::default().several_pcs());
    }

    #[test]
    fn sizes_the_stream_for_the_device() {
        let panel = |name| Panel::of(&Device { id: "", name });
        assert_eq!(panel("ROG Ally"), Panel { width: 1920, height: 1080, fps: 120 });
        assert_eq!(panel("Steam Deck OLED"), Panel { width: 1280, height: 800, fps: 90 });
        assert_eq!(panel("Gamepad"), Panel { width: 1920, height: 1080, fps: 60 });
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
