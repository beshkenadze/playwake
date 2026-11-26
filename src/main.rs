mod cloud;
mod covers;
mod deck;
mod device;
mod input;
mod journal;
mod library;
mod lsfg;
mod media;
mod pc;
mod session_store;
mod xbox;

use deck::{Action, Key, Menu, Tab};
use session_store::{Language, Preferences};
use gpui_kit::*;
use qrcode::{Color, QrCode};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use serde_json::json;
use std::{sync::{Arc, atomic::{AtomicBool, Ordering}, mpsc}, thread, time::{Duration, Instant}};

enum Event {
    Device(DevicePrompt),
    Account { xbox: Arc<xbox::Xbox> },
    SignInEnded(xbox::Result<()>),
    Stream(media::StreamEvent),
    StreamEnded(xbox::Result<()>),
    Preview(media::PreviewEvent),
    PreviewEnded(xbox::Result<()>),
    Menu(Key),
    Consoles(Arc<xbox::Xbox>, xbox::Result<Vec<xbox::Console>>, bool),
    CachedCloudCatalog(Arc<xbox::Xbox>, Vec<cloud::Game>),
    CloudCatalog(Arc<xbox::Xbox>, xbox::Result<Vec<cloud::Game>>),
    /// More covers are on disk.
    Covers,
    /// A fetch of covers has ended.
    CoversDone,
    PcCatalog(pc::Result<pc::Catalog>),
    PcPaired(pc::Unpaired, pc::Result<()>),
    /// A PC was added by its address, or the app it ran was quit.
    PcChanged(pc::Result<()>),
    /// Time to try again a PC stream that broke off.
    RetryStream,
}

/// How many times a PC stream that broke off is started again before the
/// error is shown, and the pause before each try: a Wi-Fi link that drops
/// for a few seconds comes back by then, and the game still runs on the PC.
const STREAM_RETRIES: u32 = 3;
const STREAM_RETRY_PAUSE: Duration = Duration::from_secs(3);
/// A stream that played this long broke off rather than failed to start, so
/// it gets its tries anew.
const STREAM_STABLE: Duration = Duration::from_secs(30);

/// A catalog in the order of its names, for the library and its letters.
fn by_name<T>(mut items: Vec<T>, name: impl Fn(&T) -> &str) -> Vec<T> {
    items.sort_by_cached_key(|item| deck::library_order(name(item)));
    items
}

/// Whether a PC stream that ended with an error is started again: not one
/// the user stopped, and only one that played or is already being retried.
fn tries_again(cancelled: bool, had_video: bool, retries: u32) -> bool {
    !cancelled && (had_video || retries > 0) && retries < STREAM_RETRIES
}

/// A PC tab dialog in front of the menu.
enum PcDialog {
    /// Asks before quitting the app, which can lose unsaved progress. It
    /// opens with X or a tap, so A answers it at once.
    Quit(pc::Game),
    /// Asks before closing Playwake. It opens with B, so A answers it at once.
    Exit,
    /// The address typed for a PC that the search does not find.
    Address(String),
}

struct DevicePrompt {
    url: String,
    code: String,
    prefilled: bool,
    qr: Option<Arc<QrCode>>,
}

fn login_qr(qr: Arc<QrCode>, module: Pixels) -> impl IntoElement {
    let quiet_zone = 4;
    let side = module * (qr.width() + quiet_zone * 2) as f32;
    canvas(|_, _, _| (), move |bounds, (), window, _| {
        window.paint_quad(fill(bounds, rgb(0xffffff)));
        for y in 0..qr.width() {
            for x in 0..qr.width() {
                if qr[(x, y)] == Color::Dark {
                    let origin = bounds.origin
                        + point(module * (x + quiet_zone) as f32, module * (y + quiet_zone) as f32);
                    window.paint_quad(fill(Bounds { origin, size: size(module, module) }, rgb(0x000000)));
                }
            }
        }
    }).w(side).h(side).flex_none()
}

