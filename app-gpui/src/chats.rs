//! Беседы каналов на экране почты (CH-04, ADR 0012 «Вид»: гибрид; DESIGN «Каналы»): каркас тот же — папки | список |
//! чтение. Учётки каналов — разделами дерева (`chat_tree.rs`), беседы чата — в списке (`chat_list.rs`), лента с полем
//! ввода — в области чтения (`chat_pane.rs`). Здесь — данные: снимок бесед всех каналов из базы профиля (в фоне),
//! что показано (`Scope`), запросы учётке канала и события её потока.
//!
//! Правило «Нет у сервера — нет на экране»: всё, что зависит от канала, решает [`shown`] по `ChannelFeatures`.

use std::sync::Arc;

use gpui_kit::*;
use pets_i18n::tr;
use pets_ui::Level as Toast;
use postman_cache::Cache;
use postman_core::channel::mail::{MailAccount, MailSource};
use postman_core::channel::smart::{self, Item};
use postman_core::channel::{
    Action, Address, ChanAccount, ChannelError, ChannelFeatures, ChannelFolder, ChannelStore, ConversationSource, Counts, FolderRef, Message,
    MessageId, ParticipantId, Request, Role, Update, outbox,
};
use postman_core::sync::Command;

use crate::Mailer;
use crate::chat_list::{ChatList, ListEvent};
use crate::chat_pane::{ChatPane, PaneEvent};

/// Опись части (ADR 0005, `parts.rs`): тексты, команды беседы, сцены, сброс перед чужими сценами.
pub(crate) const PART: crate::part::Part =
    crate::part::Part { name: "chats", texts: TEXTS, commands: COMMANDS, handlers, scenes: scenes::SCENES, reset: Some(scenes::reset) };

/// Действия над открытой беседой канала — кнопки командной строки и палитра.
// Имя — область действий (`chat::…`), как в id команд: так обработчики читаются и их находит `docs/PARITY.md`.
pub(crate) mod chat {
    gpui_kit::actions!(chat, [Pin, Mute, MarkRead, Reply]);
}

const COMMANDS: &[pets_commands::Command] = {
    use crate::commands::{MAIL, cmd};
    // Без клавиш: общих сочетаний у мессенджеров нет, а клавиши писем над беседой канала молчат.
    &[cmd("chat.pin", &[], MAIL), cmd("chat.mute", &[], MAIL), cmd("chat.mark_read", &[], MAIL), cmd("chat.reply", &[], MAIL)]
};

fn handlers(root: Div, cx: &mut Context<Mailer>) -> Div {
    root.on_action(cx.listener(|this, _: &chat::Pin, _, cx| this.chat_command(BarAct::Pin, cx)))
        .on_action(cx.listener(|this, _: &chat::Mute, _, cx| this.chat_command(BarAct::Mute, cx)))
        .on_action(cx.listener(|this, _: &chat::MarkRead, _, cx| this.chat_command(BarAct::Read, cx)))
        .on_action(cx.listener(|this, _: &chat::Reply, _, cx| this.chat_command(BarAct::Reply, cx)))
        // Стрелки списка писем над списком бесед канала ходят по беседам (письма скрыты).
        .capture_action(cx.listener(|this, _: &crate::actions::mail::Next, _, cx| this.chat_key(Step::By(1), cx)))
        .capture_action(cx.listener(|this, _: &crate::actions::mail::Prev, _, cx| this.chat_key(Step::By(-1), cx)))
        .capture_action(cx.listener(|this, _: &crate::actions::mail::First, _, cx| this.chat_key(Step::First, cx)))
        .capture_action(cx.listener(|this, _: &crate::actions::mail::Last, _, cx| this.chat_key(Step::Last, cx)))
}

/// Шаг по списку бесед.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    By(isize),
    First,
    Last,
}

/// Команда беседы канала из командной строки или палитры.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BarAct {
    Pin,
    Mute,
    Read,
    Reply,
}

const TEXTS: &[pets_i18n::Resource] = &[("ru", include_str!("../i18n/ru/chats.ftl")), ("en", include_str!("../i18n/en/chats.ftl"))];

/// Человек умной папки «Человек во всех каналах»: имя и его адреса (почта, телефоны, адреса в мессенджерах).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Person {
    pub(crate) name: String,
    pub(crate) addresses: Vec<String>,
}

/// Умная папка поверх всех каналов (ADR 0012 «Вид»): в ней беседы чатов и почты.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Smart {
    /// «Непрочитанное везде».
    Unread,
    /// «Упоминания меня».
    Mentions,
    /// «Закреплённые».
    Pinned,
    /// «Человек во всех каналах» — с карточки контакта.
    Person(Person),
}

impl Smart {
    /// Три постоянные папки по порядку дерева (людей добавляет пользователь).
    pub(crate) const FIXED: [Smart; 3] = [Smart::Unread, Smart::Mentions, Smart::Pinned];

    fn folder(&self) -> smart::Folder {
        match self {
            Smart::Unread => smart::Folder::Unread,
            Smart::Mentions => smart::Folder::Mentions,
            Smart::Pinned => smart::Folder::Pinned,
            Smart::Person(p) => smart::Folder::Person(p.addresses.clone()),
        }
    }

    /// Название в дереве.
    pub(crate) fn title(&self) -> String {
        match self {
            Smart::Unread => tr!("chats-smart-unread"),
            Smart::Mentions => tr!("chats-smart-mentions"),
            Smart::Pinned => tr!("chats-smart-pinned"),
            Smart::Person(p) => p.name.clone(),
        }
    }
}

/// Что показано в списке вместо папки почты.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Scope {
    /// «Все беседы» учётки канала.
    All(ChanAccount),
    /// Папка Telegram, пространство Matrix.
    Folder(ChanAccount, FolderRef),
    /// Умная папка: беседы всех каналов и почты.
    Smart(Smart),
}

impl Scope {
    /// Учётка канала, чьи беседы показаны; у умной папки её нет.
    pub(crate) fn account(&self) -> Option<&ChanAccount> {
        match self {
            Scope::All(a) | Scope::Folder(a, _) => Some(a),
            Scope::Smart(_) => None,
        }
    }

    /// Смешанный список (значок канала у строки).
    pub(crate) fn mixed(&self) -> bool {
        matches!(self, Scope::Smart(_))
    }
}

/// Строка списка: беседа, имя автора последнего сообщения, моё ли оно.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Row {
    pub(crate) item: Item,
    pub(crate) author: String,
    pub(crate) mine: bool,
    /// Вопрос опроса в последнем сообщении (если оно без текста).
    pub(crate) poll: String,
}

impl Row {
    pub(crate) fn at(&self) -> &Address {
        &self.item.summary.address
    }

    /// Беседа почты (в умной папке): строка — отправитель, тема, начало текста.
    pub(crate) fn is_mail(&self) -> bool {
        self.at().channel.0 == postman_core::channel::MAIL
    }

    /// Первая строка: название беседы; у письма — отправитель.
    pub(crate) fn title(&self) -> String {
        if self.is_mail() && !self.author.is_empty() { self.author.clone() } else { self.item.summary.conversation.title.clone() }
    }

    /// Строки 2–3 списка: «Автор: последнее сообщение»; в личной беседе чужое — без автора; у письма — тема и начало
    /// текста.
    pub(crate) fn last_line(&self) -> String {
        if self.is_mail() {
            let (subject, text) = (self.item.summary.conversation.title.trim(), self.item.summary.preview.trim());
            return if text.is_empty() { subject.to_owned() } else { format!("{subject}\n{text}") };
        }
        // Сообщение без текста с опросом — его вопрос (довесок разбирает `chat_extra`).
        let poll = if self.poll.is_empty() { String::new() } else { tr!("chats-poll-line", question = self.poll.clone()) };
        let text = if self.item.summary.preview.is_empty() { &poll } else { &self.item.summary.preview };
        let direct = self.item.summary.conversation.kind == postman_core::channel::ConversationKind::Direct;
        if self.mine {
            tr!("chats-last-mine", text = text.clone())
        } else if direct || self.author.is_empty() {
            text.clone()
        } else {
            tr!("chats-last-author", author = self.author.clone(), text = text.clone())
        }
    }
}

