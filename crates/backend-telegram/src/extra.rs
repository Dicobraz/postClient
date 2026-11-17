//! Довесок сообщения Telegram в `Message::extra` (ADR 0012 п. 4): формат — в ядре
//! (`postman_core::channel::telegram`, PLG-05 — интерфейс собирает и читает его без крейта плагина), здесь — перевод
//! ответов сервера TL в довесок: кнопки бота ([`keyboard`]), опрос и итоги ([`poll`], [`apply_results`],
//! [`renew`]). Ядро хранит байты, не разбирая; читает их канал и вид Telegram.

use grammers_tl_types as tl;
pub use postman_core::channel::telegram::{Album, Button, ButtonAction, Keyboard, Poll, PollAnswer, TgAct, TgExtra, VERSION};

fn text(t: &tl::enums::TextWithEntities) -> String {
    let tl::enums::TextWithEntities::Entities(t) = t;
    t.text.clone()
}

/// Кнопки сообщения (`reply_markup`); убрать клавиатуру, «ответить» — не кнопки.
pub fn keyboard(m: &tl::enums::ReplyMarkup) -> Option<Keyboard> {
    use tl::enums::ReplyMarkup as R;
    match m {
        R::ReplyInlineMarkup(k) => {
            let rows: Vec<Vec<Button>> = k
                .rows
                .iter()
                .map(|r| {
                    let tl::enums::KeyboardInlineButtonRow::Row(r) = r;
                    r.buttons
                        .iter()
                        .map(|b| {
                            let tl::enums::KeyboardInlineButton::Button(b) = b;
                            Button { text: b.text.clone(), action: inline_action(&b.r#type) }
                        })
                        .collect()
                })
                .collect();
            (!rows.is_empty()).then_some(Keyboard { inline: true, rows, placeholder: None, single_use: false })
        }
        R::ReplyKeyboardMarkup(k) => {
            let rows: Vec<Vec<Button>> = k
                .rows
                .iter()
                .map(|r| {
                    let tl::enums::KeyboardButtonRow::Row(r) = r;
                    r.buttons
                        .iter()
                        .map(|b| {
                            let tl::enums::KeyboardButton::Button(b) = b;
                            Button { text: b.text.clone(), action: reply_action(&b.r#type) }
                        })
                        .collect()
                })
                .collect();
            (!rows.is_empty()).then(|| Keyboard { inline: false, rows, placeholder: k.placeholder.clone(), single_use: k.single_use })
        }
        R::ReplyKeyboardHide(_) | R::ReplyKeyboardForceReply(_) => None,
    }
}

fn inline_action(t: &tl::enums::InlineButtonType) -> ButtonAction {
    use tl::enums::InlineButtonType as T;
    match t {
        T::Url(u) => ButtonAction::Url { url: u.url.clone() },
        T::UrlAuth(u) => ButtonAction::LoginUrl { url: u.url.clone() },
        T::InputInlineButtonTypeUrlAuth(u) => ButtonAction::LoginUrl { url: u.url.clone() },
        T::WebView(u) => ButtonAction::WebView { url: u.url.clone() },
        T::Callback(c) => ButtonAction::Callback { data: c.data.clone(), password: c.requires_password },
        T::Game => ButtonAction::Game,
        T::Buy => ButtonAction::Buy,
        T::SwitchInline(s) => ButtonAction::SwitchInline { query: s.query.clone(), same_peer: s.same_peer },
        T::UserProfile(u) => ButtonAction::UserProfile { user: u.user_id },
        T::InputInlineButtonTypeUserProfile(u) => match &u.user_id {
            tl::enums::InputUser::User(u) => ButtonAction::UserProfile { user: u.user_id },
            _ => ButtonAction::Disabled,
        },
        T::Copy(c) => ButtonAction::Copy { text: c.copy_text.clone() },
        T::Disabled => ButtonAction::Disabled,
    }
}

fn reply_action(t: &tl::enums::ButtonType) -> ButtonAction {
    use tl::enums::ButtonType as T;
    match t {
        T::Default => ButtonAction::Text,
        T::RequestPhone => ButtonAction::RequestPhone,
        T::RequestGeoLocation => ButtonAction::RequestGeo,
        T::RequestPoll(p) => ButtonAction::RequestPoll { quiz: p.quiz },
        T::RequestPeer(_) | T::InputButtonTypeRequestPeer(_) => ButtonAction::RequestPeer,
        T::SimpleWebView(w) => ButtonAction::WebView { url: w.url.clone() },
    }
}

/// Опрос сообщения с итогами.
pub fn poll(p: &tl::enums::Poll, results: &tl::enums::PollResults) -> Poll {
    let tl::enums::Poll::Poll(p) = p;
    let mut out = Poll {
        id: p.id,
        question: text(&p.question),
        answers: p
            .answers
            .iter()
            .filter_map(|a| match a {
                tl::enums::PollAnswer::Answer(a) => Some(PollAnswer { text: text(&a.text), option: a.option.clone(), ..PollAnswer::default() }),
                tl::enums::PollAnswer::InputPollAnswer(_) => None,
            })
            .collect(),
        closed: p.closed,
        multiple: p.multiple_choice,
        quiz: p.quiz,
        public_voters: p.public_voters,
        close_date: p.close_date.map(|d| i64::from(d) * 1000),
        ..Poll::default()
    };
    apply_results(&mut out, results);
    out
}

/// Новые итоги опроса; `min` — без моих отметок (их сервер не прислал — остаются прежние).
pub fn apply_results(p: &mut Poll, results: &tl::enums::PollResults) {
    let tl::enums::PollResults::Results(r) = results;
    if let Some(total) = r.total_voters {
        p.total = u32::try_from(total).ok();
    }
    if let Some(s) = &r.solution {
        p.solution = Some(s.clone());
    }
    for v in r.results.iter().flatten() {
        let tl::enums::PollAnswerVoters::Voters(v) = v;
        if let Some(a) = p.answers.iter_mut().find(|a| a.option == v.option) {
            a.voters = v.voters.and_then(|n| u32::try_from(n).ok()).or(a.voters);
            if !r.min {
                a.chosen = v.chosen;
                a.correct = v.correct;
            }
        }
    }
}

/// Опрос по `updateMessagePoll`: сам опрос (если сервер прислал) и итоги; мои отметки при `min` — прежние.
pub fn renew(p: &mut Poll, poll: Option<&tl::enums::Poll>, results: &tl::enums::PollResults) {
    let Some(np) = poll else {
        apply_results(p, results);
        return;
    };
    let old = std::mem::take(p);
    *p = self::poll(np, results);
    let tl::enums::PollResults::Results(r) = results;
    for a in &mut p.answers {
        if let Some(o) = old.answers.iter().find(|o| o.option == a.option) {
            if r.min {
                a.chosen = o.chosen;
                a.correct = o.correct;
            }
            a.voters = a.voters.or(o.voters);
        }
    }
    p.total = p.total.or(old.total);
    p.solution = p.solution.take().or(old.solution);
}