struct RemotePlay {
    tx: mpsc::Sender<Event>,
    account: Option<Arc<xbox::Xbox>>,
    consoles: Vec<xbox::Console>,
    sign_in: Option<Arc<AtomicBool>>,
    stream: Option<Arc<AtomicBool>>,
    stream_commands: Option<mpsc::Sender<media::StreamCommand>>,
    overlay_open: bool,
    overlay_index: usize,
    restore_windowed: bool,
    prefs: Preferences,
    /// What LSFG lacks on this device; it stays off until that is installed.
    lsfg_missing: Option<lsfg::Missing>,
    menu: Menu,
    menu_player: Option<media::Player>,
    keyboard_focus: FocusHandle,
    /// The library grid's scroll, and the row it was last scrolled to.
    library_scroll: ScrollHandle,
    library_scrolled: std::cell::Cell<Option<(&'static str, usize)>>,
    /// Fetches of covers under way; their tiles pulse until the covers come.
    covers_loading: u32,
    active_target: Option<media::Target>,
    preview: Option<Preview>,
    cloud_catalog: Option<Vec<cloud::Game>>,
    cloud_refreshed_at: Option<Instant>,
    cloud_loading: bool,
    cloud_error: Option<String>,
    pc_catalog: pc::Catalog,
    pc_loading: bool,
    pc_error: Option<String>,
    /// The PC being paired and the PIN to enter on it.
    pc_pairing: Option<(pc::Unpaired, String)>,
    /// The game the user chose and the app its start would end on the PC.
    pc_replace: Option<(pc::Game, pc::Game, deck::Confirm)>,
    pc_dialog: Option<PcDialog>,
    reconnect: bool,
    /// A PC stream that broke off, waiting to be started again.
    retry: Option<media::Target>,
    /// Tries made since a stream last played for STREAM_STABLE.
    retries: u32,
    video_ready: bool,
    /// When the current stream first showed video.
    video_at: Option<Instant>,
    refreshing: bool,
    device: Option<DevicePrompt>,
    status: String,
}

struct Preview {
    cancel: Arc<AtomicBool>,
    prefs: Preferences,
    ready: bool,
    compare: bool,
}

impl Drop for RemotePlay {
    fn drop(&mut self) {
        if let Some(cancel) = &self.sign_in { cancel.store(true, Ordering::Relaxed); }
        if let Some(cancel) = &self.stream { cancel.store(true, Ordering::Relaxed); }
        if let Some(preview) = &self.preview { preview.cancel.store(true, Ordering::Relaxed); }
    }
}

fn x11_window(window: &Window) -> Option<media::VideoWindow> {
    let handle = HasWindowHandle::window_handle(window).ok()?;
    let xid = match handle.as_raw() {
        RawWindowHandle::Xcb(handle) => u64::from(handle.window.get()),
        RawWindowHandle::Xlib(handle) => handle.window,
        _ => return None,
    };
    Some(media::VideoWindow { xid })
}

impl RemotePlay {
    fn start_login(&mut self) {
        if self.sign_in.is_some() || self.stream.is_some() { return; }
        let cancel = Arc::new(AtomicBool::new(false));
        self.sign_in = Some(cancel.clone());
        self.device = None;
        self.status = "Requesting Microsoft sign-in…".into();
        journal::record("sign_in_started", json!({}));
        let tx = self.tx.clone();
        thread::spawn(move || {
            let result = (|| {
                let client = xbox::client()?;
                let client_id = xbox::public_client_id();
                let device = xbox::begin_sign_in(&client, &client_id)?;
                let (url, prefilled) = device.verification_link();
                journal::record("sign_in_waiting_for_confirmation", json!({"prefilled": prefilled}));
                let _ = tx.send(Event::Device(DevicePrompt {
                    qr: QrCode::new(url.as_str()).ok().map(Arc::new),
                    url: url.to_string(),
                    code: device.user_code.clone(),
                    prefilled,
                }));
                let xbox = Arc::new(xbox::finish_sign_in(client, client_id, device, &cancel)?);
                let _ = tx.send(Event::Account { xbox });
                Ok(())
            })();
            let _ = tx.send(Event::SignInEnded(result));
        });
    }

    fn start_stream(&mut self, target: media::Target, window: &mut Window) {
        if self.stream.is_some() || self.preview.is_some() || self.sign_in.is_some() { return; }
        let account = self.account.clone();
        if account.is_none() && !matches!(&target, media::Target::Pc(_)) { return; }
        let native_window = x11_window(window);
        let streaming_tab = match &target {
            media::Target::Home(_) => Tab::Xbox,
            media::Target::Cloud(_) => Tab::Cloud,
            media::Target::Pc(_) => Tab::Pc,
        };
        self.active_target = Some(target.clone());
        self.video_ready = false;
        self.video_at = None;
        self.menu.select_tab(streaming_tab);
        self.restore_windowed = native_window.is_some() && !window.is_fullscreen();
        if self.restore_windowed { window.toggle_fullscreen(); }
        let cancel = Arc::new(AtomicBool::new(false));
        self.stream = Some(cancel.clone());
        let t = deck::Text(self.prefs.russian());
        self.status = if streaming_tab == Tab::Pc { t.t("Подключение к ПК…", "Connecting to PC…") }
            else { t.t("Подключение к Xbox…", "Connecting to Xbox…") }.into();
        let tx = self.tx.clone();
        let resolution = self.resolution();
        let mut prefs = self.prefs;
        if native_window.is_none() { prefs.lsfg_enabled = false; }
        let run = media::Run::Interactive(prefs);
        let (controls, rx) = mpsc::channel();
        self.stream_commands = Some(controls);
        thread::spawn(move || {
            let notify = |event| { let _ = tx.send(Event::Stream(event)); };
            let result = match (target, account) {
                (media::Target::Pc(game), _) => media::play_pc(game, native_window, cancel, prefs, rx, notify),
                (target, Some(account)) => media::play(account, target, resolution, native_window, cancel, run, Some(rx), notify),
                (_, None) => Ok(()),
            };
            let _ = tx.send(Event::StreamEnded(result));
        });
    }

    fn start_preview(&mut self, prefs: Preferences, window: &Window) {
        if self.stream.is_some() || self.preview.is_some() || self.sign_in.is_some() { return; }
        let Some(window) = x11_window(window) else {
            self.status = deck::Text(self.prefs.russian()).t(
                "Для превью LSFG нужен X11 или XWayland", "LSFG preview needs X11 or XWayland").into();
            return;
        };
        let cancel = Arc::new(AtomicBool::new(false));
        self.preview = Some(Preview { cancel: cancel.clone(), prefs, ready: false, compare: false });
        self.status.clear();
        let tx = self.tx.clone();
        thread::spawn(move || {
            let result = media::preview(window, prefs, cancel,
                |event| { let _ = tx.send(Event::Preview(event)); });
            let _ = tx.send(Event::PreviewEnded(result));
        });
    }

    /// Sends what the user typed to the PC being streamed: Steam's on-screen
    /// keyboard or a real one. Returns whether the stream took the keystroke.
    fn type_into_stream(&self, keystroke: &Keystroke) -> bool {
        if self.stream.is_none() || !self.video_ready || self.overlay_open || self.menu.tab != Tab::Pc { return false; }
        let held = keystroke.modifiers;
        if let Some(code) = media::named_key(&keystroke.key) {
            self.control_stream(media::StreamCommand::Key(code));
        } else if !(held.control || held.alt || held.platform) {
            for character in keystroke.key_char.iter().flat_map(|typed| typed.chars()) {
                self.control_stream(media::StreamCommand::Type(character));
            }
        }
        true
    }

    fn control_stream(&self, command: media::StreamCommand) {
        if let Some(controls) = &self.stream_commands { let _ = controls.send(command); }
    }

    fn stop_stream(&mut self) {
        if let Some(cancel) = &self.stream { cancel.store(true, Ordering::Relaxed); }
        self.status = "Stopping stream…".into();
    }

