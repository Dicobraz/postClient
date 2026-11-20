//! «Настройки → Аккаунты → Каналы бесед»: Matrix (CH-05; без макета — как Telegram, `account_telegram`): включатель,
//! учётки с состоянием, «Проверить это устройство», «Восстановить ключи», «Выйти», «Добавить учётку Matrix» — вход на
//! месте: сервер → имя и пароль или «Войти через браузер»; и «Шифрованные беседы» — где держать расшифрованный текст
//! (ADR 0012, решение владельца п. 1). Вид бесед — CH-04.
//!
//! Канал — отдельным процессом набора `postman-plugins-matrix` (PLG-04, ADR 0013): признака сборки нет,
//! `matrix-sdk` не в дереве `postman.exe`; включается настройкой (`Prefs::channels.matrix`, по умолчанию выключен) и
//! политикой `AllowChannelMatrix`, выбор «Шифрованные беседы» закрепляет политика `ChannelE2EStorePlaintext`. Файл
//! набора — компонент установки (PLG-03; в выпуск 0.2.0 не входит): нет файла — канал «без связи» с причиной.
//! Вход — своим потоком (`account_telegram::flow_thread`); проверка устройства и ключ резервной копии — потоком
//! учётки (`channel::Request::Flow`), ответы — глобалом [`FlowReply`].

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::*;
use gpui_kit::*;
use pets_i18n::tr;
use pets_ui::theme::Metric;
use postman_core::channel::e2e::E2eStore;
use postman_core::channel::matrix as mx;
use postman_core::channel::{AccountId, Answer, ChanAccount, ChannelError, ChannelState, FlowKind, Prompt, Secret};
use postman_core::prefs::Prefs;

use crate::account_telegram::{Line, Lines, Reply};
use crate::buttons::button;
use crate::roles::Look;
use crate::settings_tab::{link, row, seg, seg_group, toggle};

/// Id канала.
pub(crate) const MATRIX: &str = "matrix";

/// Канал Matrix есть в этой сборке всегда (PLG-04): код — в процессе набора, не в `postman.exe`; включают настройка
/// и политика, файл набора — компонент установки.
pub(crate) const BUILT: bool = true;

/// Политика организации запретила канал: его нет в настройках.
pub(crate) fn forbidden() -> bool {
    crate::security::policy().matrix == Some(false)
}

/// Включён ли канал: политика сильнее настройки.
pub(crate) fn enabled(p: &Prefs) -> bool {
    BUILT && crate::security::policy().matrix.unwrap_or(p.channels.matrix)
}

/// Действующий выбор «Шифрованные беседы»: политика сильнее настройки.
pub(crate) fn encrypted(p: &Prefs) -> E2eStore {
    crate::security::policy().encrypted.unwrap_or(p.channels.encrypted)
}

/// Ответ потока учётки на вопрос проверки устройства или ключа резервной копии (пишет экран почты).
#[derive(Default, Clone)]
pub(crate) struct FlowReply(pub(crate) Option<(ChanAccount, Reply)>);

impl Global for FlowReply {}

/// Панель — экрану почты (через настройки).
#[derive(Clone)]
pub(crate) enum PanelEvent {
    Signed {
        account: AccountId,
        name: String,
        secret: Secret,
    },
    SignOut(ChanAccount),
    Toggled(bool),
    /// Начать проверку устройства или восстановление ключей учётки.
    Flow {
        account: ChanAccount,
        kind: FlowKind,
    },
    /// Ответ на вопрос этого потока.
    Answer {
        account: ChanAccount,
        answer: Answer,
    },
    /// «Шифрованные беседы».
    Encrypted(E2eStore),
}

/// Вход на месте.
struct Card {
    prompt: Option<Prompt>,
    busy: bool,
    error: Option<String>,
    text: Entity<InputState>,
    password: Entity<InputState>,
    answers: postman_core::sync::UnboundedSender<Option<Answer>>,
}

/// Проверка устройства или ключ резервной копии учётки.
struct FlowCard {
    account: ChanAccount,
    kind: FlowKind,
    prompt: Option<Prompt>,
    busy: bool,
    error: Option<String>,
    key: Entity<InputState>,
    done: bool,
}

pub(crate) struct Panel {
    card: Option<Card>,
    flow: Option<FlowCard>,
    _subs: Vec<Subscription>,
}