/// Учётка канала в дереве.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Account {
    pub(crate) chan: ChanAccount,
    /// Кто я в учётке.
    pub(crate) name: String,
    pub(crate) features: ChannelFeatures,
    pub(crate) folders: Vec<ChannelFolder>,
}

/// Беседы всех каналов на момент чтения базы.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Snapshot {
    pub(crate) accounts: Vec<Account>,
    pub(crate) rows: Vec<Row>,
    /// Умные папки, показанные в дереве, и их беседы (чаты и почта).
    pub(crate) smart: Vec<(Smart, Vec<Row>)>,
    /// Какие из трёх постоянных умных папок показывать (порядок [`Smart::FIXED`]).
    pub(crate) shown: [bool; 3],
}

impl Snapshot {
    /// Беседа входит в то, что показано.
    fn holds(&self, scope: &Scope, row: &Row) -> bool {
        match scope {
            Scope::All(chan) => row.at().chan() == *chan,
            Scope::Folder(chan, folder) => {
                let id = &row.at().conversation;
                row.at().chan() == *chan
                    && (row.item.summary.conversation.folders.contains(folder)
                        || self
                            .accounts
                            .iter()
                            .filter(|a| a.chan == *chan)
                            .flat_map(|a| &a.folders)
                            .any(|f| f.id == *folder && f.conversations.contains(id)))
            }
            Scope::Smart(_) => false,
        }
    }

    /// Строки списка: закреплённые сверху, дальше новые первыми (порядок снимка — `smart::arrange`).
    pub(crate) fn rows_of(&self, scope: &Scope) -> Vec<Row> {
        match scope {
            Scope::Smart(s) => self.smart.iter().find(|(x, _)| x == s).map(|(_, rows)| rows.clone()).unwrap_or_default(),
            _ => self.rows.iter().filter(|r| self.holds(scope, r)).cloned().collect(),
        }
    }

    /// Счётчик у строки дерева: непрочитанное и упоминания, кроме заглушённых.
    pub(crate) fn counts(&self, scope: &Scope) -> Counts {
        let items: Vec<Item> = match scope {
            Scope::Smart(s) => self.smart.iter().filter(|(x, _)| x == s).flat_map(|(_, rows)| rows.iter().map(|r| r.item.clone())).collect(),
            _ => self.rows.iter().filter(|r| self.holds(scope, r)).map(|r| r.item.clone()).collect(),
        };
        smart::totals(&items)
    }

    pub(crate) fn account(&self, chan: &ChanAccount) -> Option<&Account> {
        self.accounts.iter().find(|a| a.chan == *chan)
    }

    /// Люди умных папок «Человек во всех каналах».
    pub(crate) fn people(&self) -> Vec<Person> {
        self.smart
            .iter()
            .filter_map(|(s, _)| match s {
                Smart::Person(p) => Some(p.clone()),
                _ => None,
            })
            .collect()
    }
}

/// Ключи базы профиля (`local`): какие умные папки показывать и люди «Человек во всех каналах».
const SMART_SHOWN: &str = "ui/smart/shown";
const SMART_PEOPLE: &str = "ui/smart/people";

/// Какие постоянные умные папки показывать; записи нет — все.
pub(crate) fn smart_shown(cache: &Cache) -> Result<[bool; 3], ChannelError> {
    let stored = cache.local(SMART_SHOWN)?.map(|d| d.value);
    Ok(match stored.as_deref() {
        Some([a, b, c]) => [*a != 0, *b != 0, *c != 0],
        _ => [true; 3],
    })
}

pub(crate) fn set_smart_shown(cache: &Cache, shown: [bool; 3]) -> Result<(), ChannelError> {
    Ok(cache.put_local(&postman_cache::LocalPut { key: SMART_SHOWN.into(), value: shown.map(u8::from).to_vec(), parts: Vec::new() })?)
}

/// Люди умных папок: строка на человека — имя и адреса через табуляцию.
pub(crate) fn smart_people(cache: &Cache) -> Result<Vec<Person>, ChannelError> {
    let Some(doc) = cache.local(SMART_PEOPLE)? else { return Ok(Vec::new()) };
    let text = String::from_utf8_lossy(&doc.value).into_owned();
    Ok(text
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t').map(str::trim).filter(|f| !f.is_empty());
            let name = fields.next()?.to_owned();
            Some(Person { name, addresses: fields.map(str::to_owned).collect() })
        })
        .collect())
}

pub(crate) fn set_smart_people(cache: &Cache, people: &[Person]) -> Result<(), ChannelError> {
    let clean = |s: &str| s.replace(['\t', '\n', '\r'], " ");
    let text: Vec<String> =
        people.iter().map(|p| std::iter::once(clean(&p.name)).chain(p.addresses.iter().map(|a| clean(a))).collect::<Vec<_>>().join("\t")).collect();
    Ok(cache.put_local(&postman_cache::LocalPut { key: SMART_PEOPLE.into(), value: text.join("\n").into_bytes(), parts: Vec::new() })?)
}

/// Снимок из базы профиля (I/O — только из фона). `on` — включён ли канал (настройка и политика); `mail` — почта для
/// умных папок (без неё в них только чаты).
pub(crate) fn snapshot(cache: &Cache, now: i64, on: &dyn Fn(&str) -> bool, mail: Option<&MailSource>) -> Result<Snapshot, ChannelError> {
    let stores: Vec<ChannelStore> = ChannelStore::all(cache)?.into_iter().filter(|s| on(&s.channel().0)).collect();
    let mut accounts = Vec::new();
    let mut me = Vec::new();
    for s in &stores {
        let who = s.me()?;
        let name = match &who {
            Some(p) if !p.name.is_empty() => p.name.clone(),
            _ => s.name()?,
        };
        me.push(who.map(|p| p.id));
        accounts.push(Account { chan: s.chan(), name, features: s.features()?, folders: s.folders()? });
    }
    // Строка беседы чата; беседа выключенного канала — `None`.
    let chat_row = |item: Item| -> Result<Option<Row>, ChannelError> {
        let chan = item.summary.address.chan();
        let Some(i) = stores.iter().position(|s| s.chan() == chan) else { return Ok(None) };
        let (author, mine) = match &item.summary.author {
            Some(id) => {
                let name = stores[i].participant(id)?.map(|p| p.name).filter(|n| !n.is_empty()).unwrap_or_else(|| id.0.clone());
                (name, me[i].as_ref() == Some(id))
            }
            None => (String::new(), false),
        };
        // Последнее сообщение без текста: вопрос опроса из довеска канала.
        let poll = if item.summary.preview.is_empty() {
            let last = stores[i].timeline(&item.summary.address.conversation, None, 1)?.pop();
            last.as_ref().and_then(|e| e.message()).and_then(|m| crate::chat_extra::summary(&chan.channel.0, &m.extra)).unwrap_or_default()
        } else {
            String::new()
        };
        Ok(Some(Row { item, author, mine, poll }))
    };
    let mut rows = Vec::new();
    for item in smart::all_chats(cache, now)? {
        rows.extend(chat_row(item)?);
    }
    // Умные папки — пока подключён хоть один канал: без каналов дерево почты прежнее.
    let shown = smart_shown(cache)?;
    let mut smart_rows = Vec::new();
    if !accounts.is_empty() {
        let fixed = Smart::FIXED.into_iter().zip(shown).filter(|(_, on)| *on).map(|(s, _)| s);
        for s in fixed.chain(smart_people(cache)?.into_iter().map(Smart::Person)) {
            let mut list = Vec::new();
            for item in smart::folder(cache, mail, &s.folder(), now)? {
                if item.summary.address.channel.0 != postman_core::channel::MAIL {
                    list.extend(chat_row(item)?);
                    continue;
                }
                // Письмо: отправитель последнего письма беседы — именем.
                let author = match mail.map(|m| m.timeline(&item.summary.address, None, 1)).transpose()?.and_then(|mut l| l.pop()) {
                    Some(line) => {
                        let at = Address { message: Some(line.id().clone()), ..item.summary.address.clone() };
                        let row = mail.map(|m| m.find(&at)).transpose()?.flatten().map(|(row, _)| row);
                        row.map(|r| if r.from_name.is_empty() { r.from_addr } else { r.from_name }).unwrap_or_default()
                    }
                    None => String::new(),
                };
                list.push(Row { item, author, mine: false, poll: String::new() });
            }
            smart_rows.push((s, list));
        }
    }
    Ok(Snapshot { accounts, rows, smart: smart_rows, shown })
}