    fn use_overlay_action(&mut self, index: usize) {
        match index {
            0 => self.control_stream(media::StreamCommand::CloseOverlay),
            1 => self.control_stream(media::StreamCommand::XboxGuide),
            2 => self.control_stream(media::StreamCommand::ToggleDiagnostics),
            3 => { self.reconnect = true; self.stop_stream(); }
            4 => self.stop_stream(),
            _ => {}
        }
    }

    fn show_account(&mut self, xbox: Arc<xbox::Xbox>) {
        self.status = String::new();
        self.account = Some(xbox);
        self.refresh_consoles(true);
        if self.menu.tab == Tab::Cloud { self.refresh_cloud(); }
    }

    fn refresh_cloud(&mut self) {
        if self.cloud_loading { return; }
        let Some(account) = self.account.clone() else { return; };
        self.cloud_loading = true;
        self.cloud_error = None;
        let read_cache = self.cloud_catalog.is_none();
        let tx = self.tx.clone();
        self.covers_loading += 1;
        thread::spawn(move || {
            let cached = read_cache.then(|| cloud::cached_catalog(&account)).flatten();
            if let Some(games) = &cached {
                let _ = tx.send(Event::CachedCloudCatalog(account.clone(), games.clone()));
            }
            let result = cloud::catalog(&account);
            let games = result.as_ref().ok().cloned().or(cached);
            let _ = tx.send(Event::CloudCatalog(account, result));
            let urls: Vec<&str> = games.iter().flatten().filter_map(|game| game.cover.as_deref()).collect();
            if let (false, Ok(client)) = (urls.is_empty(), xbox::client()) {
                covers::fetch(&cloud::COVERS, &urls, |url| covers::download(&client, url), || { let _ = tx.send(Event::Covers); });
            }
            let _ = tx.send(Event::CoversDone);
        });
    }

    fn refresh_pc(&mut self) {
        if self.pc_loading { return; }
        self.pc_loading = true;
        self.pc_error = None;
        let tx = self.tx.clone();
        self.covers_loading += 1;
        thread::spawn(move || {
            let result = pc::catalog();
            let games = result.as_ref().map(|catalog| catalog.games.clone()).unwrap_or_default();
            let _ = tx.send(Event::PcCatalog(result));
            if !games.is_empty() { pc::fetch_covers(&games, || { let _ = tx.send(Event::Covers); }); }
            let _ = tx.send(Event::CoversDone);
        });
    }

    /// Shows a PIN and waits, off the UI thread, for the user to enter it on the PC.
    fn start_pc_pairing(&mut self, unpaired: pc::Unpaired) {
        if self.pc_pairing.is_some() { return; }
        let pin = gamestream::pin();
        self.pc_pairing = Some((unpaired.clone(), pin.clone()));
        self.pc_error = None;
        journal::record("pc_pairing_started", json!({}));
        let tx = self.tx.clone();
        thread::spawn(move || {
            let result = pc::pair(&unpaired, &pin);
            let _ = tx.send(Event::PcPaired(unpaired, result));
        });
    }

    /// Runs a change to the PCs off the UI thread, then lists them again.
    fn change_pcs(&mut self, status: &str, change: impl FnOnce() -> pc::Result<()> + Send + 'static) {
        self.pc_error = None;
        self.status = status.into();
        let tx = self.tx.clone();
        thread::spawn(move || { let _ = tx.send(Event::PcChanged(change())); });
    }

    /// Edits the address being typed. Returns whether the keystroke was text.
    /// The position in the library on show of the name at `index` of the
    /// catalog: the same without a search, its place among the matches with one.
    fn view_position(&self, index: usize) -> usize {
        deck::library_names(self).map(|names| deck::library_view(&names, &self.menu.search))
            .and_then(|view| view.iter().position(|&at| at == index)).unwrap_or(index)
    }

    /// Typing goes to the library's search; Steam's keyboard comes up for it.
    fn start_search(&mut self) {
        if !deck::library_names(self).is_some_and(|names| !names.is_empty()) { return; }
        self.menu.searching = true;
        self.menu.focused_tab = None;
        if std::env::var_os("SteamAppId").is_some() {
            let _ = std::process::Command::new("steam").arg("steam://open/keyboard")
                .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn();
        }
    }

    /// Ends the search, or clears it first; the selection stays on the game
    /// that was selected among the matches.
    fn leave_search(&mut self) {
        let position = self.menu.library_position().map(|position| *position);
        let selected = deck::library_names(self).zip(position)
            .and_then(|(names, position)| deck::library_view(&names, &self.menu.search).get(position).copied());
        if self.menu.search.is_empty() { self.menu.searching = false; } else { self.menu.search.clear(); }
        if let (Some(selected), Some(position)) = (selected, self.menu.library_position()) { *position = selected; }
    }

    /// Ends the search at once, keeping the selected game selected.
    fn close_search(&mut self) {
        if !self.menu.search.is_empty() { self.leave_search(); }
        self.menu.end_search();
    }

    /// The catalog index of the selected game in the library on show.
    fn selected_index(&mut self) -> Option<usize> {
        let position = *self.menu.library_position()?;
        deck::library_names(self).and_then(|names| deck::library_view(&names, &self.menu.search).get(position).copied())
    }

    /// Takes what is typed into the library's search: any letter starts one.
    fn type_into_search(&mut self, keystroke: &Keystroke) -> bool {
        if self.stream.is_some() || self.pc_dialog.is_some() || !deck::library_names(self).is_some_and(|names| !names.is_empty()) {
            return false;
        }
        let held = keystroke.modifiers;
        if held.control || held.alt || held.platform { return false; }
        if keystroke.key == "backspace" {
            if !self.menu.searching { return false; }
            self.menu.search.pop();
        } else {
            let Some(text) = keystroke.key_char.as_deref().filter(|text| !text.chars().any(char::is_control)) else { return false };
            if !self.menu.searching && !text.chars().all(char::is_alphanumeric) { return false; }
            self.menu.searching = true;
            self.menu.search.push_str(text);
        }
        if let Some(position) = self.menu.library_position() { *position = 0; }
        true
    }

