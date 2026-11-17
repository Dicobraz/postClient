//! Канал Telegram (ADR 0012, CH-03): слова отказов входа и довесок сообщения — здесь, чтобы интерфейс не зависел от
//! крейта плагина (PLG-05 — канал в отдельном процессе набора `postman-plugins`, как Matrix в PLG-04). Протокол
//! (MTProto, `grammers`) знает только плагин; ядро довесок не разбирает — это общий формат данных для вида беседы
//! (CH-04) и ответа серверу.

use serde::{Deserialize, Serialize};

use super::Extra;

// Коды отказов входа и подключения в `ChannelError::Denied` — интерфейс переводит их в слова (Fluent), не завися от
// крейта плагина (PLG-05 — плагин в отдельном процессе).

/// В сборке нет ключа приложения (`api_id`/`api_hash`).
pub const NO_API_KEY: &str = "no-api-key";
/// Номер телефона неверный.
pub const BAD_PHONE: &str = "phone-invalid";
/// Код входа неверный.
pub const BAD_CODE: &str = "code-invalid";
/// Код входа просрочен.
pub const CODE_EXPIRED: &str = "code-expired";
/// Пароль двухэтапной проверки неверный.
pub const BAD_PASSWORD: &str = "password-invalid";
/// Номер не зарегистрирован: регистрация — в официальном приложении.
pub const SIGN_UP: &str = "sign-up-required";

/// Версия формата довеска.
pub const VERSION: u32 = 1;

/// Довесок сообщения Telegram (ADR 0012 п. 4): чего нет в общей модели, но нужно виду беседы (CH-04) и ответу
/// серверу — альбом (номера частей), кнопки бота (подпись и действие), опрос (вопрос, варианты, голоса, итоги).
/// Ядро хранит байты, не разбирая; читает их канал и вид Telegram ([`TgExtra::parse`]).
///
/// Нажать кнопку и проголосовать — `Action::Extra` с [`TgAct`] (тоже JSON): вид собирает его из довеска
/// ([`TgAct::press`], [`TgAct::vote`]), канал выполняет и отвечает `Done::Reply`.
///
/// Формат — JSON с номером версии `v`; новые поля — с `serde(default)`, старое читается как было. Нечего
/// сказать — `extra` пуст.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TgExtra {
    pub v: u32,
    /// Сообщение — альбом: части на сервере.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album: Option<Album>,
    /// Кнопки бота под сообщением или вместо клавиатуры.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keyboard: Option<Keyboard>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poll: Option<Poll>,
}

/// Альбом (`grouped_id`): у Telegram каждая часть — своё сообщение, в модели — одно (номер — первой части).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Album {
    pub group: i64,
    /// Номера частей по возрастанию (первая — номер сообщения модели); вложения — в том же порядке.
    pub parts: Vec<i32>,
    /// Часть с подписью (правка подписи уходит ей).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caption: Option<i32>,
}

/// Кнопки бота: `inline` — под сообщением (`replyInlineMarkup`), иначе — клавиатура вместо обычной
/// (`replyKeyboardMarkup`: нажатие — отправить подпись кнопки).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Keyboard {
    pub inline: bool,
    pub rows: Vec<Vec<Button>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
    /// Клавиатура прячется после нажатия.
    #[serde(default)]
    pub single_use: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Button {
    pub text: String,
    pub action: ButtonAction,
}

/// Что делает кнопка.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ButtonAction {
    /// Ответ боту (`messages.getBotCallbackAnswer` с `data`); `password` — бот просит пароль двухэтапной проверки.
    Callback {
        data: Vec<u8>,
        #[serde(default)]
        password: bool,
    },
    /// Открыть ссылку.
    Url {
        url: String,
    },
    /// Вход на сайт через Telegram (`urlAuth`).
    LoginUrl {
        url: String,
    },
    /// Мини-приложение бота.
    WebView {
        url: String,
    },
    /// Встроенный запрос бота в поле ввода.
    SwitchInline {
        query: String,
        #[serde(default)]
        same_peer: bool,
    },
    Game,
    Buy,
    /// Карточка человека.
    UserProfile {
        user: i64,
    },
    /// Скопировать текст.
    Copy {
        text: String,
    },
    /// Кнопка клавиатуры бота: отправить её подпись.
    Text,
    RequestPhone,
    RequestGeo,
    RequestPoll {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        quiz: Option<bool>,
    },
    RequestPeer,
    Disabled,
}

/// Опрос и его итоги на момент сообщения (обновляются `updateMessagePoll`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Poll {
    pub id: i64,
    pub question: String,
    pub answers: Vec<PollAnswer>,
    #[serde(default)]
    pub closed: bool,
    #[serde(default)]
    pub multiple: bool,
    /// Викторина: один верный ответ, после голоса — `correct` и пояснение.
    #[serde(default)]
    pub quiz: bool,
    /// Голоса не анонимны.
    #[serde(default)]
    pub public_voters: bool,
    /// Закроется (Unix, мс).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub close_date: Option<i64>,
    /// Всего проголосовало; `None` — итогов сервер не дал.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub solution: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PollAnswer {
    pub text: String,
    /// Номер варианта у сервера (`messages.sendVote`).
    pub option: Vec<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voters: Option<u32>,
    /// Мой голос.
    #[serde(default)]
    pub chosen: bool,
    #[serde(default)]
    pub correct: bool,
}