/// Письмо беседы почты для области чтения (умная папка): последнее письмо беседы. I/O — из фона.
pub(crate) fn mail_row(mail: &MailSource, at: &Address) -> Result<Option<postman_cache::EmailRow>, ChannelError> {
    let Some(line) = mail.timeline(at, None, 1)?.pop() else { return Ok(None) };
    let at = Address { message: Some(line.id().clone()), ..at.clone() };
    Ok(mail.find(&at)?.map(|(row, _)| row))
}

/// Что из своего у канала показать у сообщения и беседы — по возможностям учётки («Нет у сервера — нет на экране»).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Shown {
    pub(crate) reply: bool,
    pub(crate) edit: bool,
    pub(crate) delete_for_all: bool,
    pub(crate) react: bool,
    /// Отметка «прочитано» собеседником у моих сообщений.
    pub(crate) receipts: bool,
    pub(crate) typing: bool,
    /// Поле ввода: роль позволяет писать.
    pub(crate) write: bool,
}

/// Действия над сообщением `m` сейчас (`now`, Unix, мс) и над беседой с моей ролью `role`.
pub(crate) fn shown(f: &ChannelFeatures, role: Role, m: Option<&Message>, now: i64) -> Shown {
    let age = m.map_or(0, |m| now.saturating_sub(m.sent));
    let sent = m.is_some_and(|m| !outbox::is_pending(&m.id));
    let mine = m.is_some_and(|m| m.mine);
    Shown {
        reply: f.reply_to && sent && role.can_write(),
        edit: mine && sent && f.edit.open(age),
        delete_for_all: mine && sent && f.delete_for_all.open(age),
        react: sent && f.reactions != postman_core::channel::Reactions::No,
        receipts: f.read_receipts,
        typing: f.typing,
        write: role.can_write(),
    }
}

/// Ошибка канала словами (Fluent): «Проблема → причина».
pub(crate) fn error_text(e: &ChannelError) -> String {
    match e {
        ChannelError::Network(_) => tr!("chats-err-network"),
        ChannelError::SignedOut(_) => tr!("chats-err-signed-out"),
        ChannelError::Wait { secs } => tr!("chats-err-wait", secs = secs.to_string()),
        ChannelError::Denied(_) => tr!("chats-err-denied"),
        ChannelError::NotFound => tr!("chats-err-not-found"),
        ChannelError::TooLarge { .. } => tr!("chats-err-too-large"),
        ChannelError::Unsupported => tr!("chats-err-unsupported"),
        ChannelError::Protocol(_) | ChannelError::Storage(_) | ChannelError::Plugin(_) => tr!("chats-err-other"),
        // Файла набора нет (PLG-14): канал не установлен.
        ChannelError::Missing(_) => tr!("comp-missing"),
    }
}

/// Начало текста одной строкой (цитата ответа, строка над полем ввода).
pub(crate) fn preview(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(160).collect()
}

/// Сейчас, Unix, мс.
pub(crate) fn now_ms() -> i64 {
    jiff::Timestamp::now().as_millisecond()
}

/// Беседы каналов экрана почты: снимок, что показано, список и лента.
pub(crate) struct Chats {
    pub(crate) snapshot: Arc<Snapshot>,
    pub(crate) scope: Option<Scope>,
    pub(crate) list: Entity<ChatList>,
    pub(crate) pane: Entity<ChatPane>,
    /// «Медиа беседы» открытой беседы канала (MED-02).
    pub(crate) media: Entity<crate::thread_media::MediaView>,
    /// Снимок читается; `again` — за это время база изменилась, прочитать ещё раз.
    loading: bool,
    again: bool,
    /// Номер запроса учётке (`Request::Act`): ответ — `Update::Acted` с ним.
    ticket: u64,
    /// Запросы действий довеска (кнопка бота, голос): их ответ — ленте.
    extra: Vec<u64>,
    /// Запросы выгрузки файлов из поля ввода: их итог — ленте.
    uploads: Vec<u64>,
    /// В умной папке открыта беседа почты: в области чтения — письмо, кнопки — письма.
    pub(crate) mail_open: bool,
}

impl Chats {
    pub(crate) fn new(cache: Cache, worker: Arc<std::sync::Mutex<postman_worker::Client>>, cx: &mut Context<Mailer>) -> (Self, Vec<Subscription>) {
        let list = cx.new(|_| ChatList::new());
        let media = cx.new(|_| crate::thread_media::MediaView::new(cache.clone(), worker));
        let pane = cx.new(|_| ChatPane::new(cache, media.clone()));
        let subs = vec![
            cx.subscribe(&media, |this: &mut Mailer, _, ev: &crate::thread_media::MediaEvent, cx| this.on_media(ev, cx)),
            // Режим и счётчик «Медиа беседы» сменились — лента чата перерисовывает вкладки под шапкой.
            cx.observe(&media, |this: &mut Mailer, _, cx| this.chats.pane.update(cx, |_, cx| cx.notify())),
            cx.subscribe(&list, |this: &mut Mailer, _, ev: &ListEvent, cx| match ev {
                ListEvent::Open(at) => this.open_chat(at.clone(), cx),
            }),
            cx.subscribe(&pane, |this: &mut Mailer, _, ev: &PaneEvent, cx| this.on_chat_pane(ev, cx)),
        ];
        (
            Self {
                snapshot: Arc::default(),
                scope: None,
                list,
                pane,
                media,
                loading: false,
                again: false,
                ticket: 0,
                extra: Vec::new(),
                uploads: Vec::new(),
                mail_open: false,
            },
            subs,
        )
    }
}

impl Mailer {
    /// В списке — беседы канала, а не папка почты.
    pub(crate) fn chat_active(&self) -> bool {
        self.chats.scope.is_some()
    }

    /// Перечитать беседы каналов из базы (в фоне) и раздать дереву и списку.
    pub(crate) fn chats_reload(&mut self, cx: &mut Context<Self>) {
        if self.chats.loading {
            self.chats.again = true;
            return;
        }
        self.chats.loading = true;
        let cache = self.cache.clone();
        let prefs = crate::prefs::get(cx);
        let (telegram, matrix) = (crate::account_telegram::enabled(&prefs), crate::account_matrix::enabled(&prefs));
        let (mail_accounts, me) = self.mail_ctx(cx);
        let task = cx.background_spawn(async move {
            let on = |id: &str| match id {
                crate::account_telegram::TELEGRAM => telegram,
                crate::account_matrix::MATRIX => matrix,
                _ => true,
            };
            let mail = MailSource::new(cache.clone(), mail_accounts, me);
            snapshot(&cache, now_ms(), &on, Some(&mail))
        });
        cx.spawn(async move |this, cx| {
            let loaded = task.await;
            let _ = this.update(cx, |m, cx| {
                m.chats.loading = false;
                match loaded {
                    Ok(s) => m.chats_loaded(s, cx),
                    Err(e) => tracing::warn!("chats: conversations not read: {e}"),
                }
                if std::mem::take(&mut m.chats.again) {
                    m.chats_reload(cx);
                }
            });
        })
        .detach();
    }