    fn type_into_address(&mut self, keystroke: &Keystroke) -> bool {
        let Some(PcDialog::Address(address)) = &mut self.pc_dialog else { return false };
        deck::edit_address(address, &keystroke.key, keystroke.key_char.as_deref())
    }

    /// Takes a step of the open PC dialog; every step closes or replaces it.
    fn pc_step(&mut self, step: deck::PcStep) {
        let t = deck::Text(self.prefs.russian());
        match (step, self.pc_dialog.take()) {
            (deck::PcStep::QuitApp, Some(PcDialog::Quit(game))) =>
                self.change_pcs(t.t("Завершение приложения…", "Quitting the app…"), move || pc::quit(&game)),
            (deck::PcStep::Add, Some(PcDialog::Address(address))) =>
                self.change_pcs(t.t("Проверка адреса…", "Checking the address…"), move || pc::add(&address)),
            _ => {}
        }
    }

    fn start_menu_input(&mut self) {
        if cfg!(target_os = "linux") {
            let input_events = self.tx.clone();
            self.menu_player = media::menu_input(move |key| { let _ = input_events.send(Event::Menu(key)); }).ok();
        }
    }

    fn resolution(&self) -> xbox::Resolution {
        xbox::Resolution::CHOICES.into_iter().map(|(choice, _)| choice)
            .find(|choice| choice.dimensions().1 == self.prefs.height).unwrap_or_default()
    }

    fn save(&mut self) {
        if session_store::save_preferences(self.prefs).is_err() {
            self.status = deck::Text(self.prefs.russian()).t("Не удалось сохранить настройки", "Could not save settings").into();
        }
    }

    fn refresh_consoles(&mut self, auto_connect: bool) {
        if self.refreshing { return; }
        let Some(account) = self.account.clone() else { return; };
        self.refreshing = true;
        self.status = deck::Text(self.prefs.russian()).t("Обновление консолей…", "Refreshing consoles…").into();
        let tx = self.tx.clone();
        thread::spawn(move || {
            let consoles = account.consoles();
            let _ = tx.send(Event::Consoles(account, consoles, auto_connect));
        });
    }

    fn auto_connect(&mut self, window: &mut Window) {
        if self.prefs.auto_connect && self.consoles.len() == 1 {
            let console = self.consoles[0].clone();
            if ["on", "standby", "connectedstandby"].iter().any(|power| console.power.eq_ignore_ascii_case(power)) {
                self.start_stream(media::Target::Home(console), window);
            }
        }
    }

    /// Looks again for what LSFG needs, as the user may have installed it
    /// since, and turns LSFG off while something is missing.
    fn check_lsfg(&mut self) {
        self.lsfg_missing = lsfg::missing();
        if self.lsfg_missing.is_some() { self.prefs.lsfg_enabled = false; }
    }