/// Что сделать по довеску (`Action::Extra`, решение владельца 2026-11-14).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "do", rename_all = "snake_case")]
pub enum TgAct {
    /// Нажать кнопку бота (действие — как в [`Keyboard`]): ответ боту — `messages.getBotCallbackAnswer`, ссылка —
    /// ответом без запроса к серверу.
    Press { action: ButtonAction },
    /// Голос в опросе: варианты — [`PollAnswer::option`]; пусто — отозвать голос (`messages.sendVote`).
    Vote { options: Vec<Vec<u8>> },
}

impl TgAct {
    /// `payload` для `Action::Extra`.
    pub fn to_extra(&self) -> Extra {
        Extra(serde_json::to_vec(self).unwrap_or_default())
    }

    pub fn parse(e: &Extra) -> Option<Self> {
        serde_json::from_slice(&e.0).ok()
    }

    /// Нажать кнопку.
    pub fn press(b: &Button) -> Extra {
        TgAct::Press { action: b.action.clone() }.to_extra()
    }

    /// Голос за варианты опроса; пусто — отозвать.
    pub fn vote(options: &[&PollAnswer]) -> Extra {
        TgAct::Vote { options: options.iter().map(|a| a.option.clone()).collect() }.to_extra()
    }
}

impl Poll {
    /// Голос с вариантами `options` можно отдать: опрос открыт, вариантов не больше, чем позволяет опрос, все — его.
    /// Отказ — имя ошибки сервера (то же ответил бы Telegram).
    pub fn check_vote(&self, options: &[Vec<u8>]) -> Result<(), &'static str> {
        if self.closed {
            return Err("MESSAGE_POLL_CLOSED");
        }
        if options.len() > 1 && !self.multiple {
            return Err("OPTIONS_TOO_MUCH");
        }
        if options.iter().any(|o| !self.answers.iter().any(|a| a.option == *o)) {
            return Err("OPTION_INVALID");
        }
        Ok(())
    }

    /// Я уже голосовал.
    pub fn voted(&self) -> bool {
        self.answers.iter().any(|a| a.chosen)
    }
}

impl TgExtra {
    pub fn is_empty(&self) -> bool {
        self.album.is_none() && self.keyboard.is_none() && self.poll.is_none()
    }

    /// Байты `extra`; нечего сказать — пусто.
    pub fn to_extra(&self) -> Extra {
        if self.is_empty() {
            return Extra::default();
        }
        let me = TgExtra { v: VERSION, ..self.clone() };
        Extra(serde_json::to_vec(&me).unwrap_or_default())
    }

    /// Довесок Telegram из `extra`; пусто или чужое — `None`.
    pub fn parse(e: &Extra) -> Option<Self> {
        if e.0.is_empty() {
            return None;
        }
        serde_json::from_slice(&e.0).ok()
    }

    /// Довесок сообщения или пустой.
    pub fn of(e: &Extra) -> Self {
        Self::parse(e).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Пусто — пустые байты; туда и обратно; старое без новых полей читается.
    #[test]
    fn round_trip() {
        assert!(TgExtra::default().to_extra().0.is_empty());
        assert_eq!(TgExtra::parse(&Extra::default()), None);
        let e = TgExtra {
            v: 0,
            album: Some(Album { group: 7, parts: vec![10, 11], caption: Some(10) }),
            keyboard: Some(Keyboard {
                inline: true,
                rows: vec![vec![
                    Button { text: "Да".into(), action: ButtonAction::Callback { data: b"yes".to_vec(), password: false } },
                    Button { text: "Сайт".into(), action: ButtonAction::Url { url: "https://example.test".into() } },
                ]],
                placeholder: None,
                single_use: false,
            }),
            poll: None,
        };
        let bytes = e.to_extra();
        let back = TgExtra::parse(&bytes).unwrap();
        assert_eq!(back.v, VERSION);
        assert_eq!(back.album, e.album);
        assert_eq!(back.keyboard, e.keyboard);
        let old = TgExtra::parse(&Extra(br#"{"v":1,"poll":{"id":1,"question":"?","answers":[{"text":"a","option":[48]}]}}"#.to_vec())).unwrap();
        assert!(!old.poll.unwrap().voted());
    }

    /// Действие довеска туда и обратно; голос — по свойствам опроса.
    #[test]
    fn act_and_vote_rules() {
        let b = Button { text: "Да".into(), action: ButtonAction::Callback { data: b"yes".to_vec(), password: false } };
        assert_eq!(TgAct::parse(&TgAct::press(&b)), Some(TgAct::Press { action: b.action.clone() }));
        assert_eq!(TgAct::parse(&Extra(b"not json".to_vec())), None);
        let a = |o: u8| PollAnswer { text: "x".into(), option: vec![o], ..PollAnswer::default() };
        let mut p = Poll { answers: vec![a(b'0'), a(b'1')], ..Poll::default() };
        assert_eq!(TgAct::parse(&TgAct::vote(&[&p.answers[1]])), Some(TgAct::Vote { options: vec![vec![b'1']] }));
        assert_eq!(p.check_vote(&[]), Ok(()), "отзыв голоса");
        assert_eq!(p.check_vote(&[vec![b'0']]), Ok(()));
        assert_eq!(p.check_vote(&[vec![b'0'], vec![b'1']]), Err("OPTIONS_TOO_MUCH"));
        assert_eq!(p.check_vote(&[vec![b'7']]), Err("OPTION_INVALID"));
        p.multiple = true;
        assert_eq!(p.check_vote(&[vec![b'0'], vec![b'1']]), Ok(()));
        p.closed = true;
        assert_eq!(p.check_vote(&[vec![b'0']]), Err("MESSAGE_POLL_CLOSED"));
    }
}