    fn chats_loaded(&mut self, snapshot: Snapshot, cx: &mut Context<Self>) {
        let snapshot = Arc::new(snapshot);
        let before = std::mem::replace(&mut self.chats.snapshot, snapshot.clone());
        // Учётка вышла или канал выключен, пока её беседы показаны, — назад к почте.
        let gone = match self.chats.scope.as_ref() {
            // Умная папка была в прошлом снимке и пропала (выключили, убрали человека); только что созданной папки
            // человека в снимке, начатом до её записи, ещё нет — это не «пропала».
            Some(Scope::Smart(s)) => before.smart.iter().any(|(x, _)| x == s) && !snapshot.smart.iter().any(|(x, _)| x == s),
            Some(scope) => scope.account().is_some_and(|a| before.account(a).is_some() && snapshot.account(a).is_none()),
            None => false,
        };
        if gone {
            self.leave_chats(cx);
            if let Some(inbox) = crate::live::inbox(&self.folders.read(cx).mailboxes) {
                self.open_mailbox(inbox, cx);
            }
        }
        // «Настройки → Каналы»: какие умные папки показаны.
        cx.set_global(crate::settings_channels::SmartShown(snapshot.shown));
        let scope = self.chats.scope.clone();
        self.folders.update(cx, |f, cx| {
            f.chats = snapshot.clone();
            f.chat_scope = scope.clone();
            cx.notify();
        });
        if let Some(scope) = &scope {
            let rows = snapshot.rows_of(scope);
            self.chats.list.update(cx, |l, cx| l.set_rows(rows, cx));
        }
        cx.notify();
    }

    /// Строка дерева: «Все беседы» учётки или её папка.
    pub(crate) fn open_chat_scope(&mut self, scope: Scope, cx: &mut Context<Self>) {
        self.show_mail_tab(cx);
        self.drop_search(cx);
        // Письмо и выделение почты убраны: действия над письмами в беседах каналов не действуют.
        // Скрытый список писем — пустой: «Выделить всё» и клавиши писем ничего не заденут; папка вернётся в `open_mailbox`.
        self.list.update(cx, |l, cx| l.show_search(Some(Vec::new()), cx));
        self.reading.update(cx, |r, cx| r.clear(cx));
        self.chats.scope = Some(scope.clone());
        self.chat_bar(true, cx);
        let rows = self.chats.snapshot.rows_of(&scope);
        let mixed = scope.mixed();
        let empty = match &scope {
            Scope::Smart(Smart::Unread) => ("chats-empty-unread-title", "chats-empty-unread-text"),
            Scope::Smart(Smart::Mentions) => ("chats-empty-mentions-title", "chats-empty-mentions-text"),
            Scope::Smart(Smart::Pinned) => ("chats-empty-pinned-title", "chats-empty-pinned-text"),
            Scope::Smart(Smart::Person(_)) => ("chats-empty-person-title", "chats-empty-person-text"),
            Scope::All(_) | Scope::Folder(..) => ("chats-empty-list-title", "chats-empty-list-text"),
        };
        self.chats.list.update(cx, |l, cx| {
            l.selected = None;
            l.mixed = mixed;
            l.empty = empty;
            l.set_rows(rows, cx);
        });
        self.chats.mail_open = false;
        self.chats.pane.update(cx, |p, cx| p.close(cx));
        self.folders.update(cx, |f, cx| {
            f.chat_scope = Some(scope);
            f.results = None;
            cx.notify();
        });
        cx.notify();
    }

    /// Назад к почте: открывается папка (`open_mailbox`), поиск.
    pub(crate) fn leave_chats(&mut self, cx: &mut Context<Self>) {
        if self.chats.scope.take().is_none() {
            return;
        }
        self.chats.mail_open = false;
        self.chats.pane.update(cx, |p, cx| p.close(cx));
        self.chat_bar(false, cx);
        self.folders.update(cx, |f, cx| {
            f.chat_scope = None;
            cx.notify();
        });
        cx.notify();
    }

    /// Мои ящики и адреса — для бесед почты в умных папках.
    fn mail_ctx(&self, cx: &App) -> (Vec<MailAccount>, postman_core::filters::Me) {
        let folders = self.folders.read(cx);
        let own = folders.own.clone().unwrap_or_else(|| postman_demo::ME.to_owned());
        let mut accounts = vec![MailAccount::new(None, &own)];
        accounts.extend(folders.accounts.iter().map(|a| MailAccount::new(Some(&a.id), &a.name)));
        (accounts, self.list.read(cx).me.clone())
    }

    /// Беседа почты из умной папки: последнее письмо — в область чтения, кнопки — письма.
    fn open_mail_conversation(&mut self, at: Address, cx: &mut Context<Self>) {
        let (accounts, me) = self.mail_ctx(cx);
        let cache = self.cache.clone();
        let task = cx.background_spawn(async move { mail_row(&MailSource::new(cache.clone(), accounts, me), &at) });
        cx.spawn(async move |this, cx| {
            let found = task.await;
            let _ = this.update(cx, |m, cx| match found {
                Ok(Some(row)) if m.chat_active() => {
                    m.chats.mail_open = true;
                    m.chats.pane.update(cx, |p, cx| p.close(cx));
                    m.chat_bar(false, cx);
                    m.open_letter(row, cx);
                    cx.notify();
                }
                Ok(_) => {}
                Err(e) => tracing::warn!("chats: mail conversation not read: {e}"),
            });
        })
        .detach();
    }

    /// Беседа из списка — в область чтения.
    pub(crate) fn open_chat(&mut self, at: Address, cx: &mut Context<Self>) {
        self.chats.list.update(cx, |l, cx| {
            l.selected = Some(at.clone());
            cx.notify();
        });
        if at.channel.0 == postman_core::channel::MAIL {
            return self.open_mail_conversation(at, cx);
        }
        if std::mem::take(&mut self.chats.mail_open) {
            self.reading.update(cx, |r, cx| r.clear(cx));
            self.chat_bar(true, cx);
        }
        self.chats.pane.update(cx, |p, cx| p.open(at, cx));
        cx.notify();
    }

    /// ↑ / ↓, Home / End в списке бесед: соседняя беседа — в область чтения.
    fn chat_step(&mut self, to: Step, cx: &mut Context<Self>) {
        let next = self.chats.list.update(cx, |l, cx| l.step(to, cx));
        if let Some(at) = next {
            self.open_chat(at, cx);
        }
    }

    /// Письма изменились: умные папки с беседами почты — перечитать.
    pub(crate) fn chats_mail_changed(&mut self, cx: &mut Context<Self>) {
        if !self.chats.snapshot.smart.is_empty() {
            self.chats_reload(cx);
        }
    }

    /// «Настройки → Каналы»: показывать ли постоянную умную папку номер `i` в дереве.
    pub(crate) fn chat_smart_shown(&mut self, i: usize, on: bool, cx: &mut Context<Self>) {
        let mut shown = self.chats.snapshot.shown;
        let Some(slot) = shown.get_mut(i) else { return };
        *slot = on;
        let cache = self.cache.clone();
        self.chat_store(cx, move || set_smart_shown(&cache, shown));
    }

