mod input;
mod journal;
mod lsfg;
mod media;
mod session_store;
mod xbox;

use gpui_kit::component::{button::{Button, ButtonVariants}, label::Label, switch::Switch};
use gpui_kit::*;
use qrcode::{Color, QrCode};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use serde_json::json;
use std::{sync::{Arc, atomic::{AtomicBool, Ordering}, mpsc}, thread, time::Duration};

enum Event {
    Device(DevicePrompt),
    Account { xbox: Arc<xbox::Xbox>, consoles: Vec<xbox::Console> },
    SignInEnded(xbox::Result<()>),
    Stream(media::StreamEvent),
    StreamEnded(xbox::Result<()>),
}

struct DevicePrompt {
    url: String,
    code: String,
    prefilled: bool,
    qr: Option<Arc<QrCode>>,
}

fn login_qr(qr: Arc<QrCode>) -> impl IntoElement {
    let module = px(7.);
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
    resolution: xbox::Resolution,
    lsfg_enabled: bool,
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
    Some(media::VideoWindow { xid, top_inset: (58.0 * window.scale_factor()).ceil() as u32 })
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
                let consoles = xbox.consoles()?;
                let _ = tx.send(Event::Account { xbox, consoles });
                Ok(())
            })();
            let _ = tx.send(Event::SignInEnded(result));
        });
    }

    fn start_stream(&mut self, console: xbox::Console, window: Option<media::VideoWindow>) {
        if self.stream.is_some() || self.sign_in.is_some() { return; }
        let Some(account) = self.account.clone() else { return; };
        let cancel = Arc::new(AtomicBool::new(false));
        self.stream = Some(cancel.clone());
        self.status = "Connecting to Xbox…".into();
        let tx = self.tx.clone();
        let resolution = self.resolution;
        let run = media::Run::Interactive { lsfg: self.lsfg_enabled };
        let (controls, rx) = mpsc::channel();
        self.stream_commands = Some(controls);
        thread::spawn(move || {
            let result = media::play(account, console, resolution, window, cancel, run, Some(rx),
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
            2 => self.stop_stream(),
            _ => {}
        }
    }

    fn show_account(&mut self, xbox: Arc<xbox::Xbox>, consoles: Vec<xbox::Console>) {
        journal::record("xbox_discovery_completed", json!({"console_count": consoles.len()}));
        self.status = if consoles.is_empty() { "No Xbox available for Remote Play".into() } else { "Choose an Xbox".into() };
        self.account = Some(xbox);
        self.consoles = consoles;
    }

    fn receive(&mut self, event: Event) {
        match event {
            Event::Device(prompt) => {
                self.status = "Waiting for Microsoft confirmation".into();
                self.device = Some(prompt);
            }
            Event::Account { xbox, consoles } => self.show_account(xbox, consoles),
            Event::SignInEnded(result) => {
                journal::record("sign_in_ended", json!({"success": result.is_ok()}));
                self.sign_in = None;
                self.device = None;
                if let Err(error) = result { self.status = error; }
            }
            Event::Stream(media::StreamEvent::Status(status)) => self.status = status,
            Event::Stream(media::StreamEvent::Overlay(open)) => {
                self.overlay_open = open;
                self.overlay_index = 0;
            }
            Event::Stream(media::StreamEvent::OverlayKey(key)) if self.overlay_open => match key {
                media::OverlayKey::Left => self.overlay_index = (self.overlay_index + 2) % 3,
                media::OverlayKey::Right => self.overlay_index = (self.overlay_index + 1) % 3,
                media::OverlayKey::Activate => self.use_overlay_action(self.overlay_index),
                media::OverlayKey::Back => self.control_stream(media::StreamCommand::CloseOverlay),
            },
            Event::Stream(media::StreamEvent::OverlayKey(_)) => {}
            Event::StreamEnded(result) => {
                journal::record("stream_ended", json!({"success": result.is_ok()}));
                self.stream = None;
                self.stream_commands = None;
                self.overlay_open = false;
                self.status = result.err().unwrap_or_else(|| "Stream stopped".into());
            }
        }
    }
}

impl Render for RemotePlay {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let sign_button = Button::new("sign-in").primary().label("Sign in with Microsoft")
            .on_click(cx.listener(|view, _, _, cx| { view.start_login(); cx.notify(); }));
        let mut header = div().h(px(58.)).px_4().flex().items_center().justify_between()
            .child(Label::new(self.status.clone()));
        if self.stream.is_some() && self.overlay_open {
            let mut actions = div().flex().gap_2();
            for (index, label) in ["Resume", "Xbox Guide", "Stop stream"].into_iter().enumerate() {
                let mut button = Button::new(("overlay-action", index)).label(label);
                if index == self.overlay_index { button = button.primary(); }
                actions = actions.child(button.on_click(cx.listener(move |view, _, _, cx| {
                    view.use_overlay_action(index);
                    cx.notify();
                })));
            }
            header = header.child(actions);
        }
        else if self.stream.is_some() {
            header = header.child(Button::new("open-stream-overlay").label("Stream menu")
                .on_click(cx.listener(|view, _, _, _| view.control_stream(media::StreamCommand::OpenOverlay))));
        }
        else if self.account.is_some() {
            header = header.child(Button::new("sign-out").label("Sign out")
                .on_click(cx.listener(|view, _, _, cx| {
                    if session_store::clear().is_ok() {
                        view.account = None;
                        view.consoles.clear();
                        view.status = "Signed out".into();
                        journal::record("signed_out", json!({}));
                    } else {
                        view.status = "Could not remove saved sign-in".into();
                    }
                    cx.notify();
                })));
        } else if self.sign_in.is_none() { header = header.child(sign_button); }
        let mut content = div().flex_1().p_4().flex().flex_col().gap_3().bg(rgb(0x13161a)).text_color(rgb(0xf5f5f5));
        if let Some(prompt) = &self.device {
            let url = prompt.url.clone();
            if let Some(qr) = &prompt.qr {
                let instructions = if prompt.prefilled { "Scan with your phone or tablet to confirm sign-in." }
                    else { "Scan with your phone or tablet, then enter the code below." };
                content = content.child(div().child(instructions)).child(login_qr(qr.clone()));
            }
            content = content
                .child(div().text_xl().child(format!("Code: {}", prompt.code)))
                .child(Button::new("open-microsoft").primary().label("Open Microsoft")
                    .on_click(move |_, _, cx| cx.open_url(&url)))
                .child(Button::new("cancel-login").label("Cancel")
                    .on_click(cx.listener(|view, _, _, _| { if let Some(cancel) = &view.sign_in { cancel.store(true, Ordering::Relaxed); } })));
        }
        if self.stream.is_none() {
            if cfg!(target_os = "linux") && x11_window(window).is_some() {
                content = content.child(Switch::new("lsfg")
                    .label("LSFG 2× Performance · Flow Scale 1.0")
                    .checked(self.lsfg_enabled)
                    .on_change(cx.listener(|view, enabled: &bool, _, cx| {
                        if *enabled {
                            if let Err(error) = lsfg::Runtime::discover() {
                                view.status = error;
                                cx.notify();
                                return;
                            }
                        }
                        view.lsfg_enabled = *enabled;
                        if let Err(error) = session_store::save_lsfg(*enabled) {
                            view.status = "Could not save the LSFG setting".into();
                            journal::record("lsfg_setting_save_failed", json!({"kind": format!("{:?}", error.kind())}));
                        }
                        journal::record("lsfg_setting_changed", json!({"enabled": enabled}));
                        cx.notify();
                    })));
            }
            if !self.consoles.is_empty() {
                let mut choices = div().flex().items_center().gap_2().child("Requested resolution");
                for (index, (resolution, label)) in xbox::Resolution::CHOICES.into_iter().enumerate() {
                    let mut button = Button::new(("resolution", index)).label(label);
                    if self.resolution == resolution { button = button.primary(); }
                    choices = choices.child(button.on_click(cx.listener(move |view, _, _, cx| {
                        view.resolution = resolution;
                        let (width, height) = resolution.dimensions();
                        journal::record("resolution_selected", json!({"width": width, "height": height}));
                        cx.notify();
                    })));
                }
                content = content.child(choices);
            }
            for (index, console) in self.consoles.iter().enumerate() {
                let name = if console.name.is_empty() { console.model.clone() } else { console.name.clone() };
                let label = format!("{name} · {} · {}", console.power, console.id);
                let selected = console.clone();
                content = content.child(Button::new(("console", index)).label(label)
                    .on_click(cx.listener(move |view, _, window, cx| {
                        view.start_stream(selected.clone(), x11_window(window));
                        cx.notify();
                    })));
            }
        }
        div().size_full().flex().flex_col().child(header).child(content)
    }
}