    fn activate(&mut self, action: Action, direction: isize, window: &mut Window, cx: &mut Context<Self>) {
        journal::record("deck_action", json!({"action": format!("{action:?}")}));
        if matches!(action, Action::Settings | Action::Section(_)) { self.check_lsfg(); }
        // The user chose something else; a stream waiting to be tried again is not wanted.
        if self.retry.take().is_some() {
            self.retries = 0;
            if std::mem::take(&mut self.restore_windowed) && window.is_fullscreen() { window.toggle_fullscreen(); }
        }
        let t = deck::Text(self.prefs.russian());
        let save = match action {
            Action::Settings | Action::Home => {
                self.close_search();
                self.menu.select_tab(if action == Action::Settings { Tab::Settings } else { Tab::Xbox });
                false
            }
            Action::Cloud => {
                if self.menu.tab != Tab::Cloud { self.close_search(); }
                self.menu.select_tab(Tab::Cloud);
                if self.cloud_refreshed_at.is_none_or(|time| time.elapsed() >= Duration::from_secs(300)) {
                    self.refresh_cloud();
                }
                false
            }
            Action::Play(index) => {
                if let Some(console) = self.consoles.get(index).cloned() {
                    self.menu.console = index;
                    self.start_stream(media::Target::Home(console), window);
                }
                false
            }
            Action::Pc => {
                if self.menu.tab != Tab::Pc { self.close_search(); }
                self.menu.select_tab(Tab::Pc);
                // The PC's apps change: a host can add games or change their IDs.
                self.refresh_pc();
                false
            }
            Action::PcHost(index) => {
                self.menu.pc = index;
                match self.pc_catalog.pcs.get(index).cloned() {
                    Some(pc::Computer::Paired(pc)) => { self.menu.open_pc(pc.id()); self.refresh_pc(); }
                    Some(pc::Computer::Unpaired(pc)) => self.start_pc_pairing(pc),
                    Some(pc::Computer::Offline(_)) => self.refresh_pc(),
                    None => {}
                }
                false
            }
            Action::PcAdd => {
                self.menu.pc = self.pc_catalog.pcs.len();
                self.pc_dialog = Some(PcDialog::Address(String::new()));
                false
            }
            Action::PcList => { self.menu.pc_open = None; false }
            Action::PcGame(index) => {
                self.menu.pc_game = self.view_position(index);
                match self.menu.pc_open.as_deref().and_then(|id| self.pc_catalog.choose(id, index)) {
                    Some(pc::Choice::Running(game)) => self.start_stream(media::Target::Pc(game), window),
                    // Ending an app on the PC can cost a running game: ask first.
                    Some(pc::Choice::Replace(game, running)) =>
                        self.pc_replace = Some((game, running, deck::Confirm::opened(Instant::now()))),
                    Some(pc::Choice::Start(game)) => self.start_stream(media::Target::Pc(game), window),
                    None => {}
                }
                false
            }
            Action::PcQuit(index) => {
                self.menu.pc_game = self.view_position(index);
                if let Some(pc::Choice::Running(game)) = self.menu.pc_open.as_deref().and_then(|id| self.pc_catalog.choose(id, index)) {
                    self.pc_dialog = Some(PcDialog::Quit(game));
                }
                false
            }
            Action::VideoTarget => { self.menu.switch_video_target(); false }
            Action::Search => { self.start_search(); false }
            Action::Letter(letter) => {
                if let Some(names) = deck::library_names(self) {
                    let view = deck::library_view(&names, &self.menu.search);
                    let found = deck::letter_position(&names, &view, letter);
                    if let (Some(found), Some(position)) = (found, self.menu.library_position()) { *position = found; }
                }
                false
            }
            Action::AskExit => { self.pc_dialog = Some(PcDialog::Exit); false }
            Action::CloudGame(index) => {
                if let Some(game) = self.cloud_catalog.as_ref().and_then(|catalog| catalog.get(index)) {
                    self.menu.cloud_game = self.view_position(index);
                    self.start_stream(media::Target::Cloud(game.clone()), window);
                }
                false
            }
            Action::SignIn => { self.start_login(); false }
            Action::Browser => {
                if let Some(device) = &self.device { cx.open_url(&device.url); }
                false
            }
            Action::Cancel => {
                if self.menu.confirming_sign_out { self.menu.confirming_sign_out = false; }
                else if let Some(cancel) = &self.sign_in { cancel.store(true, Ordering::Relaxed); }
                else if self.stream.is_some() { self.stop_stream(); }
                false
            }
            Action::Section(index) => {
                self.menu.select_section(index);
                false
            }
            Action::Refresh => {
                match self.menu.tab {
                    Tab::Cloud => self.refresh_cloud(),
                    Tab::Pc => self.refresh_pc(),
                    _ => self.refresh_consoles(false),
                }
                false
            }
            Action::SignOut => {
                self.menu.confirming_sign_out = true;
                self.menu.browser_cancel = false;
                false
            }
            Action::ConfirmSignOut => {
                if session_store::clear().is_ok() {
                    self.account = None;
                    self.consoles.clear();
                    self.refreshing = false;
                    self.cloud_catalog = None;
                    self.cloud_refreshed_at = None;
                    self.cloud_loading = false;
                    self.cloud_error = None;
                    self.menu = Menu { pc_available: self.menu.pc_available, ..Menu::default() };
                    self.status.clear();
                    journal::record("signed_out", json!({}));
                } else { self.status = t.t("Не удалось выйти", "Could not sign out").into(); }
                false
            }
            Action::Resolution => {
                let choices = xbox::Resolution::CHOICES;
                let current = choices.iter().position(|(r, _)| *r == self.resolution()).unwrap_or(0);
                self.prefs.height = choices[(current as isize + direction).rem_euclid(choices.len() as isize) as usize].0.dimensions().1;
                journal::record("resolution_selected", json!({"height": self.prefs.height}));
                true
            }
            Action::Bitrate => { self.prefs.bitrate_mbps = (self.prefs.bitrate_mbps as isize + 5 * direction).clamp(5,40) as u32; true }
            Action::Jitter => { self.prefs.jitter_ms = session_store::next_jitter(self.prefs.jitter_ms, direction); true }
            Action::Upscale => { self.prefs.lanczos_upscale = !self.prefs.lanczos_upscale; true }
            Action::PcBitrate => { self.prefs.pc_bitrate_mbps = session_store::next_pc_bitrate(self.prefs.pc_bitrate_mbps, direction); true }
            Action::PcStreamHeight => {
                let screen = pc::Panel::of(device::current()).height;
                self.prefs.pc_stream_height = session_store::next_pc_stream_height(self.prefs.pc_stream_height, direction, screen);
                true
            }
            Action::PcHdr => { self.prefs.pc_hdr = !self.prefs.pc_hdr; true }
            Action::Lsfg => {
                if !self.prefs.lsfg_enabled {
                    if let Err(error) = lsfg::Runtime::discover() {
                        self.status = error;
                        return;
                    }
                }
                self.prefs.lsfg_enabled = !self.prefs.lsfg_enabled;
                self.status.clear();
                true
            }
            Action::LsfgPerformance => { self.prefs.lsfg_performance = !self.prefs.lsfg_performance; true }
            Action::LsfgFlowScale => {
                self.prefs.lsfg_flow_percent = if self.prefs.lsfg_flow_percent == 75 { 100 } else { 75 };
                true
            }
            Action::LsfgPreview => { self.start_preview(self.prefs, window); false }
            Action::Volume => { self.prefs.volume = (self.prefs.volume as isize + 10 * direction).clamp(0,100) as u32; true }
            Action::Mute => { self.prefs.mute = !self.prefs.mute; true }
            Action::Compact => { self.prefs.compact_stats = !self.prefs.compact_stats; true }
            Action::AutoConnect => { self.prefs.auto_connect = !self.prefs.auto_connect; true }
            Action::Palette => { self.prefs.xbox_palette = !self.prefs.xbox_palette; true }
            Action::Language => {
                let choices = [Language::System, Language::English, Language::Russian];
                self.prefs.language = choices[(self.prefs.language as isize + direction).rem_euclid(3) as usize];
                true
            }
        };
        if save { self.save(); }
    }

