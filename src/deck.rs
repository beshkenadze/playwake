use super::{DevicePrompt, RemotePlay, Window, assets, login_qr};
use crate::session_store::{Language as Locale, Preferences};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::borrow::Cow;

pub struct Branding;

impl AssetSource for Branding {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        let data: &[u8] = match path {
            "branding/icon.png" => include_bytes!("../assets/branding/icon.png"),
            "branding/hero.png" => include_bytes!("../assets/branding/steam-hero.png"),
            _ => return assets::Assets.load(path),
        };
        Ok(Some(Cow::Borrowed(data)))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        assets::Assets.list(path)
    }
}

/// Tells a new confirming press from the one that opened a dialog. A held
/// key repeats under X11 as fresh presses, so a press counts only after a
/// second without one.
pub struct Confirm(std::time::Instant);

impl Confirm {
    pub fn opened(now: std::time::Instant) -> Self {
        Self(now)
    }

    pub fn press(&mut self, now: std::time::Instant) -> bool {
        let deliberate = now.duration_since(self.0) >= std::time::Duration::from_secs(1);
        self.0 = now;
        deliberate
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Key {
    Up,
    Down,
    Left,
    Right,
    Accept,
    Back,
    Settings,
    Refresh,
    PreviousTab,
    NextTab,
}

impl Key {
    pub fn parse(key: &str) -> Option<Self> {
        Some(match key {
            "up" => Self::Up,
            "down" => Self::Down,
            "left" => Self::Left,
            "right" => Self::Right,
            "accept" | "enter" | "return" => Self::Accept,
            "back" | "escape" => Self::Back,
            "settings" => Self::Settings,
            "refresh" => Self::Refresh,
            "previous-tab" => Self::PreviousTab,
            "next-tab" => Self::NextTab,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Action {
    Settings,
    Home,
    Cloud,
    Play(usize),
    CloudGame(usize),
    Pc,
    PcGame(usize),
    SignIn,
    Browser,
    Cancel,
    Section(usize),
    AutoConnect,
    Refresh,
    SignOut,
    ConfirmSignOut,
    Resolution,
    Bitrate,
    Jitter,
    Upscale,
    PcHdr,
    Lsfg,
    LsfgPerformance,
    LsfgFlowScale,
    LsfgPreview,
    Compact,
    Language,
    Palette,
    Volume,
    Mute,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Tab {
    #[default]
    Xbox,
    Cloud,
    Pc,
    Settings,
}

impl Tab {
    fn action(self) -> Action {
        match self {
            Self::Xbox => Action::Home,
            Self::Cloud => Action::Cloud,
            Self::Pc => Action::Pc,
            Self::Settings => Action::Settings,
        }
    }

    /// The PC tab is skipped where Moonlight cannot be run.
    fn step(self, offset: isize, pc: bool) -> Self {
        let tabs = [Self::Xbox, Self::Cloud, Self::Pc, Self::Settings];
        let tabs: Vec<Self> = tabs.into_iter().filter(|tab| pc || *tab != Self::Pc).collect();
        let current = tabs.iter().position(|tab| *tab == self).unwrap_or(0) as isize;
        tabs[(current + offset).rem_euclid(tabs.len() as isize) as usize]
    }
}

const LIBRARY_COLUMNS: usize = 3;
const LIBRARY_PAGE: usize = 9;

#[derive(Default)]
pub struct Menu {
    pub tab: Tab,
    pub focused_tab: Option<Tab>,
    pub section: usize,
    pub row: Option<usize>,
    pub editing_value: bool,
    pub console: usize,
    pub cloud_game: usize,
    pub pc_game: usize,
    pub pc_available: bool,
    pub browser_cancel: bool,
    pub confirming_sign_out: bool,
}

impl Menu {
    pub fn select_tab(&mut self, tab: Tab) {
        self.tab = tab;
        self.focused_tab = None;
        self.row = None;
        self.editing_value = false;
    }

    pub fn select_section(&mut self, index: usize) {
        self.section = index;
        self.focused_tab = None;
        self.row = None;
        self.editing_value = false;
    }

    pub fn navigate(
        &mut self,
        key: Key,
        actions: &[Action],
        consoles: &[Action],
    ) -> Option<(Action, isize)> {
        use Key::*;
        let run = |action| Some((action, 1));
        let other_tab = if self.tab == Tab::Xbox {
            Tab::Settings
        } else {
            Tab::Xbox
        };
        match key {
            PreviousTab => return run(self.tab.step(-1, self.pc_available).action()),
            NextTab => return run(self.tab.step(1, self.pc_available).action()),
            Settings => return run(other_tab.action()),
            Refresh => return run(Action::Refresh),
            _ => {}
        }

        if let Some(tab) = self.focused_tab {
            match key {
                Left => self.focused_tab = Some(tab.step(-1, self.pc_available)),
                Right => self.focused_tab = Some(tab.step(1, self.pc_available)),
                Accept => return run(tab.action()),
                Down | Back => self.focused_tab = None,
                _ => {}
            }
            return None;
        }

        let library = match self.tab {
            Tab::Cloud => Some(&mut self.cloud_game),
            Tab::Pc => Some(&mut self.pc_game),
            _ => None,
        };
        if let Some(game) = library {
            let last = consoles.len().saturating_sub(1);
            match key {
                Up if *game < LIBRARY_COLUMNS => self.focused_tab = Some(self.tab),
                Up => *game -= LIBRARY_COLUMNS,
                Down => *game = (*game + LIBRARY_COLUMNS).min(last),
                Left => *game = game.saturating_sub(1),
                Right => *game = (*game + 1).min(last),
                Accept => return consoles.get((*game).min(last)).copied().and_then(run),
                Back => return run(Action::Home),
                _ => {}
            }
            return None;
        }

        if self.tab == Tab::Xbox {
            let last = consoles.len().saturating_sub(1);
            match key {
                Up => self.focused_tab = Some(self.tab),
                Left => self.console = self.console.saturating_sub(1),
                Right => self.console = self.console.saturating_add(1).min(last),
                Accept => return consoles.get(self.console.min(last)).copied().and_then(run),
                _ => {}
            }
            return None;
        }

        if let Some(row) = self.row.filter(|row| *row < actions.len()) {
            let action = actions[row];
            if self.editing_value {
                match key {
                    Left => return Some((action, -1)),
                    Right => return run(action),
                    Accept | Back => {
                        self.editing_value = false;
                        return None;
                    }
                    Up | Down => self.editing_value = false,
                    _ => {}
                }
            }
            match key {
                Left | Back => self.row = None,
                Up if row == 0 => self.focused_tab = Some(self.tab),
                Up => self.row = Some(row - 1),
                Down if row + 1 < actions.len() => self.row = Some(row + 1),
                Accept if action.adjustable() => self.editing_value = true,
                Accept => return run(action),
                _ => {}
            }
        } else {
            match key {
                Back => return run(Action::Home),
                Up if self.section == 0 => self.focused_tab = Some(self.tab),
                Up => self.section -= 1,
                Down if self.section + 1 < section_labels(&Text(false)).len() => self.section += 1,
                Right | Accept if !actions.is_empty() => self.row = Some(0),
                _ => {}
            }
        }
        None
    }
}

impl Action {
    fn adjustable(self) -> bool {
        matches!(
            self,
            Self::Resolution | Self::Bitrate | Self::Jitter | Self::Volume | Self::Language | Self::Palette
        )
    }
}

pub struct Text(pub bool);

impl Text {
    pub fn t<'a>(&self, ru: &'a str, en: &'a str) -> &'a str {
        if self.0 { ru } else { en }
    }
}

const CANVAS: u32 = 0x161818;
const PANEL: u32 = 0x1f1f21;
const CARD: u32 = 0x2c2c2e;
const LINE: u32 = 0x393b3b;
const TEXT: u32 = 0xececec;
const MUTED: u32 = 0xa1a1a6;

fn colors(prefs: Preferences) -> (u32, u32) {
    if prefs.xbox_palette {
        (0x6cc04a, 0x107c10)
    } else {
        (0x8ddcb3, 0x2a7a58)
    }
}

pub fn section_labels(t: &Text) -> [&'static str; 7] {
    [
        "Xbox",
        t.t("Видео", "Video"),
        t.t("Плавность", "Smoothness"),
        t.t("Звук", "Sound"),
        t.t("Управление", "Controls"),
        t.t("Диагностика", "Diagnostics"),
        t.t("Интерфейс", "Tweaks"),
    ]
}

struct Row {
    label: &'static str,
    hint: &'static str,
    value: String,
    action: Option<Action>,
}

impl Row {
    fn new(
        label: &'static str,
        hint: &'static str,
        value: impl Into<String>,
        action: Option<Action>,
    ) -> Self {
        Self {
            label,
            hint,
            value: value.into(),
            action,
        }
    }
}

fn rows(p: Preferences, t: &Text, signed_in: bool, count: usize, section: usize) -> Vec<Row> {
    use self::Action::*;
    let on = |value| {
        if value {
            t.t("Вкл.", "On")
        } else {
            t.t("Выкл.", "Off")
        }
    };
    match section {
        0 => vec![
            Row::new(
                t.t("Аккаунт Microsoft", "Microsoft account"),
                "",
                if signed_in {
                    t.t("Выйти", "Sign out")
                } else {
                    t.t("Войти", "Sign in")
                },
                Some(if signed_in { SignOut } else { SignIn }),
            ),
            Row::new(
                t.t("Автозапуск", "Auto-connect"),
                t.t(
                    "Автоматически подключаться, если доступна одна Xbox",
                    "Connect automatically when one Xbox is available",
                ),
                on(p.auto_connect),
                Some(AutoConnect),
            ),
            Row::new(
                t.t("Консоли в аккаунте", "Consoles on account"),
                "",
                count.to_string(),
                signed_in.then_some(Refresh),
            ),
        ],
        1 => vec![
            Row::new(
                t.t("Запрашиваемое разрешение", "Requested resolution"),
                t.t(
                    "Фактический размер потока выбирает Xbox",
                    "Xbox picks the actual stream size",
                ),
                format!("‹  {}p  ›", p.height),
                Some(Resolution),
            ),
            Row::new(
                t.t("Лимит битрейта", "Bitrate limit"),
                "",
                format!("‹  {} {}  ›", p.bitrate_mbps, t.t("Мбит/с", "Mbps")),
                Some(Bitrate),
            ),
            Row::new(
                t.t("Буфер джиттера", "Jitter buffer"),
                t.t("Меньше — ниже задержка, больше — плавнее на Wi-Fi · со следующего подключения",
                    "Lower cuts latency, higher smooths Wi-Fi · applies on reconnect"),
                format!("‹  {} {}  ›", p.jitter_ms, t.t("мс", "ms")),
                Some(Jitter),
            ),
            Row::new(
                t.t("HDR со стрима ПК", "HDR from a PC"),
                t.t("На HDR-экране, если ПК кодирует HDR · со следующего подключения",
                    "On an HDR screen when the PC encodes HDR · applies on reconnect"),
                on(p.pc_hdr),
                Some(PcHdr),
            ),
            Row::new(
                t.t("Апскейлинг", "Upscaling"),
                t.t("Для видео меньше экрана · со следующего подключения",
                    "For video smaller than the screen · applies on reconnect"),
                if p.lanczos_upscale { "Lanczos" } else { t.t("Вписать", "Fit") },
                Some(Upscale),
            ),
            Row::new(
                t.t("Полноэкранный режим", "Fullscreen"),
                "",
                t.t("Во время стрима", "While streaming"),
                None,
            ),
        ],
        2 => vec![
            Row::new(
                "LSFG 2×",
                t.t(
                    "Применяется при следующем подключении",
                    "Applies on the next connection",
                ),
                on(p.lsfg_enabled),
                Some(Lsfg),
            ),
            Row::new(
                t.t("Режим LSFG", "LSFG mode"),
                "",
                if p.lsfg_performance { t.t("Производительность", "Performance") }
                else { t.t("Качество", "Quality") },
                Some(LsfgPerformance),
            ),
            Row::new("Flow Scale", "", p.lsfg_flow_scale(), Some(LsfgFlowScale)),
            Row::new(
                t.t("Предпросмотр LSFG", "LSFG preview"),
                t.t("Сравнить движение и FPS вывода", "Compare motion and presentation FPS"),
                t.t("Открыть", "Open"),
                Some(LsfgPreview),
            ),
        ],
        3 => vec![
            Row::new(
                t.t("Громкость стрима", "Stream volume"),
                t.t(
                    "Применяется при следующем подключении",
                    "Applies on the next connection",
                ),
                format!("‹  {}%  ›", p.volume),
                Some(Volume),
            ),
            Row::new(t.t("Без звука", "Mute"), "", on(p.mute), Some(Mute)),
            Row::new(
                t.t("Устройство вывода", "Output device"),
                "",
                t.t("Как в системе", "System default"),
                None,
            ),
        ],
        4 => vec![
            Row::new(
                t.t("Открыть оверлей", "Open stream overlay"),
                t.t(
                    "Коротко нажмите обе кнопки одновременно",
                    "Tap both buttons together",
                ),
                "View + Menu",
                None,
            ),
            Row::new(
                t.t("Схема кнопок", "Button layout"),
                "",
                if crate::device::current().id == "gamepad" {
                    t.t("Геймпад", "Gamepad")
                } else {
                    crate::device::current().name
                },
                None,
            ),
            Row::new(
                "Xbox Guide",
                "",
                t.t("В оверлее стрима", "In stream overlay"),
                None,
            ),
        ],
        5 => vec![
            Row::new(
                t.t(
                    "Компактная статистика в стриме",
                    "Compact stream statistics",
                ),
                "",
                on(p.compact_stats),
                Some(Compact),
            ),
            Row::new(
                t.t("Показатели", "Metrics"),
                "",
                t.t(
                    "FPS · битрейт · RTT · пропуски",
                    "FPS · bitrate · RTT · drops",
                ),
                None,
            ),
        ],
        _ => vec![
            Row::new(
                t.t("Язык интерфейса", "Interface language"),
                "",
                match p.language {
                    Locale::System => t.t("Как в системе", "System"),
                    Locale::English => "English",
                    Locale::Russian => "Русский",
                },
                Some(Language),
            ),
            Row::new(
                t.t("Палитра", "Palette"),
                "",
                if p.xbox_palette { "Xbox" } else { "Playwake" },
                Some(Palette),
            ),
        ],
    }
}

pub fn setting_actions(v: &RemotePlay) -> Vec<Action> {
    rows(
        v.prefs,
        &Text(v.prefs.russian()),
        v.account.is_some(),
        v.consoles.len(),
        v.menu.section,
    )
    .into_iter()
    .filter_map(|row| row.action)
    .collect()
}

pub fn order(v: &RemotePlay) -> Vec<Action> {
    if v.menu.confirming_sign_out {
        return vec![Action::Cancel, Action::ConfirmSignOut];
    }
    if v.device.is_some() {
        return vec![Action::Browser, Action::Cancel];
    }
    if v.sign_in.is_some() {
        return vec![Action::Cancel];
    }
    if v.menu.tab == Tab::Pc {
        let tiles = v.pc_catalog.games.len() + v.pc_catalog.unpaired.len();
        return if tiles == 0 { vec![Action::Refresh] } else { (0..tiles).map(Action::PcGame).collect() };
    }
    if v.account.is_none() {
        return vec![Action::SignIn];
    }
    if v.menu.tab == Tab::Cloud {
        return match &v.cloud_catalog {
            Some(catalog) if !catalog.is_empty() => (0..catalog.len()).map(Action::CloudGame).collect(),
            _ => vec![Action::Refresh],
        };
    }
    if v.consoles.is_empty() {
        return vec![Action::Refresh];
    }
    (0..v.consoles.len()).map(Action::Play).collect()
}

fn action(
    id: (&'static str, usize),
    child: impl IntoElement,
    command: Action,
    active: bool,
    accent: u32,
    unit: f32,
    cx: &mut Context<RemotePlay>,
) -> Stateful<Div> {
    div()
        .id(id)
        .border(px(2. * unit))
        .border_color(rgb(if active { accent } else { LINE }))
        .cursor_pointer()
        .rounded(px(12. * unit))
        .child(child)
        .hover(|style| style.border_color(rgb(accent)))
        .on_click(cx.listener(move |view, _, win, cx| {
            view.activate(command, 1, win, cx);
            cx.notify();
        }))
}

/// A dialog's choice: the gamepad hint, which a tap or click takes too.
fn dialog_choice(id: &'static str, label: &'static str, key: Key, unit: f32, cx: &mut Context<RemotePlay>) -> Stateful<Div> {
    div().id(id).px(px(18. * unit)).py(px(10. * unit)).cursor_pointer().child(label)
        .on_click(cx.listener(move |view, _, window, cx| { view.menu_key(key, window, cx); cx.notify(); }))
}

pub fn render(v: &mut RemotePlay, window: &mut Window, cx: &mut Context<RemotePlay>) -> AnyElement {
    let viewport = window.viewport_size();
    let unit = (f32::from(viewport.width) / 1280.)
        .min(f32::from(viewport.height) / 700.)
        .max(0.2);
    let s = |p| px(p * unit);
    let t = Text(v.prefs.russian());
    let (accent, primary) = colors(v.prefs);
    if let Some(preview) = &v.preview {
        return div().id("lsfg-preview").size_full().bg(rgb(0)).text_color(rgb(TEXT))
            .flex().items_center().justify_center().track_focus(&v.keyboard_focus)
            .when(!preview.ready, |el| el.child(t.t("Открывается превью… · B Назад", "Starting preview… · B Back")))
            .into_any_element();
    }
    if let Some((game, running, _)) = &v.pc_replace {
        return div().id("pc-replace").size_full().bg(rgb(CANVAS)).text_color(rgb(TEXT)).track_focus(&v.keyboard_focus)
            .flex().flex_col().items_center().justify_center().gap(s(18.))
            .on_key_down(cx.listener(|view, ev: &KeyDownEvent, window, cx| {
                if let Some(key) = Key::parse(ev.keystroke.key.as_str()) { view.menu_key(key, window, cx); cx.notify(); }
            }))
            .child(div().text_size(s(30.)).child(format!("{} {}", t.t("На ПК запущено:", "The PC is running:"), running.name)))
            .child(div().max_w(s(820.)).text_center().text_color(rgb(MUTED)).child(format!("{} {} {}",
                t.t("Запуск", "Starting"), game.name,
                t.t("завершит его. Несохранённый прогресс в игре может пропасть.",
                    "will end it. Unsaved progress in a game can be lost."))))
            .child(div().flex().gap(s(24.)).text_color(rgb(accent))
                .child(dialog_choice("pc-replace-accept", t.t("A Завершить и запустить", "A End it and start"), Key::Accept, unit, cx))
                .child(dialog_choice("pc-replace-cancel", t.t("B Отмена", "B Cancel"), Key::Back, unit, cx)))
            .into_any_element();
    }
    if let Some((pc, pin)) = &v.pc_pairing {
        return div().id("pc-pairing").size_full().bg(rgb(CANVAS)).text_color(rgb(TEXT)).track_focus(&v.keyboard_focus)
            .flex().flex_col().items_center().justify_center().gap(s(18.))
            .child(div().text_size(s(30.)).child(format!("{} {}", t.t("Сопряжение с", "Pairing with"), pc.name)))
            .child(div().text_size(s(96.)).font_weight(FontWeight::SEMIBOLD).text_color(rgb(accent)).child(pin.clone()))
            .child(div().max_w(s(760.)).text_center().text_color(rgb(MUTED)).child(format!("{} https://{}:47990/pin",
                t.t("Введите этот PIN в веб-интерфейсе Sunshine:", "Enter this PIN in the Sunshine web UI:"), pc.address)))
            .child(div().text_color(rgb(MUTED)).child(dialog_choice("pc-pairing-cancel", t.t("B Отмена", "B Cancel"), Key::Back, unit, cx)))
            .into_any_element();
    }
    let native_stream = v.stream.is_some() && super::x11_window(window).is_some();
    if native_stream && v.video_ready {
        // The video window covers this one; it still takes what is typed.
        return div().id("stream").size_full().bg(rgb(0)).track_focus(&v.keyboard_focus)
            .on_key_down(cx.listener(|view, ev: &KeyDownEvent, _, _| { view.type_into_stream(&ev.keystroke); }))
            .into_any_element();
    }
    let tabs_available = v.stream.is_none()
        && v.sign_in.is_none()
        && v.device.is_none()
        && !v.menu.confirming_sign_out;
    let mut tab = |label: &'static str, target: Tab, index: usize| {
        let selected = v.menu.tab == target;
        let focused = v.menu.focused_tab == Some(target);
        let face = div()
            .flex()
            .items_center()
            .px(s(16.))
            .h(s(38.))
            .rounded(s(9.))
            .bg(rgb(if selected { accent } else { PANEL }))
            .text_color(rgb(if selected { CANVAS } else { TEXT }))
            .font_weight(if selected {
                FontWeight::SEMIBOLD
            } else {
                FontWeight::NORMAL
            })
            .child(label);
        action(
            ("nav", index),
            face,
            target.action(),
            focused,
            if selected { TEXT } else { accent },
            unit,
            cx,
        )
    };
    let home = tab("Remote Play", Tab::Xbox, 0);
    let cloud = tab("Xbox Cloud", Tab::Cloud, 1);
    let pc = tab(t.t("ПК", "PC"), Tab::Pc, 3);
    let settings = tab(t.t("Настройки", "Settings"), Tab::Settings, 2);
    let top = div()
        .h(s(72.))
        .flex_none()
        .px(s(32.))
        .flex()
        .items_center()
        .gap(s(16.))
        .bg(rgb(PANEL))
        .border_b_1()
        .border_color(rgb(LINE))
        .child(img("branding/icon.png").w(s(38.)).h(s(38.)).rounded(s(8.)))
        .child(
            div()
                .font_weight(FontWeight::SEMIBOLD)
                .text_size(s(23.))
                .mr(s(16.))
                .child("Playwake"),
        )
        .when(tabs_available, |bar| {
            bar.child(home).child(cloud).when(v.menu.pc_available, |bar| bar.child(pc)).child(settings)
        })
        .child(div().flex_1())
        .when(v.account.is_none(), |bar| {
            bar.child(
                div()
                    .text_color(rgb(MUTED))
                    .child(t.t("Вход не выполнен", "Not signed in")),
            )
        });
    let mut hints = vec![
        ("◀▶", t.t("Выбор", "Move")),
        ("A", t.t("Выбрать", "Select")),
        ("B", t.t("Назад", "Back")),
    ];
    if v.menu.editing_value {
        hints = vec![
            ("◀▶", t.t("Изменить значение", "Adjust value")),
            ("A", t.t("Готово", "Done")),
            ("LB/RB", t.t("Вкладки", "Tabs")),
        ];
    } else if v.menu.focused_tab.is_some() {
        hints = vec![
            ("◀▶", t.t("Выбор вкладки", "Choose tab")),
            ("A", t.t("Открыть", "Open")),
            ("↓", t.t("К содержимому", "Back to content")),
            ("LB/RB", t.t("Вкладки", "Tabs")),
        ];
    } else if v.menu.tab != Tab::Settings && (v.account.is_some() || v.menu.tab == Tab::Pc) && v.stream.is_none() {
        hints = vec![
            (if v.menu.tab == Tab::Xbox { "◀▶" } else { "↔↕" }, t.t("Выбор", "Move")),
            ("A", t.t("Играть", "Play")),
            ("Y", t.t("Обновить", "Refresh")),
            ("LB/RB", t.t("Вкладки", "Tabs")),
        ];
    } else if native_stream {
        hints = vec![("B", t.t("Отмена", "Cancel"))];
    } else if v.menu.tab == Tab::Settings {
        hints.push(("LB/RB", t.t("Вкладки", "Tabs")));
    }
    let footer = div()
        .flex_none()
        .h(s(55.))
        .px(s(32.))
        .flex()
        .items_center()
        .justify_end()
        .gap(s(20.))
        .bg(rgb(PANEL))
        .border_t_1()
        .border_color(rgb(LINE))
        .children(hints.into_iter().map(|(key, label)| {
            div()
                .flex()
                .items_center()
                .gap(s(8.))
                .text_size(s(15.))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_center()
                        .min_w(s(25.))
                        .h(s(25.))
                        .px(s(5.))
                        .rounded(s(14.))
                        .bg(rgb(TEXT))
                        .text_color(rgb(CANVAS))
                        .font_weight(FontWeight::BOLD)
                        .text_size(s(13.))
                        .child(key),
                )
                .child(label)
        }));
    let mut body = div().min_h_0().flex_1().relative().overflow_hidden();
    if v.stream.is_some() && v.video_ready {
        let choices = [
            (0, t.t("Продолжить", "Resume")),
            (1, "Xbox Guide"),
            (3, t.t("Переподключить", "Reconnect")),
            (4, t.t("Завершить стрим", "End stream")),
        ];
        body = body.child(
            div()
                .p(s(36.))
                .flex()
                .flex_col()
                .gap(s(12.))
                .child(v.status.clone())
                .when(v.overlay_open, |d| {
                    d.children(choices.into_iter().enumerate().map(|(i, (index, label))| {
                        div()
                            .id(("fallback-overlay", index))
                            .bg(rgb(if v.overlay_index == i { primary } else { CARD }))
                            .px(s(18.))
                            .py(s(12.))
                            .child(label)
                            .on_click(cx.listener(move |v, _, _, cx| {
                                v.use_overlay_action(index);
                                cx.notify();
                            }))
                    }))
                })
                .when(!v.overlay_open, |d| {
                    d.child(
                        div()
                            .id("stream-menu")
                            .cursor_pointer()
                            .child(t.t("Меню стрима", "Stream menu"))
                            .on_click(cx.listener(|v, _, _, _| {
                                v.control_stream(crate::media::StreamCommand::OpenOverlay)
                            })),
                    )
                }),
        );
    } else if native_stream {
        body = hero(body, unit).child(
            div()
                .relative()
                .size_full()
                .flex()
                .flex_col()
                .justify_center()
                .pl(s(88.))
                .gap(s(18.))
                .child(
                    div()
                        .text_size(s(38.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(t.t("Подключение к Xbox", "Connecting to Xbox")),
                )
                .child(div().text_color(rgb(MUTED)).child(v.status.clone()))
                .child(
                    action(
                        ("connecting", 0),
                        div().px(s(24.)).py(s(14.)).child(t.t("Отмена", "Cancel")),
                        Action::Cancel,
                        true,
                        accent,
                        unit,
                        cx,
                    )
                    .w(s(220.)),
                ),
        );
    } else if v.menu.confirming_sign_out {
        body = body.child(
            div()
                .p(s(50.))
                .flex()
                .flex_col()
                .gap(s(20.))
                .child(
                    div()
                        .text_size(s(32.))
                        .child(t.t("Выйти из Microsoft?", "Sign out of Microsoft?")),
                )
                .children(
                    [
                        (Action::Cancel, t.t("Отмена", "Cancel")),
                        (Action::ConfirmSignOut, t.t("Выйти", "Sign out")),
                    ]
                    .into_iter()
                    .enumerate()
                    .map(|(index, (command, label))| {
                        action(
                            ("signout", index),
                            div().p(s(14.)).child(label),
                            command,
                            v.menu.browser_cancel == (index == 0),
                            accent,
                            unit,
                            cx,
                        )
                        .w(s(270.))
                    }),
                ),
        );
    } else if let Some(DevicePrompt {
        code,
        prefilled,
        qr,
        ..
    }) = &v.device
    {
        let info = div()
            .max_w(s(485.))
            .flex()
            .flex_col()
            .gap(s(18.))
            .child(
                div()
                    .text_size(s(32.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(t.t("Войдите с телефона", "Sign in on your phone")),
            )
            .child(t.t(
                "Отсканируйте QR-код. При необходимости введите код вручную.",
                "Scan the QR code. Enter the code manually if prompted.",
            ))
            .child(div().text_size(s(28.)).child(code.clone()))
            .child(div().text_color(rgb(MUTED)).child(if *prefilled {
                t.t("Код уже подставлен в ссылку", "The link includes the code")
            } else {
                "microsoft.com/link"
            }))
            .child(action(
                ("qr", 0),
                div()
                    .p(s(12.))
                    .child(t.t("Открыть в браузере", "Open in browser")),
                Action::Browser,
                !v.menu.browser_cancel,
                accent,
                unit,
                cx,
            ))
            .child(action(
                ("qr", 1),
                div().p(s(12.)).child(t.t("Отмена", "Cancel")),
                Action::Cancel,
                v.menu.browser_cancel,
                accent,
                unit,
                cx,
            ));
        body = body.child(
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .gap(s(64.))
                .children(qr.iter().map(|qr| login_qr(qr.clone(), s(5.5))))
                .child(info),
        );
    } else if v.menu.tab == Tab::Settings {
        let current_rows = rows(
            v.prefs,
            &t,
            v.account.is_some(),
            v.consoles.len(),
            v.menu.section,
        );
        let side = div()
            .w(s(295.))
            .flex_none()
            .border_r_1()
            .border_color(rgb(LINE))
            .p(s(20.))
            .flex()
            .flex_col()
            .gap(s(5.))
            .children(section_labels(&t).into_iter().enumerate().map(|(i, name)| {
                action(
                    ("section", i),
                    div()
                        .w_full()
                        .h(s(49.))
                        .px(s(16.))
                        .flex()
                        .items_center()
                        .bg(rgb(if v.menu.section == i { CARD } else { CANVAS }))
                        .child(name),
                    Action::Section(i),
                    v.menu.section == i && v.menu.row.is_none() && v.menu.focused_tab.is_none(),
                    accent,
                    unit,
                    cx,
                )
                .border_color(rgb(
                    if v.menu.section == i && v.menu.row.is_none() && v.menu.focused_tab.is_none() {
                        accent
                    } else {
                        CANVAS
                    },
                ))
            }));
        let mut active_index = 0;
        let content = div()
            .flex_1()
            .min_w_0()
            .p(s(32.))
            .flex()
            .flex_col()
            .gap(s(10.))
            .child(
                div()
                    .text_size(s(30.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .mb(s(8.))
                    .child(section_labels(&t)[v.menu.section]),
            )
            .children(current_rows.into_iter().enumerate().map(|(i, row)| {
                let inner = div()
                    .bg(rgb(CARD))
                    .rounded(s(10.))
                    .min_h(s(62.))
                    .px(s(18.))
                    .py(s(10.))
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(s(16.))
                    .child(div().flex().flex_col().child(row.label).when(
                        !row.hint.is_empty(),
                        |d| {
                            d.child(
                                div()
                                    .text_size(s(14.))
                                    .text_color(rgb(MUTED))
                                    .child(row.hint),
                            )
                        },
                    ))
                    .child(
                        div()
                            .flex_none()
                            .text_color(rgb(if row.action.is_some() { TEXT } else { MUTED }))
                            .child(row.value),
                    );
                if let Some(command) = row.action {
                    let selected = v.menu.row == Some(active_index) && v.menu.focused_tab.is_none();
                    active_index += 1;
                    action(
                        ("setting", i),
                        inner,
                        command,
                        selected,
                        if selected && v.menu.editing_value {
                            TEXT
                        } else {
                            accent
                        },
                        unit,
                        cx,
                    )
                    .into_any_element()
                } else {
                    inner.into_any_element()
                }
            }))
            .when(!v.status.is_empty(), |el| {
                el.child(
                    div()
                        .text_size(s(14.))
                        .text_color(rgb(MUTED))
                        .child(v.status.clone()),
                )
            });
        body = body.child(div().size_full().flex().child(side).child(content));
    } else if v.menu.tab == Tab::Pc {
        let several = v.pc_catalog.several_pcs();
        let pair = t.t("Сопрячь", "Pair");
        let running = t.t("запущено", "running");
        let labels: Vec<String> = v.pc_catalog.games.iter().map(|game| game.label(several, running))
            .chain(v.pc_catalog.unpaired.iter().map(|pc| format!("{pair}: {}", pc.name)))
            .collect();
        let names = labels.iter().map(String::as_str).collect();
        body = body.child(library(v, Library {
            id: "pc-game", title: t.t("Игры с ПК", "PC games"), names, selected: v.menu.pc_game,
            loading: v.pc_loading.then(|| t.t("Поиск ПК…", "Looking for PCs…")),
            error: v.pc_error.as_deref(),
            empty: t.t("ПК с Sunshine в сети не найдены.", "No PC with Sunshine found on the network."),
            refresh: t.t("Обновить список", "Refresh list"), play: Action::PcGame,
        }, accent, unit, cx));
    } else if v.account.is_none() {
        let waiting = v.sign_in.is_some();
        body = hero(body, unit).child(
            div()
                .relative()
                .pl(s(86.))
                .size_full()
                .flex()
                .flex_col()
                .justify_center()
                .gap(s(18.))
                .child(img("branding/icon.png").w(s(88.)).h(s(88.)).rounded(s(20.)))
                .child(
                    div()
                        .text_size(s(43.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(if v.menu.tab == Tab::Cloud { "Xbox Cloud Gaming" }
                               else { t.t("Стриминг с вашей Xbox", "Stream from your Xbox") }),
                )
                .child(div().max_w(s(530.)).text_color(rgb(0xc9d3d0)).child(if v.menu.tab == Tab::Cloud {
                    t.t("Войдите в аккаунт Microsoft для доступа к облачным играм.", "Sign in to Microsoft to access cloud games.")
                } else {
                    t.t("Войдите в аккаунт Microsoft, к которому привязана консоль.", "Sign in to the Microsoft account your console uses.")
                }))
                .child(
                    action(
                        ("welcome", 0),
                        div().p(s(14.)).bg(rgb(primary)).child(if waiting {
                            t.t("Отмена", "Cancel")
                        } else {
                            t.t("Войти через Microsoft", "Sign in with Microsoft")
                        }),
                        if waiting {
                            Action::Cancel
                        } else {
                            Action::SignIn
                        },
                        true,
                        accent,
                        unit,
                        cx,
                    )
                    .w(s(290.)),
                )
                .child(
                    div()
                        .text_size(s(15.))
                        .text_color(rgb(MUTED))
                        .child(v.status.clone()),
                ),
        );
    } else if v.menu.tab == Tab::Cloud {
        let names = v.cloud_catalog.iter().flatten().map(|game| game.name.as_str()).collect();
        body = body.child(library(v, Library {
            id: "cloud-game", title: "Xbox Cloud Gaming", names, selected: v.menu.cloud_game,
            loading: v.cloud_loading.then(|| if v.cloud_catalog.is_some() {
                t.t("Обновление каталога…", "Refreshing cloud library…")
            } else {
                t.t("Загрузка каталога…", "Loading cloud library…")
            }),
            error: v.cloud_error.as_deref(),
            empty: t.t("Для этого аккаунта нет доступных облачных игр.", "No cloud games are available for this account."),
            refresh: t.t("Обновить каталог", "Refresh library"), play: Action::CloudGame,
        }, accent, unit, cx));
    } else {
        let heading = div()
            .flex()
            .items_end()
            .gap(s(18.))
            .child(
                div()
                    .text_size(s(34.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(t.t("Консоли", "Consoles")),
            )
            .child(
                div()
                    .text_size(s(15.))
                    .text_color(rgb(MUTED))
                    .child(format!(
                        "{} {}",
                        v.consoles.len(),
                        t.t("в аккаунте", "on this account")
                    )),
            );
        let start = (v.menu.console / 3) * 3;
        let cards = div().flex().gap(s(22.)).children(
            v.consoles
                .iter()
                .enumerate()
                .skip(start)
                .take(3)
                .map(|(i, item)| {
                    let name = if item.name.is_empty() {
                        if item.model.is_empty() {
                            "Xbox"
                        } else {
                            &item.model
                        }
                    } else {
                        &item.name
                    };
                    let state = item.power.to_lowercase();
                    let power = match state.as_str() {
                        "on" => t.t("Включена", "On"),
                        "standby" | "connectedstandby" => t.t("Режим ожидания", "Standby"),
                        _ => t.t("Не в сети", "Offline"),
                    };
                    let card = div()
                        .w(s(345.))
                        .bg(rgb(CARD))
                        .rounded(s(12.))
                        .overflow_hidden()
                        .child(
                            div()
                                .w_full()
                                .h(s(178.))
                                .relative()
                                .overflow_hidden()
                                .child(
                                    img("branding/hero.png")
                                        .absolute()
                                        .size_full()
                                        .object_fit(ObjectFit::Cover),
                                )
                                .child(div().absolute().size_full().bg(linear_gradient(
                                    180.,
                                    linear_color_stop(rgba(0x142b2420), 0.),
                                    linear_color_stop(rgba(0x0f211eee), 1.),
                                ))),
                        )
                        .child(
                            div()
                                .p(s(16.))
                                .flex()
                                .flex_col()
                                .gap(s(10.))
                                .child(
                                    div()
                                        .text_size(s(24.))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child(name.to_string()),
                                )
                                .child(
                                    div()
                                        .text_color(rgb(MUTED))
                                        .text_size(s(15.))
                                        .child(format!("{}  ·  {}", item.model, power)),
                                )
                                .child(
                                    div()
                                        .mt(s(3.))
                                        .p(s(12.))
                                        .text_center()
                                        .bg(rgb(primary))
                                        .rounded(s(9.))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child(if state == "on" {
                                            t.t("Играть", "Play")
                                        } else {
                                            t.t("Включить и играть", "Wake and play")
                                        }),
                                ),
                        );
                    action(
                        ("console-card", i),
                        card,
                        Action::Play(i),
                        v.menu.console == i && v.menu.focused_tab.is_none(),
                        accent,
                        unit,
                        cx,
                    )
                }),
        );
        body = body.child(
            div()
                .p(s(38.))
                .size_full()
                .flex()
                .flex_col()
                .gap(s(20.))
                .child(heading)
                .child(cards)
                .when(v.consoles.is_empty(), |el| {
                    el.child(
                        action(
                            ("refresh", 0),
                            div()
                                .p(s(12.))
                                .child(t.t("Обновить список", "Refresh consoles")),
                            Action::Refresh,
                            v.menu.focused_tab.is_none(),
                            accent,
                            unit,
                            cx,
                        )
                        .w(s(240.)),
                    )
                })
                .child(
                    div()
                        .text_size(s(16.))
                        .text_color(rgb(MUTED))
                        .child(format!(
                            "{}p · {} · LSFG {} · {} {}",
                            v.prefs.height,
                            t.t("Вписать", "Fit"),
                            if v.prefs.lsfg_enabled {
                                t.t("вкл.", "on")
                            } else {
                                t.t("выкл.", "off")
                            },
                            v.prefs.bitrate_mbps,
                            t.t("Мбит/с", "Mbps")
                        )),
                )
                .when(!v.status.is_empty(), |el| {
                    el.child(
                        div()
                            .text_size(s(15.))
                            .text_color(rgb(MUTED))
                            .child(v.status.clone()),
                    )
                }),
        );
    }
    div()
        .id("deck")
        .size_full()
        .bg(rgb(CANVAS))
        .text_color(rgb(TEXT))
        .text_size(s(18.))
        .flex()
        .flex_col()
        .track_focus(&v.keyboard_focus)
        .on_key_down(cx.listener(|view, ev: &KeyDownEvent, window, cx| {
            if view.type_into_stream(&ev.keystroke) { return; }
            let mapped = Key::parse(ev.keystroke.key.as_str());
            if let Some(key) = mapped {
                view.menu_key(key, window, cx);
                cx.notify();
            }
        }))
        .child(top)
        .child(body)
        .child(footer)
        .into_any_element()
}

/// A paged grid of game names: the cloud catalog or the apps of a PC.
struct Library<'a> {
    id: &'static str,
    title: &'static str,
    names: Vec<&'a str>,
    selected: usize,
    loading: Option<&'static str>,
    error: Option<&'a str>,
    empty: &'static str,
    refresh: &'static str,
    play: fn(usize) -> Action,
}

fn library(v: &RemotePlay, list: Library, accent: u32, unit: f32, cx: &mut Context<RemotePlay>) -> Div {
    let s = |p| px(p * unit);
    let games = &list.names;
    let start = (list.selected / LIBRARY_PAGE) * LIBRARY_PAGE;
    let end = (start + LIBRARY_PAGE).min(games.len());
    let mut page = div().p(s(38.)).size_full().flex().flex_col().gap(s(16.))
        .child(div().text_size(s(34.)).font_weight(FontWeight::SEMIBOLD).child(list.title));
    if let Some(loading) = list.loading {
        page = page.child(loading);
    }
    if let Some(error) = list.error {
        page = page.child(div().text_color(rgb(MUTED)).child(error.to_owned()));
    }
    if games.is_empty() && list.loading.is_none() {
        if list.error.is_none() {
            page = page.child(list.empty);
        }
        return page.child(action((list.id, usize::MAX), div().p(s(12.))
            .child(list.refresh), Action::Refresh,
            v.menu.focused_tab.is_none(), accent, unit, cx).w(s(240.)));
    }
    for (row, chunk) in games[start..end].chunks(LIBRARY_COLUMNS).enumerate() {
        page = page.child(div().flex().gap(s(14.)).children(chunk.iter().enumerate().map(|(column, name)| {
            let index = start + row * LIBRARY_COLUMNS + column;
            let card = div().w(s(335.)).h(s(76.)).px(s(14.)).flex().items_center()
                .bg(rgb(CARD)).rounded(s(10.)).text_size(s(18.)).child(name.to_string());
            action((list.id, index), card, (list.play)(index),
                list.selected == index && v.menu.focused_tab.is_none(), accent, unit, cx)
        })));
    }
    if !games.is_empty() {
        page = page.child(div().text_color(rgb(MUTED)).child(format!("{}–{} / {}", start + 1, end, games.len())));
        if !v.status.is_empty() { page = page.child(div().text_color(rgb(MUTED)).child(v.status.clone())); }
    }
    page
}

fn hero(body: Div, unit: f32) -> Div {
    body.child(
        img("branding/hero.png")
            .absolute()
            .size_full()
            .object_fit(ObjectFit::Cover),
    )
    .child(div().absolute().size_full().bg(linear_gradient(
        90.,
        linear_color_stop(rgba(0x0d1b19ff), 0.45),
        linear_color_stop(rgba(0x0d1b191c), 1.0),
    )))
    .min_h(px(200. * unit))
}

#[cfg(test)]
mod navigation_tests {
    use super::{Action, Confirm, Key, Menu, Tab};

    #[test]
    fn a_held_key_does_not_confirm_the_dialog_it_opened() {
        let ms = std::time::Duration::from_millis;
        let opened = std::time::Instant::now();
        let mut confirm = Confirm::opened(opened);
        // The key repeats after half a second and then every 40 ms.
        assert!((0..50).all(|repeat| !confirm.press(opened + ms(500 + repeat * 40))));
        assert!(confirm.press(opened + ms(500 + 49 * 40 + 1000)));
    }

    #[test]
    fn focus_and_active_tab_are_independent() {
        let mut menu = Menu::default();
        let consoles = [Action::Play(0)];
        assert_eq!(menu.navigate(Key::Up, &[], &consoles), None);
        assert_eq!(menu.focused_tab, Some(Tab::Xbox));
        assert_eq!(menu.navigate(Key::Right, &[], &consoles), None);
        assert_eq!(menu.focused_tab, Some(Tab::Cloud));
        assert_eq!(menu.tab, Tab::Xbox);
        assert_eq!(
            menu.navigate(Key::Accept, &[], &consoles),
            Some((Action::Cloud, 1))
        );
    }

    #[test]
    fn leaves_a_settings_row_without_back_and_switches_tabs_with_bumpers() {
        let mut menu = Menu::default();
        menu.select_tab(Tab::Settings);
        menu.select_section(1);
        let rows = [Action::Resolution, Action::Bitrate];
        menu.navigate(Key::Right, &rows, &[]);
        assert_eq!(menu.row, Some(0));
        menu.navigate(Key::Left, &rows, &[]);
        assert_eq!(menu.row, None);
        menu.navigate(Key::Right, &rows, &[]);
        menu.navigate(Key::Up, &rows, &[]);
        assert_eq!(menu.focused_tab, Some(Tab::Settings));
        assert_eq!(
            menu.navigate(Key::PreviousTab, &rows, &[]),
            Some((Action::Cloud, 1))
        );
        menu.select_tab(Tab::Xbox);
        assert_eq!(
            menu.navigate(Key::NextTab, &[], &[]),
            Some((Action::Cloud, 1))
        );
    }

    #[test]
    fn edits_only_when_the_value_is_activated() {
        let mut menu = Menu::default();
        menu.select_tab(Tab::Settings);
        let rows = [Action::Resolution];
        menu.navigate(Key::Right, &rows, &[]);
        assert_eq!(menu.navigate(Key::Accept, &rows, &[]), None);
        assert!(menu.editing_value);
        assert_eq!(
            menu.navigate(Key::Left, &rows, &[]),
            Some((Action::Resolution, -1))
        );
        menu.navigate(Key::Up, &rows, &[]);
        assert!(!menu.editing_value);
        assert_eq!(menu.focused_tab, Some(Tab::Settings));
    }

    #[test]
    fn the_pc_tab_is_reachable_only_where_it_is_available() {
        let mut menu = Menu::default();
        menu.select_tab(Tab::Cloud);
        assert_eq!(menu.navigate(Key::NextTab, &[], &[]), Some((Action::Settings, 1)));
        menu.pc_available = true;
        assert_eq!(menu.navigate(Key::NextTab, &[], &[]), Some((Action::Pc, 1)));
        menu.select_tab(Tab::Settings);
        assert_eq!(menu.navigate(Key::PreviousTab, &[], &[]), Some((Action::Pc, 1)));
    }

    #[test]
    fn the_pc_grid_moves_by_columns_and_starts_the_selected_game() {
        let mut menu = Menu::default();
        menu.select_tab(Tab::Pc);
        let games: Vec<Action> = (0..5).map(Action::PcGame).collect();
        menu.navigate(Key::Down, &[], &games);
        menu.navigate(Key::Right, &[], &games);
        assert_eq!(menu.pc_game, 4);
        assert_eq!(menu.cloud_game, 0);
        assert_eq!(menu.navigate(Key::Accept, &[], &games), Some((Action::PcGame(4), 1)));
        assert_eq!(menu.navigate(Key::Back, &[], &games), Some((Action::Home, 1)));
    }
}