fn main() {
    journal::record("app_started", json!({"version": env!("CARGO_PKG_VERSION")}));
    match std::env::args().nth(1).as_deref() {
        Some("--probe-resolutions") => return run_probe(media::Run::Timed(Duration::from_secs(16))),
        Some("--probe-latency") => return run_probe(media::Run::Latency),
        Some("--probe-transport") => return run_probe(media::Run::Transport),
        Some("--probe-vulkan") => return run_probe(media::Run::Vulkan),
        _ => {}
    }
    let (tx, rx) = mpsc::channel();
    let saved_account = xbox::restore_sign_in().and_then(|account| account.map(|xbox| {
        let consoles = xbox.consoles()?;
        Ok((Arc::new(xbox), consoles))
    }).transpose());
    application().with_assets(assets::Assets).run(move |cx| {
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
                cx.spawn(async move |view, cx| loop {
                    cx.background_executor().timer(Duration::from_millis(120)).await;
                    while let Ok(event) = rx.try_recv() {
                        if view.update(cx, |view: &mut RemotePlay, cx| { view.receive(event); cx.notify(); }).is_err() { return; }
                        cx.refresh();
                    }
                }).detach();
                let mut view = RemotePlay { tx, account: None, consoles: Vec::new(), sign_in: None,
                    stream: None, stream_commands: None, overlay_open: false, overlay_index: 0,
                    resolution: xbox::Resolution::default(), device: None,
                    lsfg_enabled: session_store::load_lsfg(),
                    status: if media::runtime_available() { "Playwake".into() } else { "GStreamer worker missing".into() } };
                match saved_account {
                    Ok(Some((xbox, consoles))) => view.show_account(xbox, consoles),
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
    use xbox::Resolution::{P720, P800, P900, P1080};

    let xbox = Arc::new(xbox::restore_sign_in()?.ok_or("Sign in through the app first")?);
    let consoles = xbox.consoles()?;
    let [console] = consoles.as_slice() else {
        return Err("Resolution probe requires exactly one available Xbox".into());
    };
    let resolutions: &[_] = match run {
        media::Run::Latency => &[P720, P1080],
        media::Run::Transport | media::Run::Vulkan => &[P1080],
        _ => &[P720, P800, P900],
    };
    for resolution in resolutions {
        let result = media::play(xbox.clone(), console.clone(), *resolution, None,
            Arc::new(AtomicBool::new(false)), run, None, |_| {});
        journal::record("probe_stream_ended", json!({
            "requested_height": resolution.dimensions().1,
            "success": result.is_ok(),
        }));
    }
    Ok(())
}