    fn menu_key(&mut self, key: Key, window: &mut Window, cx: &mut Context<Self>) {
        if self.pc_replace.is_some() {
            match key {
                Key::Accept => {
                    let deliberate = self.pc_replace.as_mut().is_some_and(|(_, _, confirm)| confirm.press(Instant::now()));
                    if let Some((game, running, _)) = self.pc_replace.take_if(|_| deliberate) {
                        self.start_stream(media::Target::Pc(game.ending(&running)), window);
                    }
                }
                Key::Back => self.pc_replace = None,
                _ => {}
            }
            return;
        }
        if let Some(dialog) = &mut self.pc_dialog {
            match deck::pc_dialog_step(dialog, key) {
                Some(deck::PcStep::Exit) => cx.quit(),
                Some(step) => self.pc_step(step),
                None => {}
            }
            return;
        }
        if self.pc_pairing.is_some() {
            // The PC keeps waiting for the PIN; a late success still pairs.
            if key == Key::Back { self.pc_pairing = None; }
            return;
        }
        if let Some(preview) = &mut self.preview {
            match key {
                Key::Accept if preview.ready && !preview.cancel.load(Ordering::Relaxed) => {
                    preview.compare = true;
                    preview.cancel.store(true, Ordering::Relaxed);
                }
                Key::Back => {
                    preview.compare = false;
                    preview.cancel.store(true, Ordering::Relaxed);
                }
                _ => {}
            }
            return;
        }
        if self.stream.is_some() {
            if !self.video_ready && key == Key::Back { self.stop_stream(); }
            return;
        }
        if self.menu.confirming_sign_out || self.device.is_some() || self.sign_in.is_some() {
            match key {
                Key::Back => self.activate(Action::Cancel, 1, window, cx),
                Key::Accept => {
                    let order = deck::order(self);
                    self.activate(order[usize::from(self.menu.browser_cancel).min(order.len() - 1)], 1, window, cx);
                }
                Key::Up | Key::Down | Key::Left | Key::Right => self.menu.browser_cancel = !self.menu.browser_cancel,
                _ => {}
            }
            return;
        }
        if let Some(names) = deck::library_names(self).map(|names| names.into_iter().map(str::to_owned).collect::<Vec<_>>()) {
            let names: Vec<&str> = names.iter().map(String::as_str).collect();
            match key {
                Key::Search => { self.start_search(); return; }
                Key::Back if self.menu.searching || !self.menu.search.is_empty() => { self.leave_search(); return; }
                Key::PreviousLetter | Key::NextLetter if self.menu.search.is_empty() => {
                    let view = deck::library_view(&names, "");
                    let direction = if key == Key::NextLetter { 1 } else { -1 };
                    if let Some(position) = self.menu.library_position() {
                        *position = deck::letter_jump(&names, &view, *position, direction);
                    }
                    return;
                }
                _ => {}
            }
        }
        let actions = deck::setting_actions(self);
        let order = deck::order(self);
        if let Some((action, direction)) = self.menu.navigate(key, &actions, &order) {
            self.activate(action, direction, window, cx);
        }
    }

