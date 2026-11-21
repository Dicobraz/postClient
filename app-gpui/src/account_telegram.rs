//! «Настройки → Аккаунты → Каналы» (CH-03; без макета — по `docs/DESIGN.md`: карточка на месте, как добавление
//! почтового аккаунта): включатель Telegram, учётки канала с состоянием и «Выйти», «Добавить учётку Telegram» —
//! вход на месте: телефон → код → пароль двухэтапной проверки или QR-код с другого устройства. Полноценный вид
//! бесед — CH-04.
//!
//! Канал — отдельным процессом общего набора `postman-plugins` (PLG-05, ADR 0013): признака сборки нет,
//! `grammers` не в дереве `postman.exe`; включается настройкой (`Prefs::channels`, по умолчанию выключен) и
//! политикой организации `AllowChannelTelegram` (`0` — канала нет в настройках, учётки не подключаются; `1` —
//! включён, в настройках не выключить). Ключ приложения — у плагина, его наличие приходит в `Hello`
//! (`plugins::telegram_has_key`). Вход идёт своим потоком с tokio: интерфейс сети не трогает, ответы приходят
//! сообщениями; секрет сессии кладёт в хранилище ОС экран почты (`channels.rs`).

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::*;
use gpui_kit::*;
use pets_i18n::tr;
use pets_ui::theme::Metric;
use postman_core::channel::telegram as tg;
use postman_core::channel::{AccountId, Answer, ChanAccount, ChannelError, ChannelState, CodeVia, Prompt, Secret};
use postman_core::prefs::Prefs;

use crate::buttons::button;
use crate::roles::Look;
use crate::settings_channels::account_row;
use crate::settings_tab::{group, link, row, seg, seg_group, toggle};

/// Id канала.
pub(crate) const TELEGRAM: &str = "telegram";

/// Опись части (ADR 0005): только тексты.
pub(crate) const PART: crate::part::Part = crate::part::Part { name: "channels", texts: TEXTS, ..crate::part::Part::NONE };

const TEXTS: &[pets_i18n::Resource] = &[("ru", include_str!("../i18n/ru/channels.ftl")), ("en", include_str!("../i18n/en/channels.ftl"))];

/// Канал Telegram есть в этой сборке всегда (PLG-05): код — в процессе набора, не в `postman.exe`; включают настройка
/// и политика, файл набора — часть установки.
pub(crate) const BUILT: bool = true;

/// Политика организации запретила канал: его нет в настройках.
pub(crate) fn forbidden() -> bool {
    crate::security::policy().telegram == Some(false)
}

/// Включён ли канал: политика сильнее настройки.
pub(crate) fn enabled(p: &Prefs) -> bool {
    BUILT && crate::security::policy().telegram.unwrap_or(p.channels.telegram)
}

/// Канал включён (для потоков учёток): чужие каналы (демо) — всегда.
pub(crate) fn channel_on(id: &str, p: &Prefs) -> bool {
    match id {
        TELEGRAM => enabled(p),
        crate::account_matrix::MATRIX => crate::account_matrix::enabled(p),
        _ => true,
    }
}

/// Ключ приложения есть (без него вход отвечает «нет ключа приложения»): говорит плагин в `Hello` (PLG-05), интерфейс
/// у `api_key` не зовёт. Процесса ещё не было — показываем вход, отказ вернёт сам плагин.
pub(crate) fn has_key() -> bool {
    crate::plugins::telegram_has_key()
}

/// Учётки каналов окна — для настроек (пишет экран почты по событиям потоков).
#[derive(Default, Clone)]
pub(crate) struct Lines(pub(crate) Vec<Line>);

impl Global for Lines {}

#[derive(Clone)]
pub(crate) struct Line {
    pub(crate) account: ChanAccount,
    pub(crate) name: String,
    pub(crate) state: Option<ChannelState>,
}

/// Панель — экрану почты (через настройки).
#[derive(Clone)]
pub(crate) enum PanelEvent {
    /// Вход удался: секрет — в хранилище ОС, учётку — подключить.
    Signed { account: AccountId, name: String, secret: Secret },
    /// «Выйти» из учётки канала.
    SignOut(ChanAccount),
    /// «Повторить» у проблемы канала: подключиться сейчас (`Request::Reconnect`, AUD-06).
    Reconnect(ChanAccount),
    /// Включатель канала.
    Toggled(bool),
}