    /// «Человек во всех каналах» с карточки контакта: папка человека — в дерево и в список.
    pub(crate) fn chat_person(&mut self, person: Person, cx: &mut Context<Self>) {
        if self.chats.snapshot.accounts.is_empty() {
            return self.toast(Toast::Info, tr!("chats-person-no-channels"), cx);
        }
        let mut people = self.chats.snapshot.people();
        people.retain(|p| p.name != person.name);
        people.push(person.clone());
        let cache = self.cache.clone();
        self.set_section(crate::Section::Mail, cx);
        self.open_chat_scope(Scope::Smart(Smart::Person(person)), cx);
        self.chat_store(cx, move || set_smart_people(&cache, &people));
    }

    /// Убрать папку человека из дерева.
    pub(crate) fn chat_person_forget(&mut self, person: &Person, cx: &mut Context<Self>) {
        let mut people = self.chats.snapshot.people();
        people.retain(|p| p != person);
        if self.chats.scope == Some(Scope::Smart(Smart::Person(person.clone()))) {
            self.leave_chats(cx);
            if let Some(inbox) = crate::live::inbox(&self.folders.read(cx).mailboxes) {
                self.open_mailbox(inbox, cx);
            }
        }
        let cache = self.cache.clone();
        self.chat_store(cx, move || set_smart_people(&cache, &people));
    }

    /// Запись настроек умных папок в базу профиля (в фоне), затем — перечитать снимок.
    fn chat_store(&mut self, cx: &mut Context<Self>, write: impl FnOnce() -> Result<(), ChannelError> + Send + 'static) {
        let task = cx.background_spawn(async move { write() });
        cx.spawn(async move |this, cx| {
            let done = task.await;
            let _ = this.update(cx, |m, cx| match done {
                Ok(()) => m.chats_reload(cx),
                Err(e) => m.toast(Toast::Error, error_text(&e), cx),
            });
        })
        .detach();
    }

    /// Запрос учётке канала; потока нет (канал выключен, нет связи с потоком) — тост.
    pub(crate) fn chat_request(&mut self, account: ChanAccount, request: Request, cx: &mut Context<Self>) -> bool {
        let sent = self.send_channel(Command::Channel { account, request: Box::new(request) });
        if !sent {
            self.toast(Toast::Warning, tr!("chats-err-not-running"), cx);
        }
        sent
    }

    fn chat_act(&mut self, account: ChanAccount, action: Action, cx: &mut Context<Self>) -> u64 {
        self.chats.ticket += 1;
        let ticket = self.chats.ticket;
        self.chat_request(account, Request::Act { ticket, action }, cx);
        ticket
    }

    fn on_chat_pane(&mut self, ev: &PaneEvent, cx: &mut Context<Self>) {
        match ev {
            PaneEvent::Request(account, request) => {
                self.chat_request(account.clone(), request.clone(), cx);
            }
            PaneEvent::Upload(account, meta, bytes) => {
                self.chats.ticket += 1;
                let ticket = self.chats.ticket;
                if self.chat_request(account.clone(), Request::Upload { ticket, meta: meta.clone(), bytes: bytes.clone() }, cx) {
                    self.chats.uploads.push(ticket);
                } else {
                    self.chats.pane.update(cx, |p, cx| p.uploaded(Err(ChannelError::Network(String::new())), cx));
                }
            }
            PaneEvent::Act(account, action) => {
                self.chat_act(account.clone(), action.clone(), cx);
            }
            PaneEvent::Extra(account, action) => {
                let ticket = self.chat_act(account.clone(), action.clone(), cx);
                self.chats.extra.push(ticket);
            }
            PaneEvent::Toast(level, text) => self.toast(*level, text.clone(), cx),
            PaneEvent::Mute(at, on) => self.chat_mute(at.clone(), *on, cx),
            PaneEvent::Pin(at, on) => self.chat_pin(at.clone(), *on, cx),
            PaneEvent::Copy(text) => crate::screens::to_clipboard(text.clone(), cx),
        }
    }

    /// Заглушить беседу или вернуть звук: на сервере, если канал умеет, иначе — отметкой в базе профиля.
    fn chat_mute(&mut self, at: Address, on: bool, cx: &mut Context<Self>) {
        let Some(features) = self.chats.snapshot.account(&at.chan()).map(|a| a.features.clone()) else { return };
        let cache = self.cache.clone();
        let task = cx.background_spawn({
            let at = at.clone();
            async move { smart::set_mute(&cache, &at, &features, on.then_some(i64::MAX)) }
        });
        cx.spawn(async move |this, cx| {
            let done = task.await;
            let _ = this.update(cx, |m, cx| match done {
                Ok(action) => {
                    if let Some(a) = action {
                        let _ = m.chat_act(at.chan(), a, cx);
                    }
                    m.toast(Toast::Info, tr!(if on { "chats-muted" } else { "chats-unmuted" }), cx);
                    m.chats_reload(cx);
                    m.chats.pane.update(cx, |p, cx| p.reload(cx));
                }
                Err(e) => m.toast(Toast::Error, error_text(&e), cx),
            });
        })
        .detach();
    }

    /// Закрепить или открепить: на сервере или в базе; в базе закреплённых не больше предела — тост.
    fn chat_pin(&mut self, at: Address, on: bool, cx: &mut Context<Self>) {
        let Some(features) = self.chats.snapshot.account(&at.chan()).map(|a| a.features.clone()) else { return };
        let cache = self.cache.clone();
        let task = cx.background_spawn({
            let at = at.clone();
            async move { smart::set_pin(&cache, &at, &features, on) }
        });
        cx.spawn(async move |this, cx| {
            let done = task.await;
            let _ = this.update(cx, |m, cx| match done {
                Ok(Ok(action)) => {
                    if let Some(a) = action {
                        let _ = m.chat_act(at.chan(), a, cx);
                    }
                    m.toast(Toast::Info, tr!(if on { "chats-pinned" } else { "chats-unpinned" }), cx);
                    m.chats_reload(cx);
                    m.chats.pane.update(cx, |p, cx| p.reload(cx));
                }
                Ok(Err(smart::Full)) => m.toast(Toast::Warning, tr!("chats-pin-full", n = postman_core::pin::LIMIT as i64), cx),
                Err(e) => m.toast(Toast::Error, error_text(&e), cx),
            });
        })
        .detach();
    }

    /// Событие потока учётки канала (`channels.rs`, до общей обработки).
    pub(crate) fn chat_update(&mut self, account: &ChanAccount, update: &Update, cx: &mut Context<Self>) {
        match update {
            Update::Changed { conversations, gone } => {
                self.chats_reload(cx);
                self.chats.pane.update(cx, |p, cx| p.changed(account, conversations, gone, cx));
            }
            Update::Typing { conversation, who, on } => self.chats.pane.update(cx, |p, cx| p.typing(account, conversation, who, *on, cx)),
            Update::Connected { .. } | Update::Features(_) | Update::SignedOut => {
                self.chats_reload(cx);
                self.chats.pane.update(cx, |p, cx| p.reload(cx));
            }
            Update::Sent { .. } | Update::Queued { .. } | Update::Unsent { .. } => self.chats.pane.update(cx, |p, cx| p.reload(cx)),
            Update::SendFailed { error, .. } => {
                self.toast(Toast::Error, tr!("chats-send-failed", why = error_text(error)), cx);
                self.chats.pane.update(cx, |p, cx| p.reload(cx));
            }
            Update::Uploaded { ticket, result } => {
                if let Some(i) = self.chats.uploads.iter().position(|t| t == ticket) {
                    self.chats.uploads.swap_remove(i);
                    let result = result.clone();
                    self.chats.pane.update(cx, |p, cx| p.uploaded(result, cx));
                }
            }
            Update::Acted { ticket, result } => {
                let extra = self.chats.extra.iter().position(|t| t == ticket).map(|i| self.chats.extra.swap_remove(i)).is_some();
                match result {
                    // Ответ бота на кнопку или голос — ленте: подсказка, предупреждение, ссылка.
                    Ok(postman_core::channel::Done::Reply(reply)) if extra => {
                        let reply = reply.clone();
                        self.chats.pane.update(cx, |p, cx| p.extra_reply(reply, cx));
                    }
                    Ok(_) => {}
                    Err(e) => self.toast(Toast::Error, error_text(e), cx),
                }
            }
            _ => {}
        }
    }

