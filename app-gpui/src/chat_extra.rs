//! Довесок сообщения канала для ленты (CH-04, ADR 0012 «Решения для владельца» п. 3): кнопки ботов и опросы.
//!
//! Формат довеска (`Message::extra`) — у канала; ядро его не разбирает. Здесь довесок каждого канала переводится в
//! общий вид для ленты ([`View`]): кнопки рядами и опрос, у каждой кнопки и у голоса — готовый `payload` для
//! `Action::Extra`. Нажать можно только при `ChannelFeatures::extra` и только то, что канал выполнит
//! («Нет у сервера — нет на экране»): остальных кнопок в ленте нет.

use postman_core::channel::Extra;

/// Кнопка бота.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Button {
    pub(crate) text: String,
    /// Что отправить каналу (`Action::Extra`).
    pub(crate) payload: Extra,
    /// Кнопка-ссылка: канал ответит ссылкой, открывает её пользователь.
    pub(crate) link: bool,
}

/// Вариант опроса.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Choice {
    pub(crate) text: String,
    /// Голосов; `None` — итогов канал не дал.
    pub(crate) votes: Option<u32>,
    /// Мой голос.
    pub(crate) chosen: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Poll {
    pub(crate) question: String,
    pub(crate) multiple: bool,
    pub(crate) closed: bool,
    pub(crate) choices: Vec<Choice>,
}

impl Poll {
    pub(crate) fn voted(&self) -> bool {
        self.choices.iter().any(|c| c.chosen)
    }

    /// Всего голосов по вариантам (для долей).
    pub(crate) fn total(&self) -> u32 {
        self.choices.iter().filter_map(|c| c.votes).sum()
    }
}

/// Довесок сообщения в общем виде.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct View {
    pub(crate) rows: Vec<Vec<Button>>,
    pub(crate) poll: Option<Poll>,
}

impl View {
    fn is_empty(&self) -> bool {
        self.rows.iter().all(Vec::is_empty) && self.poll.is_none()
    }
}

/// Довесок сообщения канала `channel`; пусто, чужое или нечего показать — `None`.
pub(crate) fn view(channel: &str, extra: &Extra) -> Option<View> {
    if extra.0.is_empty() {
        return None;
    }
    let v = match channel {
        postman_demo::channel::ID => demo(extra)?,
        crate::account_telegram::TELEGRAM => telegram(extra)?,
        _ => return None,
    };
    (!v.is_empty()).then_some(v)
}

/// `payload` голоса: варианты по номерам (пусто — отозвать голос).
pub(crate) fn vote(channel: &str, extra: &Extra, picks: &[usize]) -> Option<Extra> {
    match channel {
        postman_demo::channel::ID => Some(postman_demo::channel::DemoExtra::vote(picks)),
        crate::account_telegram::TELEGRAM => {
            use postman_core::channel::telegram::{TgAct, TgExtra};
            let poll = TgExtra::parse(extra)?.poll?;
            let answers: Vec<_> = picks.iter().filter_map(|i| poll.answers.get(*i)).collect();
            (answers.len() == picks.len()).then(|| TgAct::vote(&answers))
        }
        _ => {
            let _ = extra;
            None
        }
    }
}

/// Одна строка для списка бесед, когда у сообщения нет текста: вопрос опроса.
pub(crate) fn summary(channel: &str, extra: &Extra) -> Option<String> {
    view(channel, extra)?.poll.map(|p| p.question)
}

fn demo(extra: &Extra) -> Option<View> {
    use postman_demo::channel::{DemoButton, DemoExtra};
    let e = DemoExtra::parse(extra)?;
    // Кнопки демо — один ряд по две.
    let buttons: Vec<Button> = e
        .buttons
        .iter()
        .enumerate()
        .map(|(n, b)| {
            let (text, link) = match b {
                DemoButton::Callback { text, .. } => (text, false),
                DemoButton::Url { text, .. } => (text, true),
            };
            Button { text: text.clone(), payload: DemoExtra::press(n), link }
        })
        .collect();
    let poll = e.poll.map(|p| Poll {
        question: p.question,
        multiple: p.multiple,
        closed: p.closed,
        choices: p.options.into_iter().map(|o| Choice { text: o.text, votes: Some(o.votes), chosen: o.chosen }).collect(),
    });
    Some(View { rows: buttons.chunks(2).map(<[Button]>::to_vec).collect(), poll })
}