/// Ответ потока входа.
pub(crate) type Reply = Result<Prompt, ChannelError>;

/// Вход на месте.
struct Card {
    prompt: Option<Prompt>,
    busy: bool,
    error: Option<String>,
    /// Телефон или код.
    text: Entity<InputState>,
    password: Entity<InputState>,
    answers: postman_core::sync::UnboundedSender<Option<Answer>>,
    qr: Option<Qr>,
}

pub(crate) struct Panel {
    card: Option<Card>,
    _subs: Vec<Subscription>,
}

impl EventEmitter<PanelEvent> for Panel {}

impl Panel {
    pub(crate) fn new(cx: &mut Context<Self>) -> Self {
        let subs = vec![cx.observe_global::<Lines>(|_, cx| cx.notify())];
        Self { card: None, _subs: subs }
    }

    /// Карточка входа открыта.
    pub(crate) fn signing_in(&self) -> bool {
        self.card.is_some()
    }

    /// Сцена снимков `chat-login`: шаг «Код» с отказом сервера — без потока входа и сети.
    pub(crate) fn screen_code_step(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (answers, _) = tokio::sync::mpsc::unbounded_channel();
        let text = cx.new(|cx| InputState::new(window, cx));
        let password = cx.new(|cx| InputState::new(window, cx).masked(true));
        let prompt = Some(Prompt::Code { via: CodeVia::App, length: Some(5) });
        let error = Some(error_text(&ChannelError::Denied("code-invalid".into())));
        self.card = Some(Card { prompt, busy: false, error, text, password, answers, qr: None });
        cx.notify();
    }