    /// Клавиша списка писем, пока показаны беседы канала: шаг по беседам, письмам она не достаётся.
    fn chat_key(&mut self, to: Step, cx: &mut Context<Self>) {
        if self.chat_active() && self.section == crate::Section::Mail {
            cx.stop_propagation();
            self.chat_step(to, cx);
        }
    }

    /// Команда беседы: над открытой беседой канала; беседа не открыта — подсказка.
    fn chat_command(&mut self, act: BarAct, cx: &mut Context<Self>) {
        let state = self.chats.pane.read(cx).state();
        let Some((at, muted, pinned)) = state.filter(|_| self.chat_active()) else {
            return self.toast(Toast::Info, tr!("chats-pick-first"), cx);
        };
        match act {
            BarAct::Pin => self.chat_pin(at, !pinned, cx),
            BarAct::Mute => self.chat_mute(at, !muted, cx),
            BarAct::Read => self.chats.pane.update(cx, |p, cx| p.mark_read(cx)),
            BarAct::Reply => self.chats.pane.update(cx, |p, cx| p.focus_reply(cx)),
        }
    }

    /// Командная строка: кнопки беседы канала вместо кнопок письма.
    fn chat_bar(&mut self, on: bool, cx: &mut Context<Self>) {
        self.bar.update(cx, |b, cx| {
            if b.chat != on {
                b.chat = on;
                cx.notify();
            }
        });
    }

    /// Esc в беседе канала: сначала ответ и правка у поля ввода, затем строки подтверждения.
    pub(crate) fn chat_escape(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.chat_active() {
            return false;
        }
        if self.chats.pane.update(cx, |p, cx| p.escape(cx)) {
            return true;
        }
        // Панель чтения скрыта (Настройки → Чтение): беседа на месте списка — назад к списку.
        let hidden = crate::prefs::get(cx).reading.pane == postman_core::prefs::Pane::Hidden;
        if hidden && (self.chats.mail_open || self.chats.pane.read(cx).address().is_some()) {
            if std::mem::take(&mut self.chats.mail_open) {
                self.reading.update(cx, |r, cx| r.clear(cx));
                self.chat_bar(true, cx);
            }
            self.chats.pane.update(cx, |p, cx| p.close(cx));
            self.chats.list.update(cx, |l, cx| {
                l.selected = None;
                cx.notify();
            });
            cx.notify();
            return true;
        }
        false
    }
}

/// id сообщения очереди отправки → номер отправки (для «Отменить»).
pub(crate) fn pending_client(id: &MessageId) -> Option<u64> {
    outbox::is_pending(id).then(|| id.0.trim_start_matches(|c: char| !c.is_ascii_digit()).parse().ok()).flatten()
}

/// Автор сообщения — я (по отметке канала или по id).
pub(crate) fn is_mine(m: &Message, me: Option<&ParticipantId>) -> bool {
    m.mine || me == Some(&m.author)
}

/// Сцены `--screens` бесед каналов (макеты «✓ Каналы»): демо-канал запускает первая сцена.
pub(crate) mod scenes {
    use gpui_kit::*;
    use postman_core::channel::ChanAccount;

    use super::{Person, Scope, Smart};
    use crate::screens::Scene;
    use crate::{Mailer, Section};

    pub(crate) const SCENES: &[Scene] = &[
        // `ChMain`: дерево с учёткой канала, список бесед (закреплённые, счётчики), лента группы с полем ввода.
        Scene::new("chat-main", main, main_ready).wait(wait),
        // Личная беседа: вложения, «прочитано», ответ на сообщение над полем ввода.
        Scene::new("chat-direct", direct, direct_ready).continues().wait(wait),
        // Рассылка только для чтения: поля ввода нет («Нет у сервера — нет на экране»).
        Scene::new("chat-read-only", news, news_ready).continues(),
        // Кнопки бота и опрос в ленте группы; ответ бота — предупреждением и ссылкой над полем ввода.
        Scene::new("chat-bot", bot, bot_ready).continues().wait(wait),
        // Умная папка «Непрочитанное везде»: беседы чатов и почты в одном списке, значок канала у аватара.
        Scene::new("chat-smart", smart_folder, smart_ready).continues().wait(wait),
        // `ChPerson`: «Человек во всех каналах» — папка человека в дереве, его беседы почты и чатов.
        Scene::new("chat-person", person, person_ready).continues(),
        // `ChEmpty`: пустая умная папка — пустые состояния списка и области чтения.
        Scene::new("chat-empty", empty, empty_ready).continues(),
        // `ChSettings`: «Настройки → Каналы» — каналы, политика, умные папки справа.
        Scene::new("chat-settings", settings, settings_ready).continues(),
        // `ChLogin`: вход в Telegram на месте — шаги, «По номеру | По QR-коду», ошибка кода красной строкой.
        Scene::new("chat-login", login, login_ready).continues(),
    ];

    fn demo() -> ChanAccount {
        ChanAccount::new(postman_demo::channel::ID, postman_demo::channel::ACCOUNT)
    }

    fn address(conversation: &str) -> postman_core::channel::Address {
        let a = demo();
        postman_core::channel::Address::conversation(&a.channel, &a.account, &postman_core::channel::ConversationId::new(conversation))
    }

    /// Перед чужими сценами — назад к почте (сцены бесед — последние в прогоне).
    pub(crate) fn reset(s: &Scene, m: &mut Mailer, _: &mut Window, cx: &mut Context<Mailer>) {
        if !s.name.starts_with("chat-") && crate::prefs::get(cx).channels.telegram {
            crate::prefs::update(cx, |p| p.channels.telegram = false);
        }
        if !s.name.starts_with("chat-") && m.chat_active() {
            m.leave_chats(cx);
            if let Some(inbox) = crate::live::inbox(&m.folders.read(cx).mailboxes) {
                m.open_mailbox(inbox, cx);
            }
        }
    }

    /// Сцена `chat-main` уже попросила дерево прокрутиться к разделам каналов.
    static TREE_AT_END: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    fn main(_: &Scene, m: &mut Mailer, _: &mut Window, cx: &mut Context<Mailer>) -> Result<(), String> {
        TREE_AT_END.store(false, std::sync::atomic::Ordering::Relaxed);
        m.set_section(Section::Mail, cx);
        m.start_demo_channel(cx);
        m.open_chat_scope(Scope::All(demo()), cx);
        Ok(())
    }

    fn open(m: &mut Mailer, conversation: &str, cx: &mut Context<Mailer>) -> bool {
        let at = address(conversation);
        if m.chats.snapshot.rows.iter().all(|r| *r.at() != at) {
            return false;
        }
        if m.chats.pane.read(cx).address() != Some(&at) {
            m.open_chat(at, cx);
        }
        true
    }

    fn shown(m: &Mailer, conversation: &str, cx: &App) -> bool {
        let p = m.chats.pane.read(cx);
        p.address() == Some(&address(conversation)) && p.ready() && !m.chats.list.read(cx).is_empty()
    }