/// Довесок Telegram: формат — в ядре (`postman_core::channel::telegram`, PLG-05), интерфейс от крейта плагина
/// не зависит.
fn telegram(extra: &Extra) -> Option<View> {
    use postman_core::channel::telegram::{ButtonAction, TgAct, TgExtra};
    let e = TgExtra::parse(extra)?;
    // Кнопки под сообщением, которые канал выполнит: ответ боту (без пароля) и ссылки. Клавиатура вместо обычной,
    // мини-приложения, игры и оплата — не из Postman.
    let rows = e
        .keyboard
        .filter(|k| k.inline)
        .map(|k| {
            k.rows
                .iter()
                .map(|row| {
                    row.iter()
                        .filter_map(|b| {
                            let link = match &b.action {
                                ButtonAction::Callback { password: false, .. } => false,
                                ButtonAction::Url { .. } | ButtonAction::LoginUrl { .. } => true,
                                _ => return None,
                            };
                            Some(Button { text: b.text.clone(), payload: TgAct::press(b), link })
                        })
                        .collect::<Vec<_>>()
                })
                .filter(|row| !row.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let poll = e.poll.map(|p| Poll {
        question: p.question,
        multiple: p.multiple,
        closed: p.closed,
        choices: p.answers.into_iter().map(|a| Choice { text: a.text, votes: a.voters, chosen: a.chosen }).collect(),
    });
    Some(View { rows, poll })
}

#[cfg(test)]
mod tests {
    use postman_core::channel::Extra;
    use postman_demo::channel::{DemoButton, DemoExtra, DemoOption, DemoPoll, ID};

    use super::{summary, view, vote};

    /// Довесок демо-канала: кнопки рядами с готовым `payload`, ссылка помечена; опрос — с итогами и моим голосом.
    #[test]
    fn demo_extra_becomes_buttons_and_poll() {
        let e = DemoExtra {
            buttons: vec![
                DemoButton::Callback { text: "Перезапустить".into(), data: "restart".into() },
                DemoButton::Callback { text: "Отменить".into(), data: "cancel".into() },
                DemoButton::Url { text: "Журнал".into(), url: "https://ci.example.test/build/128".into() },
            ],
            poll: Some(DemoPoll {
                question: "Когда созвон?".into(),
                multiple: false,
                closed: false,
                options: vec![
                    DemoOption { text: "Понедельник".into(), votes: 1, chosen: false },
                    DemoOption { text: "Среда".into(), votes: 3, chosen: true },
                ],
            }),
        }
        .to_extra();
        let v = view(ID, &e).unwrap();
        assert_eq!(v.rows.iter().map(Vec::len).collect::<Vec<_>>(), [2, 1], "по две кнопки в ряд");
        assert_eq!(v.rows[0][1].payload, DemoExtra::press(1));
        assert!(v.rows[1][0].link && !v.rows[0][0].link);
        let poll = v.poll.unwrap();
        assert!(poll.voted() && !poll.closed && !poll.multiple);
        assert_eq!((poll.total(), poll.choices[1].votes), (4, Some(3)));
        assert_eq!(summary(ID, &e).as_deref(), Some("Когда созвон?"));
        assert_eq!(vote(ID, &e, &[0]), Some(DemoExtra::vote(&[0])));
        assert_eq!(vote(ID, &e, &[]), Some(DemoExtra::vote(&[])), "пусто — отозвать голос");
    }

    /// Пустой довесок, чужой формат и неизвестный канал — ничего на экране.
    #[test]
    fn foreign_extra_shows_nothing() {
        assert!(view(ID, &Extra::default()).is_none());
        assert!(view(ID, &Extra(b"\xff\xfe".to_vec())).is_none());
        assert!(view("matrix", &DemoExtra::press(0)).is_none());
        assert!(vote("matrix", &Extra::default(), &[0]).is_none());
    }

    /// Telegram: под сообщением — только кнопки, которые канал выполнит (ответ боту без пароля, ссылка); голос —
    /// вариантами сервера.
    #[test]
    fn telegram_extra_keeps_only_pressable_buttons() {
        use postman_core::channel::telegram::{Button, ButtonAction, Keyboard, Poll, PollAnswer, TgAct, TgExtra};
        let button = |text: &str, action| Button { text: text.into(), action };
        let keyboard = Keyboard {
            inline: true,
            rows: vec![
                vec![
                    button("Да", ButtonAction::Callback { data: b"y".to_vec(), password: false }),
                    button("Передать", ButtonAction::Callback { data: b"t".to_vec(), password: true }),
                ],
                vec![button("Игра", ButtonAction::Game)],
                vec![button("Сайт", ButtonAction::Url { url: "https://example.test/".into() })],
            ],
            placeholder: None,
            single_use: false,
        };
        let answers = vec![
            PollAnswer { text: "А".into(), option: vec![0], voters: Some(2), ..PollAnswer::default() },
            PollAnswer { text: "Б".into(), option: vec![1], ..PollAnswer::default() },
        ];
        let poll = Poll { id: 1, question: "Вопрос".into(), answers: answers.clone(), ..Poll::default() };
        let e = TgExtra { keyboard: Some(keyboard.clone()), poll: Some(poll), ..TgExtra::default() }.to_extra();
        let v = view("telegram", &e).unwrap();
        assert_eq!(v.rows.iter().map(|r| r.iter().map(|b| b.text.as_str()).collect::<Vec<_>>()).collect::<Vec<_>>(), [vec!["Да"], vec!["Сайт"]]);
        assert!(v.rows[1][0].link);
        assert_eq!(v.rows[0][0].payload, TgAct::press(&keyboard.rows[0][0]));
        assert_eq!(v.poll.as_ref().unwrap().choices[0].votes, Some(2));
        assert_eq!(vote("telegram", &e, &[1]), Some(TgAct::vote(&[&answers[1]])));
        assert_eq!(vote("telegram", &e, &[7]), None, "варианта нет — голоса нет");
        // Клавиатура вместо обычной — не под сообщением.
        let reply = TgExtra { keyboard: Some(Keyboard { inline: false, ..keyboard }), ..TgExtra::default() }.to_extra();
        assert!(view("telegram", &reply).is_none());
    }
}
