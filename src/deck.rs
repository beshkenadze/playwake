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
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Action {
    Settings,
    Home,
    Play(usize),
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
    Lsfg,
    Compact,
    Language,
    Palette,
    Volume,
    Mute,
}

#[derive(Default)]
pub struct Menu {
    pub settings: bool,
    pub section: usize,
    pub row: Option<usize>,
    pub console: usize,
    pub browser_cancel: bool,
    pub confirming_sign_out: bool,
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
                t.t("Масштабирование", "Scaling"),
                "",
                t.t("Вписать", "Fit"),
                None,
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
                "LSFG 2× Performance",
                t.t(
                    "Применяется при следующем подключении",
                    "Applies on the next connection",
                ),
                on(p.lsfg_enabled),
                Some(Lsfg),
            ),
            Row::new(
                "Vulkan",
                "",
                t.t("Вывод при включённом LSFG", "Video output with LSFG"),
                None,
            ),
            Row::new("Flow Scale", "", "1.0", None),
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
                    "Одиночные нажатия уходят на Xbox",
                    "Individual presses are sent to Xbox",
                ),
                "View + Menu · 0.5 s",
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
    if v.account.is_none() {
        return vec![Action::SignIn];
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

pub fn render(v: &mut RemotePlay, window: &mut Window, cx: &mut Context<RemotePlay>) -> AnyElement {
    let viewport = window.viewport_size();
    let unit = (f32::from(viewport.width) / 1280.)
        .min(f32::from(viewport.height) / 700.)
        .max(0.2);
    let s = |p| px(p * unit);
    let t = Text(v.prefs.russian());
    let (accent, primary) = colors(v.prefs);
    let native_stream = v.stream.is_some() && super::x11_window(window).is_some();
    if native_stream && v.video_ready {
        return div().size_full().bg(rgb(0)).into_any_element();
    }
    let tab = |label: &'static str, active: bool| {
        div()
            .flex()
            .items_center()
            .px(s(16.))
            .h(s(38.))
            .bg(rgb(if active { CARD } else { PANEL }))
            .child(label)
    };
    let settings = action(
        ("nav", 0),
        tab(
            t.t("Настройки", "Settings"),
            v.menu.settings && !native_stream,
        ),
        Action::Settings,
        false,
        accent,
        unit,
        cx,
    );
    let home = action(
        ("nav", 1),
        tab("Xbox", !v.menu.settings || native_stream),
        Action::Home,
        false,
        accent,
        unit,
        cx,
    );
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
        .when(!native_stream, |bar| bar.child(home).child(settings))
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
    if !v.menu.settings && v.account.is_some() && v.stream.is_none() {
        hints = vec![
            ("◀▶", t.t("Выбор", "Move")),
            ("A", t.t("Играть", "Play")),
            ("Y", t.t("Обновить", "Refresh")),
            ("≡", t.t("Настройки", "Settings")),
        ];
    } else if native_stream {
        hints = vec![("B", t.t("Отмена", "Cancel"))];
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
    } else if v.menu.settings {
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
                    v.menu.section == i && v.menu.row.is_none(),
                    accent,
                    unit,
                    cx,
                )
                .border_color(rgb(
                    if v.menu.section == i && v.menu.row.is_none() {
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
                    let selected = v.menu.row == Some(active_index);
                    active_index += 1;
                    action(("setting", i), inner, command, selected, accent, unit, cx)
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
                        .child(t.t("Стриминг с вашей Xbox", "Stream from your Xbox")),
                )
                .child(div().max_w(s(530.)).text_color(rgb(0xc9d3d0)).child(t.t(
                    "Войдите в аккаунт Microsoft, к которому привязана консоль.",
                    "Sign in to the Microsoft account your console uses.",
                )))
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
                        v.menu.console == i,
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
                            true,
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