    fn main_ready(_: &Scene, m: &Mailer, cx: &App) -> bool {
        shown(m, postman_demo::channel::GROUP, cx) && TREE_AT_END.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn direct(_: &Scene, m: &mut Mailer, _: &mut Window, cx: &mut Context<Mailer>) -> Result<(), String> {
        if !open(m, postman_demo::channel::DIRECT, cx) {
            return Err("no direct conversation in the demo channel".into());
        }
        Ok(())
    }

    fn direct_ready(_: &Scene, m: &Mailer, cx: &App) -> bool {
        shown(m, postman_demo::channel::DIRECT, cx) && m.chats.pane.read(cx).replying()
    }

    fn news(_: &Scene, m: &mut Mailer, _: &mut Window, cx: &mut Context<Mailer>) -> Result<(), String> {
        if !open(m, postman_demo::channel::NEWS, cx) {
            return Err("no broadcast in the demo channel".into());
        }
        Ok(())
    }

    fn news_ready(_: &Scene, m: &Mailer, cx: &App) -> bool {
        shown(m, postman_demo::channel::NEWS, cx)
    }

    fn bot(_: &Scene, m: &mut Mailer, _: &mut Window, cx: &mut Context<Mailer>) -> Result<(), String> {
        m.open_chat_scope(Scope::All(demo()), cx);
        if !open(m, postman_demo::channel::GROUP, cx) {
            return Err("no group in the demo channel".into());
        }
        Ok(())
    }

    fn bot_ready(_: &Scene, m: &Mailer, cx: &App) -> bool {
        shown(m, postman_demo::channel::GROUP, cx) && m.chats.pane.read(cx).bot_reply_shown()
    }

    fn smart_folder(_: &Scene, m: &mut Mailer, _: &mut Window, cx: &mut Context<Mailer>) -> Result<(), String> {
        m.open_chat_scope(Scope::Smart(Smart::Unread), cx);
        Ok(())
    }

    /// В области чтения — беседа чата или письмо беседы почты.
    fn reading(m: &Mailer, cx: &App) -> bool {
        m.chats.pane.read(cx).ready() || (m.chats.mail_open && m.reading.read(cx).reading.is_some())
    }

    fn smart_ready(_: &Scene, m: &Mailer, cx: &App) -> bool {
        m.chats.scope == Some(Scope::Smart(Smart::Unread)) && !m.chats.list.read(cx).is_empty() && reading(m, cx)
    }

    fn person(_: &Scene, m: &mut Mailer, _: &mut Window, cx: &mut Context<Mailer>) -> Result<(), String> {
        // Имя — как у личной беседы демо-канала; адрес — её почта в демо-ящике.
        let direct = address(postman_demo::channel::DIRECT);
        let name = m.chats.snapshot.rows.iter().find(|r| *r.at() == direct).map(|r| r.item.summary.conversation.title.clone());
        let Some(name) = name else { return Err("no direct conversation in the demo channel".into()) };
        m.chat_person(Person { name, addresses: vec!["anna@example.test".into()] }, cx);
        m.folders.update(cx, |f, cx| f.screen_scroll_top(cx));
        Ok(())
    }

    fn person_ready(_: &Scene, m: &Mailer, cx: &App) -> bool {
        let Some(Scope::Smart(Smart::Person(p))) = &m.chats.scope else { return false };
        m.section == Section::Mail && m.chats.snapshot.people().contains(p) && !m.chats.list.read(cx).is_empty()
    }

    fn empty(_: &Scene, m: &mut Mailer, _: &mut Window, cx: &mut Context<Mailer>) -> Result<(), String> {
        m.open_chat_scope(Scope::Smart(Smart::Pinned), cx);
        Ok(())
    }

    fn empty_ready(_: &Scene, m: &Mailer, cx: &App) -> bool {
        m.chats.scope == Some(Scope::Smart(Smart::Pinned)) && m.chats.pane.read(cx).address().is_none()
    }

    fn settings(_: &Scene, m: &mut Mailer, window: &mut Window, cx: &mut Context<Mailer>) -> Result<(), String> {
        m.open_settings(Some(crate::settings_tab::Section::Channels), window, cx);
        Ok(())
    }

    fn settings_ready(_: &Scene, m: &Mailer, cx: &App) -> bool {
        crate::screens::settings_on(m, crate::settings_tab::Section::Channels, cx)
    }

    fn login(_: &Scene, m: &mut Mailer, window: &mut Window, cx: &mut Context<Mailer>) -> Result<(), String> {
        crate::prefs::update(cx, |p| p.channels.telegram = true);
        m.open_settings(Some(crate::settings_tab::Section::Channels), window, cx);
        let Some(view) = m.settings.view.clone() else { return Err("settings are not open".into()) };
        let panel = view.read(cx).channels.clone();
        panel.update(cx, |p, cx| p.screen_code_step(window, cx));
        Ok(())
    }

    fn login_ready(_: &Scene, m: &Mailer, cx: &App) -> bool {
        let card = m.settings.view.as_ref().is_some_and(|v| v.read(cx).channels.read(cx).signing_in());
        card && crate::screens::settings_on(m, crate::settings_tab::Section::Channels, cx)
    }

    /// Сколько раз сцена ждала: раз в сотню шагов — строка в журнал, что ещё не готово (диагностика прогона снимков).
    static WAITED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    fn trace(s: &Scene, m: &Mailer, cx: &App) {
        let n = WAITED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if n % 100 == 99 {
            let pane = m.chats.pane.read(cx);
            tracing::info!(
                scene = s.name,
                accounts = m.chats.snapshot.accounts.len(),
                rows = m.chats.snapshot.rows.len(),
                listed = m.chats.list.read(cx).len(),
                open = pane.address().is_some(),
                loaded = pane.ready(),
                "screens: chat scene is waiting"
            );
        }
    }

    /// Шаг ожидания сцен: беседа открывается, когда снимок прочитан; в личной — «Ответить» на первое сообщение.
    pub(crate) fn wait(s: &Scene, m: &mut Mailer, cx: &mut Context<Mailer>) {
        trace(s, m, cx);
        match s.name {
            "chat-main" => {
                // Разделы каналов — под почтовыми папками: дерево — к нижнему краю.
                // Один раз: дерево, которое помещается целиком, прокручивать некуда.
                if open(m, postman_demo::channel::GROUP, cx) && !TREE_AT_END.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    m.folders.update(cx, |f, cx| f.screen_scroll_chats(cx));
                }
            }
            "chat-bot" => {
                let ready = shown(m, postman_demo::channel::GROUP, cx);
                if ready && !m.chats.pane.read(cx).bot_reply_shown() {
                    m.chats.pane.update(cx, |p, cx| p.screen_bot_reply(cx));
                }
            }
            // Первая беседа умной папки — в область чтения.
            "chat-smart" => {
                let first = m.chats.list.read(cx).first();
                if let Some(at) = first.filter(|_| !reading(m, cx) && m.chats.list.read(cx).selected.is_none()) {
                    m.open_chat(at, cx);
                }
            }
            "chat-direct" => {
                let ready = shown(m, postman_demo::channel::DIRECT, cx);
                if ready && !m.chats.pane.read(cx).replying() {
                    m.chats.pane.update(cx, |p, cx| p.screen_reply_first(cx));
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use postman_cache::Cache;
    use postman_core::channel::{
        AccountId, Body, ChannelEvent, ChannelFeatures, ChannelFolder, ChannelId, ChannelStore, Conversation, ConversationId, ConversationKind,
        FolderRef, Limit, Message, MessageId, Participant, ParticipantId, Reactions, Role, outbox,
    };

    use super::{Scope, pending_client, shown, snapshot};

    fn message(id: &str, author: &str, sent: i64, text: &str) -> Message {
        Message { id: MessageId::new(id), author: ParticipantId::new(author), sent, body: Body::Plain(text.into()), ..Message::default() }
    }

    /// Снимок: учётка с папкой, беседы по папке, закреплённая — первой, автор последнего сообщения — по имени;
    /// выключенный канал — без учётки и бесед.
    #[test]
    fn snapshot_reads_accounts_folders_and_rows() {
        let cache = Cache::open_in_memory().unwrap();
        let (channel, account) = (ChannelId::new("demo"), AccountId::new("me@example.test"));
        let store = ChannelStore::open(cache.clone(), &channel, &account, "Я").unwrap();
        let conv = |id: &str, title: &str, pinned| Conversation {
            id: ConversationId::new(id),
            kind: ConversationKind::Group,
            title: title.into(),
            pinned,
            ..Conversation::default()
        };
        store
            .apply(vec![
                ChannelEvent::Conversations(vec![conv("g", "Команда", false), conv("p", "Важное", true)]),
                ChannelEvent::People(vec![Participant { id: ParticipantId::new("u-anna"), name: "Анна".into(), ..Participant::default() }]),
                ChannelEvent::Messages {
                    conversation: ConversationId::new("g"), messages: vec![message("1", "u-anna", 20, "привет")], live: false
                },
                ChannelEvent::Messages {
                    conversation: ConversationId::new("p"), messages: vec![message("2", "u-anna", 10, "старое")], live: false
                },
                ChannelEvent::Folders(vec![ChannelFolder {
                    id: FolderRef::new("f"),
                    name: "Работа".into(),
                    parent: None,
                    conversations: vec![ConversationId::new("g")],
                }]),
            ])
            .unwrap();
        let s = snapshot(&cache, 100, &|_| true, None).unwrap();
        let chan = store.chan();
        assert_eq!(s.accounts.len(), 1);
        assert_eq!(s.accounts[0].folders[0].name, "Работа");
        let all = s.rows_of(&Scope::All(chan.clone()));
        assert_eq!(all.iter().map(|r| r.item.summary.conversation.title.as_str()).collect::<Vec<_>>(), ["Важное", "Команда"], "закреплённая сверху");
        assert_eq!(all[1].author, "Анна");
        let folder = s.rows_of(&Scope::Folder(chan.clone(), FolderRef::new("f")));
        assert_eq!(folder.len(), 1);
        assert_eq!(folder[0].at().conversation.0, "g");
        assert!(s.rows_of(&Scope::All(postman_core::channel::ChanAccount::new("telegram", "1"))).is_empty());
        let off = snapshot(&cache, 100, &|_| false, None).unwrap();
        assert!(off.accounts.is_empty() && off.rows.is_empty(), "канал выключен — его бесед на экране нет");
    }

    /// Умные папки: без каналов их нет; выключенной в настройках нет в снимке; папка человека — по его адресам;
    /// настройки и люди переживают перечитывание базы.
    #[test]
    fn smart_folders_follow_settings() {
        use postman_core::channel::{Member, Role};

        use super::{Person, Smart, set_smart_people, set_smart_shown, smart_people, smart_shown};
        let cache = Cache::open_in_memory().unwrap();
        assert_eq!(smart_shown(&cache).unwrap(), [true; 3], "записи нет — показаны все");
        assert!(snapshot(&cache, 100, &|_| true, None).unwrap().smart.is_empty(), "каналов нет — умных папок нет");
        let (channel, account) = (ChannelId::new("demo"), AccountId::new("me@example.test"));
        let store = ChannelStore::open(cache.clone(), &channel, &account, "Я").unwrap();
        let conv = |id: &str, pinned| Conversation {
            id: ConversationId::new(id),
            kind: ConversationKind::Group,
            title: id.into(),
            pinned,
            ..Conversation::default()
        };
        let anna =
            Participant {
                id: ParticipantId::new("u-anna"), name: "Анна".into(), emails: vec!["anna@example.test".into()], ..Participant::default()
            };
        store
            .apply(vec![
                ChannelEvent::Conversations(vec![conv("g", false), conv("p", true)]),
                ChannelEvent::People(vec![anna]),
                ChannelEvent::Members {
                    conversation: ConversationId::new("g"),
                    changed: vec![Member { id: ParticipantId::new("u-anna"), role: Role::Member }],
                    left: vec![],
                },
                ChannelEvent::Messages {
                    conversation: ConversationId::new("g"), messages: vec![message("1", "u-anna", 20, "привет")], live: false
                },
            ])
            .unwrap();
        let person = Person { name: "Анна".into(), addresses: vec!["anna@example.test".into()] };
        set_smart_shown(&cache, [true, false, true]).unwrap();
        set_smart_people(&cache, std::slice::from_ref(&person)).unwrap();
        assert_eq!(smart_shown(&cache).unwrap(), [true, false, true]);
        assert_eq!(smart_people(&cache).unwrap(), std::slice::from_ref(&person));
        let s = snapshot(&cache, 100, &|_| true, None).unwrap();
        let kinds: Vec<Smart> = s.smart.iter().map(|(k, _)| k.clone()).collect();
        assert_eq!(kinds, [Smart::Unread, Smart::Pinned, Smart::Person(person.clone())], "«Упоминания меня» выключены");
        let ids = |smart: Smart| s.rows_of(&Scope::Smart(smart)).iter().map(|r| r.at().conversation.0.clone()).collect::<Vec<_>>();
        assert_eq!(ids(Smart::Unread), ["g"]);
        assert_eq!(ids(Smart::Pinned), ["p"]);
        assert_eq!(ids(Smart::Person(person.clone())), ["g"]);
        assert!(ids(Smart::Mentions).is_empty());
        assert_eq!(s.counts(&Scope::Smart(Smart::Unread)).unread, 1);
        assert_eq!(s.people(), [person]);
    }

    /// «Нет у сервера — нет на экране»: правка, удаление у всех, реакции, ответ, поле ввода — по возможностям, сроку
    /// и роли; у сообщения в очереди отправки — ничего.
    #[test]
    fn actions_follow_channel_features() {
        let mine = Message { mine: true, ..message("1", "me", 1_000, "a") };
        let theirs = message("2", "u", 1_000, "b");
        let none = ChannelFeatures::default();
        let s = shown(&none, Role::Member, Some(&mine), 2_000);
        assert!(!s.reply && !s.edit && !s.delete_for_all && !s.react && !s.receipts && !s.typing && s.write);
        let all = ChannelFeatures {
            edit: Limit::Within(60),
            delete_for_all: Limit::Always,
            reactions: Reactions::Any,
            read_receipts: true,
            typing: true,
            reply_to: true,
            ..ChannelFeatures::default()
        };
        let s = shown(&all, Role::Member, Some(&mine), 2_000);
        assert!(s.reply && s.edit && s.delete_for_all && s.react && s.receipts && s.typing);
        assert!(!shown(&all, Role::Member, Some(&mine), 1_000 + 61_000).edit, "срок правки прошёл — пункта нет");
        let s = shown(&all, Role::Member, Some(&theirs), 2_000);
        assert!(s.reply && s.react && !s.edit && !s.delete_for_all, "чужое не править и не удалять у всех");
        let s = shown(&all, Role::ReadOnly, Some(&theirs), 2_000);
        assert!(!s.write && !s.reply, "только чтение — без поля ввода и ответа");
        let pending = Message { id: outbox::pending_id(7), mine: true, ..message("x", "me", 1_000, "c") };
        let s = shown(&all, Role::Member, Some(&pending), 2_000);
        assert!(!s.reply && !s.edit && !s.react && !s.delete_for_all, "ещё не отправлено");
        assert_eq!(pending_client(&pending.id), Some(7));
        assert_eq!(pending_client(&MessageId::new("12")), None);
    }
}