    /// «Добавить учётку Telegram»: поток входа и первый вопрос.
    fn start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(plugin) = crate::plugins::registry().channel(TELEGRAM).cloned() else { return };
        let (answers, replies) = flow_thread(plugin);
        let text = cx.new(|cx| InputState::new(window, cx));
        let password = cx.new(|cx| InputState::new(window, cx).masked(true));
        let enter = |this: &mut Self, _: &Entity<InputState>, ev: &InputEvent, window: &mut Window, cx: &mut Context<Self>| {
            if let InputEvent::PressEnter { .. } = ev {
                this.submit(window, cx);
            }
        };
        self._subs.push(cx.subscribe_in(&text, window, enter));
        self._subs.push(cx.subscribe_in(&password, window, enter));
        let _ = answers.send(None);
        self.card = Some(Card { prompt: None, busy: true, error: None, text, password, answers, qr: None });
        cx.spawn(async move |this, cx| {
            use futures::StreamExt as _;
            let mut replies = replies;
            while let Some(r) = replies.next().await {
                let gone = this.update(cx, |p, cx| p.on_reply(r, cx)).is_err();
                if gone {
                    break;
                }
            }
        })
        .detach();
        cx.notify();
    }

    fn on_reply(&mut self, r: Reply, cx: &mut Context<Self>) {
        let Some(card) = &mut self.card else { return };
        card.busy = false;
        match r {
            Ok(Prompt::Done { account, name, secret }) => {
                self.card = None;
                cx.emit(PanelEvent::Signed { account, name, secret });
            }
            Ok(Prompt::Finished) => self.card = None,
            Ok(p) => {
                card.error = None;
                card.qr = match &p {
                    Prompt::Qr { url, .. } => Qr::encode(url),
                    _ => None,
                };
                // QR на экране — ждать, пока его отсканируют (поток вернёт новый QR по сроку или вход).
                if matches!(p, Prompt::Qr { .. }) {
                    card.busy = true;
                    let _ = card.answers.send(Some(Answer::Confirm(true)));
                }
                card.prompt = Some(p);
            }
            Err(e) => {
                tracing::info!(channel = TELEGRAM, "channel: sign-in step refused: {}", error_code(&e));
                card.error = Some(error_text(&e));
                if card.prompt.is_none() {
                    card.prompt = Some(Prompt::Phone);
                }
            }
        }
        cx.notify();
    }

    fn send(&mut self, a: Answer, cx: &mut Context<Self>) {
        let Some(card) = &mut self.card else { return };
        if card.busy {
            return;
        }
        card.busy = true;
        card.error = None;
        let _ = card.answers.send(Some(a));
        cx.notify();
    }

    /// Ответ на текущий вопрос из поля.
    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(card) = &self.card else { return };
        let answer = match &card.prompt {
            Some(Prompt::Phone | Prompt::Code { .. }) => {
                let v = card.text.read(cx).value().trim().to_string();
                if v.is_empty() {
                    return;
                }
                Answer::Text(v)
            }
            Some(Prompt::Password { .. }) => {
                let v = card.password.read(cx).value().to_string();
                if v.is_empty() {
                    return;
                }
                Answer::Secret(Secret::new(v.into_bytes()))
            }
            _ => return,
        };
        let clear = matches!(answer, Answer::Secret(_));
        self.send(answer, cx);
        // Пароль — не держать в поле дольше нужного; телефон и код — до ответа сервера (ошибка — исправить).
        if clear && let Some(card) = &self.card {
            card.password.update(cx, |s, cx| s.set_value("", window, cx));
        }
    }

    /// Шаги входа «Телефон → Код → Пароль» (текущий — акцентом) и способ «По номеру | По QR-коду».
    fn steps(&self, l: &Look, cx: &mut Context<Self>) -> Option<Div> {
        let card = self.card.as_ref()?;
        let prompt = card.prompt.as_ref()?;
        let at = step_of(prompt);
        let qr = matches!(prompt, Prompt::Qr { .. });
        let mut steps = div().flex().items_center().gap(px(6.)).text_size(px(12.));
        for (i, key) in ["tg-step-phone", "tg-step-code", "tg-step-password"].into_iter().enumerate() {
            if i > 0 {
                steps = steps.child(div().text_color(l.text3()).child("→"));
            }
            let on = !qr && at == Some(i);
            steps = steps.child(
                div()
                    .when(on, |d| d.font_weight(FontWeight::SEMIBOLD).text_color(l.accent_text()))
                    .when(!on, |d| d.text_color(l.text2()))
                    .child(tr!(key)),
            );
        }
        // Способ входа меняется только на первом шаге и на экране QR.
        let free = !card.busy || qr;
        let by_phone = seg("tg-by-phone", tr!("tg-by-phone"), !qr, free, l).when(qr, |s| {
            s.on_click(cx.listener(|p, _, _, cx| {
                // Ожидание QR прервать нельзя без ответа потока: ответ «телефон» уйдёт следом.
                if let Some(c) = &mut p.card {
                    let _ = c.answers.send(Some(Answer::Confirm(false)));
                    c.busy = true;
                }
                cx.notify();
            }))
        });
        let first = matches!(prompt, Prompt::Phone);
        let by_qr = seg("tg-by-qr", tr!("tg-by-qr"), qr, first && free, l)
            .when(first && !qr, |s| s.on_click(cx.listener(|p, _, _, cx| p.send(Answer::Confirm(true), cx))));
        Some(div().flex().flex_wrap().items_center().gap_3().child(seg_group([by_phone, by_qr], l)).child(div().flex_1()).child(steps))
    }

    fn card(&self, l: &Look, cx: &mut Context<Self>) -> Div {
        let Some(card) = &self.card else { return div() };
        let mut body = div()
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .rounded(l.px(Metric::RadiusOverlay))
            .border_1()
            .border_color(l.border())
            .bg(l.panel())
            .child(div().text_size(px(15.)).font_weight(FontWeight::SEMIBOLD).child(tr!("tg-signin-title")))
            .children(self.steps(l, cx));
        let field = |label: String, input: &Entity<InputState>, hint: Option<String>| {
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(div().text_color(l.text2()).child(label))
                .child(Input::new(input))
                .when_some(hint, |d, h| d.child(div().text_size(px(12.)).text_color(l.text2()).child(h)))
        };
        let busy = card.busy;
        let next =
            |label: String| button("tg-next", label, true, l).when(!busy, |b| b.on_click(cx.listener(|p, _, window, cx| p.submit(window, cx))));
        match &card.prompt {
            None => body = body.child(div().text_color(l.text2()).child(tr!("tg-connecting"))),
            Some(Prompt::Phone) => {
                body = body
                    .child(field(tr!("tg-phone"), &card.text, Some(tr!("tg-phone-hint"))))
                    .child(div().flex().items_center().gap_2().child(next(tr!("tg-get-code"))));
            }
            Some(Prompt::Code { via, length }) => {
                let via = match via {
                    CodeVia::App => "app",
                    CodeVia::Sms => "sms",
                    CodeVia::Call => "call",
                    CodeVia::Email => "email",
                };
                let hint = length.map(|n| tr!("tg-code-length", n = n.to_string()));
                body = body.child(field(tr!("tg-code", via = via), &card.text, hint)).child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(next(tr!("tg-sign-in")))
                        .child(link("tg-resend", tr!("tg-resend"), l).on_click(cx.listener(|p, _, _, cx| p.send(Answer::Resend, cx)))),
                );
            }
            Some(Prompt::Password { hint }) => {
                let hint = hint.as_ref().map(|h| tr!("tg-password-hint", hint = h.clone()));
                body = body.child(field(tr!("tg-password"), &card.password, hint)).child(next(tr!("tg-sign-in")));
            }
            Some(Prompt::Qr { .. }) => {
                body = body.child(div().text_color(l.text2()).child(tr!("tg-qr-hint"))).when_some(card.qr.clone(), |d, q| d.child(q.view(l)));
            }
            Some(_) => body = body.child(div().text_color(l.warning()).child(tr!("tg-err-other"))),
        }
        // Ошибка входа — красной строкой (безопасность).
        if let Some(e) = &card.error {
            body = body.child(div().id("tg-error").text_color(l.error()).child(e.clone()));
        }
        if busy && card.prompt.is_some() && !matches!(card.prompt, Some(Prompt::Qr { .. })) {
            body = body.child(div().text_size(px(12.)).text_color(l.text2()).child(tr!("tg-wait")));
        }
        body.child(link("tg-cancel", tr!("settings-cancel"), l).on_click(cx.listener(|p, _, _, cx| {
            p.card = None;
            cx.notify();
        })))
    }
}