    fn receive(&mut self, event: Event, window: &mut Window, cx: &mut Context<Self>) {
        // X11 video and its controls are drawn by the media worker. Presenting
        // another GPUI backing frame during playback can flash black in Gamescope.
        let native_video = (self.stream.is_some() && self.video_ready
            || self.preview.as_ref().is_some_and(|preview| preview.ready)) && x11_window(window).is_some();
        let redraw = !native_video || matches!(&event, Event::StreamEnded(_) | Event::PreviewEnded(_));
        match event {
            Event::Menu(key) => {
                let managed = std::env::var_os("ALLY_GAMESCOPE_CONTROL_DISPLAY").is_some();
                if managed || window.is_window_active() { self.menu_key(key, window, cx); }
            }
            Event::Consoles(account, result, auto_connect) => {
                if self.account.as_ref().is_some_and(|current| Arc::ptr_eq(current, &account)) {
                    self.refreshing = false;
                    match result {
                        Ok(consoles) => {
                            self.consoles = consoles;
                            self.menu.console = self.menu.console.min(self.consoles.len().saturating_sub(1));
                            if self.menu.tab == Tab::Xbox && self.stream.is_none() { self.status.clear(); }
                            journal::record("xbox_discovery_completed", json!({"console_count": self.consoles.len()}));
                            if auto_connect && self.menu.tab == Tab::Xbox { self.auto_connect(window); }
                        }
                        Err(error) if self.menu.tab == Tab::Xbox && self.stream.is_none() => self.status = error,
                        Err(_) => {},
                    }
                }
            }
            Event::CachedCloudCatalog(account, games) => {
                if self.account.as_ref().is_some_and(|current| Arc::ptr_eq(current, &account)) {
                    let games = by_name(games, |game| &game.name);
                    self.menu.cloud_game = self.menu.cloud_game.min(games.len().saturating_sub(1));
                    self.cloud_catalog = Some(games);
                }
            }
            Event::CloudCatalog(account, result) => {
                if self.account.as_ref().is_some_and(|current| Arc::ptr_eq(current, &account)) {
                    self.cloud_loading = false;
                    match result {
                        Ok(games) => {
                            let games = by_name(games, |game| &game.name);
                            let previous = (self.menu.tab == Tab::Cloud).then(|| self.selected_index()).flatten()
                                .or(Some(self.menu.cloud_game))
                                .and_then(|index| self.cloud_catalog.as_ref()?.get(index)).map(|game| game.id.clone());
                            let found = previous.and_then(|id| games.iter().position(|item| item.id == id));
                            self.cloud_catalog = Some(games);
                            let count = deck::library_names(self).map_or(0, |names| deck::library_view(&names, &self.menu.search).len());
                            self.menu.cloud_game = found.map(|index| self.view_position(index))
                                .unwrap_or(self.menu.cloud_game).min(count.saturating_sub(1));
                            self.cloud_refreshed_at = Some(Instant::now());
                        }
                        Err(error) => {
                            journal::record("cloud_catalog_failed", json!({}));
                            self.cloud_error = Some(error);
                        }
                    }
                }
            }
            Event::Covers => {}
            Event::CoversDone => self.covers_loading = self.covers_loading.saturating_sub(1),
            Event::PcCatalog(result) => {
                self.pc_loading = false;
                match result {
                    Ok(catalog) => {
                        let open = self.menu.tab == Tab::Pc && self.menu.pc_open.is_some();
                        let previous = open.then(|| self.selected_index()).flatten()
                            .and_then(|index| self.menu.pc_open.as_deref().and_then(|id| self.pc_catalog.apps(id).get(index).map(|game| game.app_id())));
                        self.menu.follow_pcs(&catalog);
                        self.pc_catalog = catalog;
                        let found = previous.and_then(|app| self.menu.pc_open.as_deref()
                            .and_then(|id| self.pc_catalog.apps(id).iter().position(|game| game.app_id() == app)));
                        let count = deck::library_names(self).map_or(0, |names| deck::library_view(&names, &self.menu.search).len());
                        if let Some(found) = found { self.menu.pc_game = self.view_position(found); }
                        self.menu.pc_game = self.menu.pc_game.min(count.saturating_sub(1));
                    }
                    Err(error) => self.pc_error = Some(error),
                }
            }
            Event::PcPaired(pc, result) => {
                journal::record("pc_pairing_ended", json!({"success": result.is_ok()}));
                // An attempt the user left can end while another PC's PIN is shown.
                if self.pc_pairing.as_ref().is_some_and(|(shown, _)| *shown == pc) {
                    self.pc_pairing = None;
                    self.pc_error = result.as_ref().err().cloned();
                }
                if result.is_ok() { self.refresh_pc(); }
            }
            Event::PcChanged(result) => {
                self.status.clear();
                match result {
                    Ok(()) => self.refresh_pc(),
                    Err(error) => self.pc_error = Some(error),
                }
            }
            Event::Device(prompt) => {
                self.menu.browser_cancel = false;
                self.status = "Waiting for Microsoft confirmation".into();
                self.device = Some(prompt);
            }
            Event::Account { xbox } => self.show_account(xbox),
            Event::SignInEnded(result) => {
                journal::record("sign_in_ended", json!({"success": result.is_ok()}));
                self.sign_in = None;
                self.device = None;
                if let Err(error) = result { self.status = error; }
            }
            Event::Stream(media::StreamEvent::VideoReady) => {
                self.video_ready = true;
                self.video_at.get_or_insert_with(Instant::now);
            }
            Event::RetryStream => {
                if let Some(target) = self.retry.take() {
                    let was_windowed = self.restore_windowed;
                    self.start_stream(target, window);
                    self.restore_windowed = was_windowed;
                }
            }
            Event::Preview(media::PreviewEvent::Ready) => {
                if let Some(preview) = &mut self.preview { preview.ready = true; }
            }
            Event::Preview(media::PreviewEvent::Toggle) => self.menu_key(Key::Accept, window, cx),
            Event::Preview(media::PreviewEvent::Back) => self.menu_key(Key::Back, window, cx),
            Event::PreviewEnded(result) => {
                if let Some(preview) = self.preview.take() {
                    if preview.compare && result.is_ok() {
                        let mut prefs = preview.prefs;
                        prefs.lsfg_enabled = !prefs.lsfg_enabled;
                        self.start_preview(prefs, window);
                    }
                    if let Err(error) = result { self.status = error; }
                }
            }
            Event::Stream(media::StreamEvent::Status(status)) => self.status = status,
            Event::Stream(media::StreamEvent::Overlay(open)) => {
                self.overlay_open = open;
                self.overlay_index = 0;
            }
            Event::Stream(media::StreamEvent::OverlayKey(key)) if self.overlay_open => {
                let native = x11_window(window).is_some();
                let count = if native { 5 } else { 4 };
                match key {
                    media::OverlayKey::Left => self.overlay_index = (self.overlay_index + count - 1) % count,
                    media::OverlayKey::Right => self.overlay_index = (self.overlay_index + 1) % count,
                    media::OverlayKey::Activate => self.use_overlay_action(
                        if !native && self.overlay_index >= 2 { self.overlay_index + 1 } else { self.overlay_index }),
                    media::OverlayKey::Back => self.control_stream(media::StreamCommand::CloseOverlay),
                    media::OverlayKey::Select(index) => self.use_overlay_action(index),
                }
                if matches!(key, media::OverlayKey::Left | media::OverlayKey::Right) {
                    journal::record("stream_overlay_focus", json!({"index":self.overlay_index}));
                    self.control_stream(media::StreamCommand::OverlaySelection(self.overlay_index));
                }
            }
            Event::Stream(media::StreamEvent::OverlayKey(_)) => {}
            Event::Stream(media::StreamEvent::Quit) => self.stop_stream(),
            Event::Stream(media::StreamEvent::ToggleStats) => self.control_stream(media::StreamCommand::ToggleDiagnostics),
            Event::StreamEnded(result) => {
                journal::record("stream_ended", json!({"success": result.is_ok()}));
                let had_video = self.video_ready;
                if self.video_at.take().is_some_and(|at| at.elapsed() >= STREAM_STABLE) { self.retries = 0; }
                // A stream the user stopped is not tried again, whatever error came with it.
                let cancelled = self.stream.take().is_some_and(|cancel| cancel.load(Ordering::Relaxed));
                self.stream_commands = None;
                self.overlay_open = false;
                self.video_ready = false;
                if std::mem::take(&mut self.reconnect) {
                    if let Some(target) = self.active_target.take() {
                        // The PC may run something else by now; a reconnect must not end it.
                        let target = match target {
                            media::Target::Pc(game) => media::Target::Pc(game.asking_again()),
                            other => other,
                        };
                        let was_windowed = self.restore_windowed;
                        self.start_stream(target, window);
                        self.restore_windowed = was_windowed;
                        cx.notify();
                        return;
                    }
                }
                // A PC stream that broke off after it played, or a try that
                // failed, is started again: the game still runs on the PC.
                if let (Err(error), Some(media::Target::Pc(game))) = (&result, &self.active_target)
                    && tries_again(cancelled, had_video, self.retries)
                {
                    self.retries += 1;
                    journal::record("stream_retry", json!({"attempt": self.retries, "error": error}));
                    self.retry = Some(media::Target::Pc(game.clone().asking_again()));
                    self.active_target = None;
                    self.status = deck::Text(self.prefs.russian()).t("Связь с ПК прервалась · переподключение", "Lost the PC · reconnecting")
                        .to_string() + &format!(" {}/{STREAM_RETRIES}…", self.retries);
                    let tx = self.tx.clone();
                    thread::spawn(move || { thread::sleep(STREAM_RETRY_PAUSE); let _ = tx.send(Event::RetryStream); });
                    cx.notify();
                    return;
                }
                self.retries = 0;
                self.active_target = None;
                if std::mem::take(&mut self.restore_windowed) && window.is_fullscreen() {
                    window.toggle_fullscreen();
                }
                self.status = result.err().unwrap_or_else(|| "Stream stopped".into());
                // Which app the PC runs has changed.
                if self.menu.tab == Tab::Pc { self.refresh_pc(); }
            }
        }
        if redraw {
            cx.notify();
        }
    }
}

impl Render for RemotePlay {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        deck::render(self, window, cx)
    }
}

