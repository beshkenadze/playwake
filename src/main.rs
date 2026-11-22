mod cloud;
mod deck;
mod device;
mod input;
mod journal;
mod lsfg;
mod media;
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
    Menu(Key),
    Consoles(Arc<xbox::Xbox>, xbox::Result<Vec<xbox::Console>>, bool),
    CachedCloudCatalog(Arc<xbox::Xbox>, Vec<cloud::Game>),
    CloudCatalog(Arc<xbox::Xbox>, xbox::Result<Vec<cloud::Game>>),
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
    menu: Menu,
    menu_player: Option<media::Player>,
    keyboard_focus: FocusHandle,
    active_target: Option<media::Target>,
    cloud_catalog: Option<Vec<cloud::Game>>,
    cloud_refreshed_at: Option<Instant>,
    cloud_loading: bool,
    cloud_error: Option<String>,
    reconnect: bool,
    video_ready: bool,
    refreshing: bool,
    device: Option<DevicePrompt>,
    status: String,
}

impl Drop for RemotePlay {
    fn drop(&mut self) {
        if let Some(cancel) = &self.sign_in { cancel.store(true, Ordering::Relaxed); }
        if let Some(cancel) = &self.stream { cancel.store(true, Ordering::Relaxed); }
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
        if self.stream.is_some() || self.sign_in.is_some() { return; }
        let Some(account) = self.account.clone() else { return; };
        let native_window = x11_window(window);
        let streaming_tab = if matches!(&target, media::Target::Home(_)) { Tab::Xbox } else { Tab::Cloud };
        self.active_target = Some(target.clone());
        self.video_ready = false;
        self.menu.select_tab(streaming_tab);
        self.restore_windowed = native_window.is_some() && !window.is_fullscreen();
        if self.restore_windowed { window.toggle_fullscreen(); }
        let cancel = Arc::new(AtomicBool::new(false));
        self.stream = Some(cancel.clone());
        self.status = "Connecting to Xbox…".into();
        let tx = self.tx.clone();
        let resolution = self.resolution();
        let mut prefs = self.prefs;
        if native_window.is_none() { prefs.lsfg_enabled = false; }
        let run = media::Run::Interactive(prefs);
        let (controls, rx) = mpsc::channel();
        self.stream_commands = Some(controls);
        thread::spawn(move || {
            let result = media::play(account, target, resolution, native_window, cancel, run, Some(rx),
                |event| { let _ = tx.send(Event::Stream(event)); });
            let _ = tx.send(Event::StreamEnded(result));
        });
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
        thread::spawn(move || {
            if read_cache && let Some(games) = cloud::cached_catalog(&account) {
                let _ = tx.send(Event::CachedCloudCatalog(account.clone(), games));
            }
            let result = cloud::catalog(&account);
            let _ = tx.send(Event::CloudCatalog(account, result));
        });
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

    fn activate(&mut self, action: Action, direction: isize, window: &mut Window, cx: &mut Context<Self>) {
        journal::record("deck_action", json!({"action": format!("{action:?}")}));
        let t = deck::Text(self.prefs.russian());
        let save = match action {
            Action::Settings | Action::Home => {
                self.menu.select_tab(if action == Action::Settings { Tab::Settings } else { Tab::Xbox });
                false
            }
            Action::Cloud => {
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
            Action::CloudGame(index) => {
                if let Some(game) = self.cloud_catalog.as_ref().and_then(|catalog| catalog.get(index)) {
                    self.menu.cloud_game = index;
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
                if self.menu.tab == Tab::Cloud { self.refresh_cloud(); } else { self.refresh_consoles(false); }
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
                    self.menu = Menu::default();
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
        let actions = deck::setting_actions(self);
        let order = deck::order(self);
        if let Some((action, direction)) = self.menu.navigate(key, &actions, &order) {
            self.activate(action, direction, window, cx);
        }
    }

    fn receive(&mut self, event: Event, window: &mut Window, cx: &mut Context<Self>) {
        // X11 video and its controls are drawn by the media worker. Presenting
        // another GPUI backing frame during playback can flash black in Gamescope.
        let native_video = self.stream.is_some() && self.video_ready && x11_window(window).is_some();
        let redraw = !native_video || matches!(&event, Event::StreamEnded(_));
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
                    self.menu.cloud_game = self.menu.cloud_game.min(games.len().saturating_sub(1));
                    self.cloud_catalog = Some(games);
                }
            }
            Event::CloudCatalog(account, result) => {
                if self.account.as_ref().is_some_and(|current| Arc::ptr_eq(current, &account)) {
                    self.cloud_loading = false;
                    match result {
                        Ok(games) => {
                            let previous = self.cloud_catalog.as_ref().and_then(|list| list.get(self.menu.cloud_game));
                            self.menu.cloud_game = previous.and_then(|game| games.iter().position(|item| item.id == game.id))
                                .unwrap_or_else(|| self.menu.cloud_game.min(games.len().saturating_sub(1)));
                            self.cloud_catalog = Some(games);
                            self.cloud_refreshed_at = Some(Instant::now());
                        }
                        Err(error) => {
                            journal::record("cloud_catalog_failed", json!({}));
                            self.cloud_error = Some(error);
                        }
                    }
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
            Event::Stream(media::StreamEvent::VideoReady) => self.video_ready = true,
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
            Event::StreamEnded(result) => {
                journal::record("stream_ended", json!({"success": result.is_ok()}));
                self.stream = None;
                self.stream_commands = None;
                self.overlay_open = false;
                self.video_ready = false;
                if std::mem::take(&mut self.reconnect) {
                    if let Some(target) = self.active_target.take() {
                        let was_windowed = self.restore_windowed;
                        self.start_stream(target, window);
                        self.restore_windowed = was_windowed;
                        cx.notify();
                        return;
                    }
                }
                self.active_target = None;
                if std::mem::take(&mut self.restore_windowed) && window.is_fullscreen() {
                    window.toggle_fullscreen();
                }
                self.status = result.err().unwrap_or_else(|| "Stream stopped".into());
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
                    prefs: session_store::load_preferences(), device: None, menu: Menu::default(),
                    menu_player: None, keyboard_focus: cx.focus_handle(), active_target: None,
                    cloud_catalog: None, cloud_refreshed_at: None, cloud_loading: false, cloud_error: None,
                    reconnect: false, video_ready: false, refreshing: false,
                    status: if media::runtime_available() { String::new() } else { "GStreamer worker missing".into() } };
                window.focus(&view.keyboard_focus, cx);
                if cfg!(target_os = "linux") {
                    let input_events = view.tx.clone();
                    view.menu_player = media::menu_input(move |key| { let _ = input_events.send(Event::Menu(key)); }).ok();
                }
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