impl Render for Panel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let l = crate::roles::look(cx);
        if !BUILT || forbidden() {
            return div().into_any_element();
        }
        let on = crate::prefs::with(cx, enabled);
        let pinned = crate::security::policy().telegram.is_some();
        let hint = if pinned { tr!("tg-policy") } else { tr!("tg-channel-hint") };
        let mut out = div().flex().flex_col().child(group(tr!("tg-channels"), &l)).child(row(
            tr!("tg-channel"),
            Some(hint),
            toggle("tg-toggle", on, &l).when(!pinned, |t| t.on_click(cx.listener(move |_, _, _, cx| cx.emit(PanelEvent::Toggled(!on))))),
            &l,
        ));
        if !on {
            return out.into_any_element();
        }
        let lines: Vec<Line> = cx.try_global::<Lines>().map(|g| g.0.clone()).unwrap_or_default();
        for (n, line) in lines.iter().filter(|x| x.account.channel.0 == TELEGRAM).enumerate() {
            let state = match &line.state {
                Some(ChannelState::Online) => tr!("tg-state-online"),
                Some(ChannelState::Connecting) | None => tr!("tg-state-connecting"),
                Some(ChannelState::Offline { retry_in }) => tr!("tg-state-offline", secs = retry_in.to_string()),
                Some(ChannelState::Problem(e)) => error_text(e),
            };
            let name = if line.name.is_empty() { line.account.account.0.clone() } else { line.name.clone() };
            let (account, retry) = (line.account.clone(), line.account.clone());
            // У проблемы — «Повторить» (AUD-06): у процесса плагина автоповторов нет, поток ждёт запрос.
            let actions = div()
                .flex()
                .items_center()
                .gap_3()
                .when(matches!(line.state, Some(ChannelState::Problem(_))), |d| {
                    d.child(
                        link(("tg-retry", n), tr!("comp-retry"), &l)
                            .on_click(cx.listener(move |_, _, _, cx| cx.emit(PanelEvent::Reconnect(retry.clone())))),
                    )
                })
                .child(
                    link(("tg-sign-out", n), tr!("tg-sign-out"), &l)
                        .on_click(cx.listener(move |_, _, _, cx| cx.emit(PanelEvent::SignOut(account.clone())))),
                );
            out = out.child(account_row(line.state.as_ref(), name, state, actions, &l));
        }
        // Компонент канала (PLG-15): файла набора нет или он после обновления — скачать из выпуска.
        let component = crate::components::state(crate::plugins::TELEGRAM_SET, cx);
        if let Some(comp) = crate::components::row(crate::plugins::TELEGRAM_SET, &component, &l) {
            out = out.child(comp);
        }
        if self.card.is_some() {
            out = out.child(div().pt_3().child(self.card(&l, cx)));
        } else if has_key() {
            out =
                out.child(div().pt_3().child(
                    button("tg-add", format!("+ {}", tr!("tg-add")), false, &l).on_click(cx.listener(|p, _, window, cx| p.start(window, cx))),
                ));
        } else {
            out = out.child(div().pt_3().text_size(px(12.)).text_color(l.text2()).child(tr!("tg-no-key")));
        }
        out.into_any_element()
    }
}