fn main() {
    journal::record("app_started", json!({"version": env!("CARGO_PKG_VERSION")}));
    let device = device::current();
    journal::record("control_layout_detected", json!({"layout":device.id,"device":device.name}));
    match std::env::args().nth(1).as_deref() {
        Some("--probe-resolutions") => return run_probe(media::Run::Timed(Duration::from_secs(16))),
        Some("--probe-latency") => return run_probe(media::Run::Latency),
        Some("--probe-transport") => return run_probe(media::Run::Transport),
        Some("--probe-vulkan") => return run_probe(media::Run::Vulkan),
        _ => {}
    }
    let (tx, rx) = mpsc::channel();
    let saved_account = xbox::restore_sign_in().map(|account| account.map(Arc::new));
    application().with_assets(deck::Branding).run(move |cx| {
        init(cx);
        let bounds = Bounds::centered(None, size(px(1120.), px(690.)), cx);
        open_window(WindowOptions {
            titlebar: Some(TitlebarOptions { title: Some("Playwake".into()), ..Default::default() }),
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            ..Default::default()
        }, cx, |window, cx| {
            if let Some(window) = x11_window(window) { media::focus_gamescope(window.xid); }
            cx.new(|cx| {
                cx.observe_window_activation(window, |view: &mut RemotePlay, window, _| {
                    view.control_stream(media::StreamCommand::InputFocus(window.is_window_active()));
                }).detach();
                cx.spawn_in(window, async move |view, cx| loop {
                    cx.background_executor().timer(Duration::from_millis(40)).await;
                    while let Ok(event) = rx.try_recv() {
                        if view.update_in(cx, |view: &mut RemotePlay, window, cx| view.receive(event, window, cx)).is_err() { return; }
                    }
                }).detach();
                let mut view = RemotePlay { tx, account: None, consoles: Vec::new(), sign_in: None,
                    stream: None, stream_commands: None, overlay_open: false, overlay_index: 0, restore_windowed: false,
                    prefs: session_store::load_preferences(), lsfg_missing: None, device: None,
                    menu: Menu { pc_available: cfg!(target_os = "linux"), ..Menu::default() },
                    menu_player: None, keyboard_focus: cx.focus_handle(), library_scroll: ScrollHandle::new(),
                    library_scrolled: Default::default(), covers_loading: 0, active_target: None, preview: None,
                    cloud_catalog: None, cloud_refreshed_at: None, cloud_loading: false, cloud_error: None,
                    pc_catalog: pc::Catalog::default(), pc_loading: false, pc_error: None, pc_pairing: None, pc_replace: None,
                    pc_dialog: None, reconnect: false, retry: None, retries: 0, video_ready: false, video_at: None, refreshing: false,
                    status: if media::runtime_available() { String::new() } else { "GStreamer worker missing".into() } };
                window.focus(&view.keyboard_focus, cx);
                view.check_lsfg();
                view.start_menu_input();
                match saved_account {
                    Ok(Some(xbox)) => view.show_account(xbox),
                    Ok(None) => {}
                    Err(error) => {
                        journal::record("session_restore_failed", json!({}));
                        view.status = error;
                    }
                }
                view
            })
        }).expect("Could not open the GPUI window");
    });
}

fn run_probe(run: media::Run) {
    if let Err(error) = probe(run) {
        journal::record("probe_unavailable", json!({"reason": error}));
        std::process::exit(1);
    }
}

fn probe(run: media::Run) -> xbox::Result<()> {
    use xbox::Resolution::{P720, P1080, P1440};

    let xbox = Arc::new(xbox::restore_sign_in()?.ok_or("Sign in through the app first")?);
    let consoles = xbox.consoles()?;
    let [console] = consoles.as_slice() else {
        return Err("Resolution probe requires exactly one available Xbox".into());
    };
    let resolutions: &[_] = match run {
        media::Run::Latency => &[P720, P1080],
        media::Run::Transport | media::Run::Vulkan => &[P1080],
        _ => &[P720, P1080, P1440],
    };
    for resolution in resolutions {
        let result = media::play(xbox.clone(), media::Target::Home(console.clone()), *resolution, None,
            Arc::new(AtomicBool::new(false)), run, None, |_| {});
        journal::record("probe_stream_ended", json!({
            "requested_height": resolution.dimensions().1,
            "success": result.is_ok(),
        }));
    }
    Ok(())
}

#[cfg(test)]
mod retry_tests {
    use super::{STREAM_RETRIES, tries_again};

    #[test]
    fn a_stream_the_user_stopped_is_not_tried_again() {
        assert!(tries_again(false, true, 0));
        assert!(!tries_again(true, true, 0));
        assert!(!tries_again(true, false, 1));
        // A stream that never played is not retried, and tries run out.
        assert!(!tries_again(false, false, 0));
        assert!(!tries_again(false, true, STREAM_RETRIES));
    }
}