impl EventEmitter<PanelEvent> for Panel {}

impl Panel {
    pub(crate) fn new(cx: &mut Context<Self>) -> Self {
        let subs = vec![cx.observe_global::<Lines>(|_, cx| cx.notify()), cx.observe_global::<FlowReply>(|p, cx| p.on_flow(cx))];
        Self { card: None, flow: None, _subs: subs }
    }

    /// «Добавить учётку Matrix»: поток входа и первый вопрос.
    fn start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(plugin) = crate::plugins::registry().channel(MATRIX).cloned() else { return };
        let (answers, replies) = crate::account_telegram::flow_thread(plugin);
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
        self.card = Some(Card { prompt: None, busy: true, error: None, text, password, answers });
        cx.spawn(async move |this, cx| {
            use futures::StreamExt as _;
            let mut replies = replies;
            while let Some(r) = replies.next().await {
                if this.update(cx, |p, cx| p.on_reply(r, cx)).is_err() {
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
                if let Prompt::Browser { url } = &p {
                    // Страница входа сервера — в браузере ОС; поток ждёт возврата на loopback.
                    cx.open_url(url);
                    card.busy = true;
                    let _ = card.answers.send(Some(Answer::Confirm(true)));
                }
                card.prompt = Some(p);
            }
            Err(e) => {
                tracing::info!(channel = MATRIX, "channel: sign-in step refused: {}", error_code(&e));
                card.error = Some(error_text(&e));
                if card.prompt.is_none() {
                    card.prompt = Some(Prompt::Server);
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

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(card) = &self.card else { return };
        let answer = match &card.prompt {
            Some(Prompt::Server | Prompt::Login) => {
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
        let (clear_text, clear_password) = (matches!(card.prompt, Some(Prompt::Server)), matches!(answer, Answer::Secret(_)));
        self.send(answer, cx);
        if let Some(card) = &self.card {
            // Сервер отвечен — поле под имя; пароль — не держать в поле дольше нужного.
            if clear_text {
                card.text.update(cx, |s, cx| s.set_value("", window, cx));
            }
            if clear_password {
                card.password.update(cx, |s, cx| s.set_value("", window, cx));
            }
        }
    }

    fn card(&self, l: &Look, cx: &mut Context<Self>) -> Div {
        let Some(card) = &self.card else { return div() };
        let mut body = frame(tr!("mx-signin-title"), l);
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
            |label: String| button("mx-next", label, true, l).when(!busy, |b| b.on_click(cx.listener(|p, _, window, cx| p.submit(window, cx))));
        match &card.prompt {
            None => body = body.child(div().text_color(l.text2()).child(tr!("mx-connecting"))),
            Some(Prompt::Server) => {
                body = body.child(field(tr!("mx-server"), &card.text, Some(tr!("mx-server-hint")))).child(next(tr!("mx-continue")));
            }
            Some(Prompt::Login) => {
                body =
                    body.child(field(tr!("mx-login"), &card.text, Some(tr!("mx-login-hint")))).child(
                        div().flex().items_center().gap_2().child(next(tr!("mx-continue"))).child(
                            link("mx-browser", tr!("mx-use-browser"), l).on_click(cx.listener(|p, _, _, cx| p.send(Answer::Confirm(true), cx))),
                        ),
                    );
            }
            Some(Prompt::Password { .. }) => {
                body = body.child(field(tr!("mx-password"), &card.password, None)).child(next(tr!("mx-sign-in")));
            }
            Some(Prompt::Browser { url }) => {
                let url = url.clone();
                body = body
                    .child(div().text_color(l.text2()).child(tr!("mx-browser-hint")))
                    .child(link("mx-browser-again", tr!("mx-browser-again"), l).on_click(cx.listener(move |_, _, _, cx| cx.open_url(&url))));
            }
            Some(_) => body = body.child(div().text_color(l.warning()).child(tr!("mx-err-other"))),
        }
        if let Some(e) = &card.error {
            body = body.child(div().text_color(l.warning()).child(e.clone()));
        }
        if busy && card.prompt.is_some() && !matches!(card.prompt, Some(Prompt::Browser { .. })) {
            body = body.child(div().text_size(px(12.)).text_color(l.text2()).child(tr!("mx-wait")));
        }
        body.child(link("mx-cancel", tr!("settings-cancel"), l).on_click(cx.listener(|p, _, _, cx| {
            p.card = None;
            cx.notify();
        })))
    }

    /// «Проверить это устройство» или «Восстановить ключи»: вопрос — от потока учётки.
    fn start_flow(&mut self, account: ChanAccount, kind: FlowKind, window: &mut Window, cx: &mut Context<Self>) {
        let key = cx.new(|cx| InputState::new(window, cx).masked(true));
        let enter = |this: &mut Self, _: &Entity<InputState>, ev: &InputEvent, window: &mut Window, cx: &mut Context<Self>| {
            if let InputEvent::PressEnter { .. } = ev {
                this.submit_key(window, cx);
            }
        };
        self._subs.push(cx.subscribe_in(&key, window, enter));
        self.flow = Some(FlowCard { account: account.clone(), kind, prompt: None, busy: true, error: None, key, done: false });
        cx.emit(PanelEvent::Flow { account, kind });
        cx.notify();
    }

    fn on_flow(&mut self, cx: &mut Context<Self>) {
        let Some((account, reply)) = cx.try_global::<FlowReply>().and_then(|g| g.0.clone()) else { return };
        let Some(f) = &mut self.flow else { return };
        if f.account != account {
            return;
        }
        f.busy = false;
        match reply {
            Ok(Prompt::Finished) => {
                f.done = true;
                f.prompt = None;
            }
            Ok(p) => {
                f.error = None;
                f.prompt = Some(p);
            }
            Err(e) => {
                tracing::info!(channel = MATRIX, "channel: flow step refused: {}", error_code(&e));
                f.error = Some(error_text(&e));
            }
        }
        cx.notify();
    }

    fn answer(&mut self, answer: Answer, cx: &mut Context<Self>) {
        let Some(f) = &mut self.flow else { return };
        if f.busy {
            return;
        }
        f.busy = true;
        f.error = None;
        cx.emit(PanelEvent::Answer { account: f.account.clone(), answer });
        cx.notify();
    }

    fn submit_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(f) = &self.flow else { return };
        let v = f.key.read(cx).value().trim().to_string();
        if v.is_empty() || !matches!(f.prompt, Some(Prompt::RecoveryKey)) {
            return;
        }
        let key = f.key.clone();
        self.answer(Answer::Secret(Secret::new(v.into_bytes())), cx);
        key.update(cx, |s, cx| s.set_value("", window, cx));
    }

    fn flow_card(&self, l: &Look, cx: &mut Context<Self>) -> Div {
        let Some(f) = &self.flow else { return div() };
        let title = match f.kind {
            FlowKind::Verify => tr!("mx-verify-title"),
            FlowKind::Recovery => tr!("mx-recovery-title"),
        };
        let mut body = frame(title, l);
        if f.done {
            let text = match f.kind {
                FlowKind::Verify => tr!("mx-verify-done"),
                FlowKind::Recovery => tr!("mx-recovery-done"),
            };
            body = body.child(div().child(text));
        } else {
            match (&f.prompt, f.kind) {
                (None, FlowKind::Verify) => body = body.child(div().text_color(l.text2()).child(tr!("mx-verify-wait"))),
                (None, FlowKind::Recovery) => body = body.child(div().text_color(l.text2()).child(tr!("mx-wait"))),
                (Some(Prompt::Compare { emoji }), _) => {
                    let row_of = emoji.iter().map(|(sym, name)| {
                        div()
                            .flex()
                            .flex_col()
                            .items_center()
                            .w(px(64.))
                            .gap_1()
                            .child(div().text_size(px(28.)).child(sym.clone()))
                            .child(div().text_size(px(11.)).text_color(l.text2()).child(name.clone()))
                    });
                    let busy = f.busy;
                    body = body
                        .child(div().text_color(l.text2()).child(tr!("mx-verify-compare")))
                        .child(div().flex().flex_wrap().gap_2().children(row_of))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(
                                    button("mx-verify-yes", tr!("mx-verify-match"), true, l)
                                        .when(!busy, |b| b.on_click(cx.listener(|p, _, _, cx| p.answer(Answer::Confirm(true), cx)))),
                                )
                                .child(
                                    link("mx-verify-no", tr!("mx-verify-mismatch"), l)
                                        .when(!busy, |b| b.on_click(cx.listener(|p, _, _, cx| p.answer(Answer::Confirm(false), cx)))),
                                ),
                        );
                }
                (Some(Prompt::RecoveryKey), _) => {
                    let busy = f.busy;
                    body = body.child(div().text_color(l.text2()).child(tr!("mx-recovery-hint"))).child(Input::new(&f.key)).child(
                        button("mx-recovery-go", tr!("mx-recovery-go"), true, l)
                            .when(!busy, |b| b.on_click(cx.listener(|p, _, window, cx| p.submit_key(window, cx)))),
                    );
                }
                (Some(_), _) => body = body.child(div().text_color(l.warning()).child(tr!("mx-err-other"))),
            }
        }
        if let Some(e) = &f.error {
            body = body.child(div().text_color(l.warning()).child(e.clone()));
        }
        body.child(link("mx-flow-close", tr!(if f.done { "mx-close" } else { "settings-cancel" }), l).on_click(cx.listener(|p, _, _, cx| {
            p.flow = None;
            cx.notify();
        })))
    }

    /// «Шифрованные беседы»: три выбора; закреплено политикой — только подпись.
    fn encrypted_row(&self, l: &Look, cx: &mut Context<Self>) -> Div {
        let pinned = crate::security::policy().encrypted.is_some();
        let now = crate::prefs::with(cx, encrypted);
        let hint = if pinned { tr!("tg-policy") } else { tr!("mx-e2e-hint") };
        let items = [(E2eStore::Search, "mx-e2e-search"), (E2eStore::Store, "mx-e2e-store"), (E2eStore::Memory, "mx-e2e-memory")].map(|(v, key)| {
            seg(("mx-e2e", v.code() as usize), tr!(key), now == v, !pinned, l)
                .when(!pinned && now != v, |s| s.on_click(cx.listener(move |_, _, _, cx| cx.emit(PanelEvent::Encrypted(v)))))
        });
        row(tr!("mx-e2e"), Some(hint), seg_group(items, l), l)
    }
}

fn frame(title: String, l: &Look) -> Div {
    div()
        .flex()
        .flex_col()
        .gap_3()
        .p_4()
        .rounded(l.px(Metric::RadiusOverlay))
        .border_1()
        .border_color(l.border())
        .bg(l.panel())
        .child(div().text_size(px(15.)).font_weight(FontWeight::SEMIBOLD).child(title))
}

impl Render for Panel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _ = window;
        let l = crate::roles::look(cx);
        if !BUILT || forbidden() {
            return div().into_any_element();
        }
        let on = crate::prefs::with(cx, enabled);
        let pinned = crate::security::policy().matrix.is_some();
        let hint = if pinned { tr!("tg-policy") } else { tr!("mx-channel-hint") };
        // Заголовок «Каналы бесед» рисует панель Telegram; её нет — свой.
        let heading = !crate::account_telegram::BUILT || crate::account_telegram::forbidden();
        let mut out = div().flex().flex_col().when(heading, |d| d.child(crate::settings_tab::group(tr!("tg-channels"), &l))).child(row(
            tr!("mx-channel"),
            Some(hint),
            toggle("mx-toggle", on, &l).when(!pinned, |t| t.on_click(cx.listener(move |_, _, _, cx| cx.emit(PanelEvent::Toggled(!on))))),
            &l,
        ));
        if !on {
            return out.into_any_element();
        }
        let lines: Vec<Line> = cx.try_global::<Lines>().map(|g| g.0.clone()).unwrap_or_default();
        for (n, line) in lines.iter().filter(|x| x.account.channel.0 == MATRIX).enumerate() {
            let state = match &line.state {
                Some(ChannelState::Online) => tr!("tg-state-online"),
                Some(ChannelState::Connecting) | None => tr!("tg-state-connecting"),
                Some(ChannelState::Offline { retry_in }) => tr!("tg-state-offline", secs = retry_in.to_string()),
                Some(ChannelState::Problem(e)) => error_text(e),
            };
            let name = if line.name.is_empty() { line.account.account.0.clone() } else { line.name.clone() };
            let online = matches!(line.state, Some(ChannelState::Online));
            let (a1, a2, a3) = (line.account.clone(), line.account.clone(), line.account.clone());
            let actions = div()
                .flex()
                .items_center()
                .gap_3()
                .when(online, |d| {
                    d.child(
                        link(("mx-verify", n), tr!("mx-verify"), &l)
                            .on_click(cx.listener(move |p, _, window, cx| p.start_flow(a1.clone(), FlowKind::Verify, window, cx))),
                    )
                    .child(
                        link(("mx-recovery", n), tr!("mx-recovery"), &l)
                            .on_click(cx.listener(move |p, _, window, cx| p.start_flow(a2.clone(), FlowKind::Recovery, window, cx))),
                    )
                })
                .child(
                    link(("mx-sign-out", n), tr!("tg-sign-out"), &l)
                        .on_click(cx.listener(move |_, _, _, cx| cx.emit(PanelEvent::SignOut(a3.clone())))),
                );
            out = out.child(crate::settings_channels::account_row(line.state.as_ref(), name, state, actions, &l));
        }
        if self.flow.is_some() {
            out = out.child(div().pt_3().child(self.flow_card(&l, cx)));
        }
        if self.card.is_some() {
            out = out.child(div().pt_3().child(self.card(&l, cx)));
        } else {
            out =
                out.child(div().pt_3().child(
                    button("mx-add", format!("+ {}", tr!("mx-add")), false, &l).on_click(cx.listener(|p, _, window, cx| p.start(window, cx))),
                ));
        }
        out = out.child(div().pt_3().child(self.encrypted_row(&l, cx)));
        out.into_any_element()
    }
}

/// Код отказа для журнала: без имени, пароля и ключа.
fn error_code(e: &ChannelError) -> String {
    match e {
        ChannelError::Denied(c) => format!("denied {c}"),
        ChannelError::Wait { secs } => format!("wait {secs}"),
        ChannelError::Network(_) => "network".into(),
        ChannelError::SignedOut(_) => "signed out".into(),
        other => format!("{other}"),
    }
}

/// «Проблема → причина» словами (Fluent); коды — в ядре (`postman_core::channel::matrix`, PLG-04): интерфейс от
/// крейта плагина не зависит.
pub(crate) fn error_text(e: &ChannelError) -> String {
    match e {
        ChannelError::Denied(code) => match code.as_str() {
            mx::BAD_SERVER => tr!("mx-err-server"),
            mx::BAD_LOGIN => tr!("mx-err-login"),
            mx::BAD_PASSWORD => tr!("mx-err-password"),
            mx::NO_LOGIN_FLOW => tr!("mx-err-unsupported"),
            mx::BROWSER_TIMEOUT => tr!("mx-err-browser"),
            mx::NO_CROSS_SIGNING => tr!("mx-err-cross-signing"),
            mx::VERIFY_CANCELLED => tr!("mx-err-verify"),
            mx::BAD_RECOVERY_KEY => tr!("mx-err-recovery"),
            _ => tr!("mx-err-denied"),
        },
        ChannelError::Wait { secs } => tr!("mx-err-wait", secs = secs.to_string()),
        ChannelError::Network(_) => tr!("mx-err-network"),
        ChannelError::SignedOut(_) => tr!("tg-err-signed-out"),
        // Файла набора нет (PLG-14): канал не установлен; установка компонента приходит с докачкой (PLG-15).
        ChannelError::Missing(_) => tr!("comp-missing"),
        _ => tr!("mx-err-other"),
    }
}

#[cfg(test)]
mod tests {
    use postman_core::channel::ChannelError;
    use postman_core::prefs::Prefs;

    use super::{BUILT, enabled, error_code, error_text, forbidden};

    /// По умолчанию — выключен; включённый — если собран и не запрещён политикой.
    #[test]
    fn channel_switch() {
        assert!(!enabled(&Prefs::default()));
        let mut on = Prefs::default();
        on.channels.matrix = true;
        assert_eq!(enabled(&on), BUILT && !forbidden());
        assert!(crate::account_telegram::channel_on("demo", &Prefs::default()));
        assert!(!crate::account_telegram::channel_on(super::MATRIX, &Prefs::default()));
    }

    #[test]
    fn errors_are_words() {
        assert_eq!(error_code(&ChannelError::Network("@anna:example.test".into())), "network");
        for code in ["server-invalid", "password-invalid", "recovery-key-invalid", "no-cross-signing"] {
            assert!(!error_text(&ChannelError::Denied(code.into())).is_empty());
        }
    }
}