/// Номер шага входа «Телефон → Код → Пароль»; у QR и прочих вопросов шага нет.
fn step_of(p: &Prompt) -> Option<usize> {
    match p {
        Prompt::Phone => Some(0),
        Prompt::Code { .. } => Some(1),
        Prompt::Password { .. } => Some(2),
        _ => None,
    }
}

/// Код отказа для журнала: без телефона, кода и пароля.
fn error_code(e: &ChannelError) -> String {
    match e {
        ChannelError::Denied(c) => format!("denied {c}"),
        ChannelError::Wait { secs } => format!("wait {secs}"),
        ChannelError::Network(_) => "network".into(),
        ChannelError::SignedOut(_) => "signed out".into(),
        other => format!("{other}"),
    }
}

/// «Проблема → причина» отказа словами (Fluent); коды — в ядре (`postman_core::channel::telegram`, PLG-05):
/// интерфейс от крейта плагина не зависит.
pub(crate) fn error_text(e: &ChannelError) -> String {
    match e {
        ChannelError::Denied(code) => match code.as_str() {
            tg::NO_API_KEY => tr!("tg-no-key"),
            tg::BAD_PHONE => tr!("tg-err-phone"),
            tg::BAD_CODE => tr!("tg-err-code"),
            tg::CODE_EXPIRED => tr!("tg-err-code-expired"),
            tg::BAD_PASSWORD => tr!("tg-err-password"),
            tg::SIGN_UP => tr!("tg-err-sign-up"),
            _ => tr!("tg-err-denied"),
        },
        ChannelError::Wait { secs } => tr!("tg-err-wait", secs = secs.to_string()),
        ChannelError::Network(_) => tr!("tg-err-network"),
        ChannelError::SignedOut(_) => tr!("tg-err-signed-out"),
        // Процесс плагина не работает (ADR 0013): автоповторов нет — поток ждёт «Повторить» (AUD-06).
        ChannelError::Plugin(_) => tr!("tg-err-plugin"),
        // Файла набора нет (PLG-14): канал не установлен; установка компонента приходит с докачкой (PLG-15).
        ChannelError::Missing(_) => tr!("comp-missing"),
        _ => tr!("tg-err-other"),
    }
}

/// Поток входа: свой рантайм tokio и `LocalSet` (шаги входа — не `Send`); ответы — в интерфейс. Общий для каналов
/// (Telegram, Matrix — `account_matrix`).
pub(crate) fn flow_thread(
    plugin: std::sync::Arc<dyn postman_core::channel::ChannelPlugin>,
) -> (postman_core::sync::UnboundedSender<Option<Answer>>, futures::channel::mpsc::UnboundedReceiver<Reply>) {
    let (answers, mut rx) = tokio::sync::mpsc::unbounded_channel::<Option<Answer>>();
    let (out, replies) = futures::channel::mpsc::unbounded::<Reply>();
    let started = std::thread::Builder::new().name("channel sign-in".into()).spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(rt) => rt,
            Err(e) => {
                let _ = out.unbounded_send(Err(ChannelError::Storage(format!("tokio runtime: {e}"))));
                return;
            }
        };
        tokio::task::LocalSet::new().block_on(&rt, async move {
            let store: std::rc::Rc<dyn postman_core::channel::PluginStore> = std::rc::Rc::new(NoStore);
            let mut flow = match plugin.sign_in(postman_core::channel::FlowCtx { store, sso: None }).await {
                Ok(f) => f,
                Err(e) => {
                    let _ = out.unbounded_send(Err(e));
                    return;
                }
            };
            while let Some(answer) = rx.recv().await {
                let r = flow.step(answer).await;
                let done = matches!(r, Ok(Prompt::Done { .. } | Prompt::Finished));
                if out.unbounded_send(r).is_err() || done {
                    break;
                }
            }
        });
    });
    if let Err(e) = started {
        tracing::error!("channel sign-in thread: {e}");
    }
    (answers, replies)
}

/// Вход не хранит своего: шаги идут в памяти потока.
struct NoStore;

impl postman_core::channel::PluginStore for NoStore {
    fn get(&self, _key: &str) -> Result<Option<Vec<u8>>, ChannelError> {
        Ok(None)
    }

    fn put(&self, _key: &str, _value: &[u8]) -> Result<(), ChannelError> {
        Ok(())
    }

    fn remove_prefix(&self, _prefix: &str) -> Result<(), ChannelError> {
        Ok(())
    }
}

/// QR-код входа: модули — квадратами на светлом поле (контраст для камеры — в любой теме).
#[derive(Clone)]
struct Qr {
    size: usize,
    dark: std::rc::Rc<Vec<bool>>,
}

impl Qr {
    fn encode(url: &str) -> Option<Qr> {
        let q = qrcodegen::QrCode::encode_text(url, qrcodegen::QrCodeEcc::Medium).ok()?;
        let n = usize::try_from(q.size()).ok()?;
        let size = i32::try_from(n).ok()?;
        let dark = (0..size).flat_map(|y| (0..size).map(move |x| (x, y))).map(|(x, y)| q.get_module(x, y)).collect();
        Some(Qr { size: n, dark: std::rc::Rc::new(dark) })
    }

    fn view(&self, l: &Look) -> Div {
        const MODULE: f32 = 5.;
        const QUIET: usize = 4;
        let side = px(MODULE * (self.size + 2 * QUIET) as f32);
        let (ink, paper) = (l.qr_ink(), l.qr_paper());
        let me = self.clone();
        div().size(side).flex_none().bg(paper).child(
            canvas(
                |_, _, _| {},
                move |bounds, _, window, _| {
                    for y in 0..me.size {
                        for x in 0..me.size {
                            if me.dark.get(y * me.size + x).copied().unwrap_or(false) {
                                let o = point(bounds.origin.x + px(MODULE * (x + QUIET) as f32), bounds.origin.y + px(MODULE * (y + QUIET) as f32));
                                window.paint_quad(fill(Bounds::new(o, size(px(MODULE), px(MODULE))), ink));
                            }
                        }
                    }
                },
            )
            .size_full(),
        )
    }
}

#[cfg(test)]
mod tests {
    use postman_core::channel::ChannelError;
    use postman_core::prefs::Prefs;

    use super::{BUILT, channel_on, enabled, error_code, error_text, forbidden};

    /// Без политики и настройки — выключен; запрещённый политикой — не включить; демо — всегда.
    #[test]
    fn channel_switch() {
        let p = Prefs::default();
        assert!(!enabled(&p), "по умолчанию — выключен");
        assert!(channel_on("demo", &p));
        let mut on = Prefs::default();
        on.channels.telegram = true;
        assert_eq!(enabled(&on), BUILT && !forbidden());
    }

    /// Отказы входа — словами, коды — без данных пользователя.
    #[test]
    fn errors_are_words() {
        assert_eq!(error_code(&ChannelError::Network("+99912345678".into())), "network");
        assert!(!error_text(&ChannelError::Denied("code-invalid".into())).is_empty());
        // Неработающий процесс плагина — своя строка, не общий отказ (AUD-06).
        assert_ne!(error_text(&ChannelError::Plugin("crashed".into())), error_text(&ChannelError::Protocol("x".into())));
    }

    #[test]
    fn qr_encodes() {
        let q = super::Qr::encode("tg://login?token=AbCdEf-_123").unwrap();
        assert!(q.size >= 21 && q.dark.iter().any(|d| *d));
    }
}
