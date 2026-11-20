//! Сообщения хоста и плагина по обмену (ADR 0013 «Договор»): одно перечисление на сторону, кодек — postcard, кадры —
//! `postman_proc::link`. Двоичная часть кадра несёт байты без перекодирования: файл выгрузки ([`Call::Upload`]) и
//! загрузки (ответ на [`Call::Download`]).
//!
//! - Хост → плагин ([`ToPlugin`]): вопрос с ответом по номеру ([`Ask`]: `Hello`, `Open`, вызовы сессии и шаги входа),
//!   смена настроек, `Close`, `Leave`, ответ на услугу ядра ([`ToPlugin::Stored`]). `Ping` и `Shutdown` — кадры самой
//!   связи.
//! - Плагин → хост ([`FromPlugin`]): ответ на вопрос ([`Reply`]), событие учётки ([`FromPlugin::Event`]), услуги ядра —
//!   вызовом связи ([`FromPlugin::Store`]: `PluginStore` учётки или входа; [`FromPlugin::Sso`]: возврат SSO из браузера
//!   слушает ядро, песочница плагина входящих не принимает).
//!
//! Номер сессии и номер входа выдаёт хост: им же помечены события и вызовы хранилища, хост по ним находит учётку.

use postman_cache::{AccountRow, BodyRow, EmailRow, FolderId, FolderRow, LetterId, LetterRow, MailboxRow, RemoteRef};
use postman_calendar::{Invitation, Partstat};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::channel::{
    AccountId, Action, Address, Answer, AttachmentSource, ChannelError, ChannelEvent, ChannelFeatures, ConversationId, Cursor, Done, FileMeta,
    FlowKind, MessageId, Page, Participant, Prompt, Secret, Uploaded,
};
use crate::invite::{Answer as InviteAnswer, Responded};
use crate::model::{Error as ModelError, Features};
use crate::smime::directory::DirectoryError;
use crate::sync::{Command as SyncCommand, Event as SyncEvent, Sender};
use crate::tasks::Delta;

/// Версия договора обмена: меняется с любым несовместимым изменением сообщений. №2 — `PluginHello` сообщает наличие
/// ключа приложения (PLG-05: ключ Telegram остался в плагине, интерфейс его больше не читает из сборки); №3 — запрос
/// каталога (`Ask::Certificates`, PLG-06: LDAP — процессом набора); №4 — области учётки и публичные загрузки
/// (`Ask::Area*`, `Ask::Fetch`, PLG-07: CalDAV и CardDAV — процессом, задачи поверх кэша — услугой ядра); №5 —
/// почтовый путь и услуги ядра ([`Ask::MailOpen`], [`FromPlugin::MailEvent`], [`FromPlugin::Cache`], [`FromPlugin::Worker`],
/// [`FromPlugin::Smime`], PLG-08: EWS — процессом, хранилище учётки, разбор письма и S/MIME — услугами).
pub const WIRE: u32 = 5;

/// `serde_json::Value` и `Map` в сообщениях обмена (задачи областей, PLG-07): кодек postcard — формат без
/// самоописания, `Value` он не разбирает (`deserialize_any` не поддержан), поэтому значение едет байтами JSON — тем
/// же значением. Как `crate::json` у календаря (PLG-07 serde).
pub(crate) mod json {
    pub mod value {
        use serde::{Deserialize, Deserializer, Serialize, Serializer};
        use serde_json::Value;

        pub fn serialize<S: Serializer>(v: &Value, s: S) -> Result<S::Ok, S::Error> {
            serde_json::to_vec(v).map_err(serde::ser::Error::custom)?.serialize(s)
        }

        pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Value, D::Error> {
            serde_json::from_slice(&Vec::<u8>::deserialize(d)?).map_err(serde::de::Error::custom)
        }
    }

    pub mod vec {
        use serde::{Deserialize, Deserializer, Serialize, Serializer};
        use serde_json::Value;

        pub fn serialize<S: Serializer>(v: &[Value], s: S) -> Result<S::Ok, S::Error> {
            serde_json::to_vec(v).map_err(serde::ser::Error::custom)?.serialize(s)
        }

        pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Value>, D::Error> {
            serde_json::from_slice(&Vec::<u8>::deserialize(d)?).map_err(serde::de::Error::custom)
        }
    }

    pub mod map {
        use serde::{Deserialize, Deserializer, Serialize, Serializer};
        use serde_json::{Map, Value};

        pub fn serialize<S: Serializer>(v: &Map<String, Value>, s: S) -> Result<S::Ok, S::Error> {
            serde_json::to_vec(v).map_err(serde::ser::Error::custom)?.serialize(s)
        }

        pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Map<String, Value>, D::Error> {
            serde_json::from_slice(&Vec::<u8>::deserialize(d)?).map_err(serde::de::Error::custom)
        }
    }
}

/// Чья сборка: версия договора, версия программы и номер сборки. Хост и плагин из разных сборок (обновление прервано)
/// не работают вместе.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Build {
    pub wire: u32,
    pub version: String,
    /// Коммит сборки CI (`GITHUB_SHA`); локальная сборка — `dev`.
    pub build: String,
}

impl Build {
    pub fn current() -> Self {
        Self { wire: WIRE, version: env!("CARGO_PKG_VERSION").into(), build: option_env!("GITHUB_SHA").unwrap_or("dev").into() }
    }
}

/// Вид плагина: что он реализует в договоре ядра.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    /// Канал бесед (`ChannelPlugin`).
    Channel,
    /// Почтовый путь (`MailPlugin`).
    Mail,
    /// Путь областей (`AreaPlugin`).
    Area,
    /// Каталог (LDAP).
    Directory,
}

/// Настройки, которые плагин получает от ядра: в `Hello` и сообщением при смене ([`ToPlugin::Settings`]). Окружения
/// у процесса нет — всё, что ему нужно знать о машине и политике, приходит отсюда.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    /// Уровень журнала (`POSTMAN_LOG`, «Настройки → Общие → Уровень журнала»): `warn`, `info`, `debug`…
    pub log: String,
    /// Только TLS 1.3 для новых подключений (политика и «Настройки → Безопасность», S5-30).
    pub tls13_only: bool,
    /// Прокси сети (`http://узел:порт`); `None` — напрямую.
    pub proxy: Option<String>,
}

/// Приветствие хоста.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostHello {
    pub build: Build,
    pub settings: Settings,
}

/// Приветствие плагина: чья сборка, кто он и есть ли у него ключ приложения (Telegram; остальные плагины — `false`,
/// интерфейс читает только у своего канала).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginHello {
    pub build: Build,
    pub id: String,
    pub kind: Kind,
    /// Ключ приложения (`api_id`/`api_hash`) в сборке плагина есть (PLG-05, ADR 0013 «Договор»): сам ключ — у
    /// плагина, интерфейсу — только факт его наличия.
    pub key: bool,
}

/// Подключённая сессия канала: кто я и что умеет учётка (`ChannelSession::me`, `features`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Opened {
    pub me: Participant,
    pub features: ChannelFeatures,
}

/// Хост → плагин.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ToPlugin {
    /// Вопрос: ответ — [`FromPlugin::Answer`] с тем же номером.
    Ask { id: u64, ask: Ask },
    /// Настройки сменились (уровень журнала, политика, прокси).
    Settings(Settings),
    /// Сессия больше не нужна.
    Close { session: u64 },
    /// Вход или поток вопросов больше не нужен.
    CloseFlow { flow: u64 },
    /// Программа закрывается: дослать исходящее, что держит плагин. Каналы исходящего не держат (очередь — у ядра).
    Leave,
    /// Ответ ядра на вызов хранилища ([`FromPlugin::Store`]).
    Stored(Result<Option<Vec<u8>>, ChannelError>),
    /// Ответ ядра на услугу SSO ([`FromPlugin::Sso`]).
    SsoReply(Result<SsoDone, ChannelError>),
    /// Ответ хранилища учётки ([`FromPlugin::Cache`], PLG-08): чтение или приём записи, вызванной с ответом.
    /// `Err` — база не прочиталась/не записалась и очередь записей кончилась ошибкой.
    Cached(Result<CacheDone, String>),
    /// Ответ воркера ([`FromPlugin::Worker`], PLG-08).
    Worked(Result<WorkerDone, String>),
    /// Ответ S/MIME ([`FromPlugin::Smime`], PLG-08): письмо собрано или отказ.
    Sealed(Result<Vec<u8>, String>),
}

/// Вопрос хоста.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Ask {
    /// Первое сообщение: сверка сборки, настройки. Ответ — [`Reply::Hello`].
    Hello(HostHello),
    /// Память процесса для `--bench` (`plugins_rss_mb`). Ответ — [`Reply::Stats`].
    Stats,
    /// `ChannelPlugin::features`. Ответ — [`Reply::Features`].
    Features,
    /// `ChannelPlugin::parse_link`. Ответ — [`Reply::Address`].
    ParseLink(String),
    /// `ChannelPlugin::link`. Ответ — [`Reply::Url`].
    Link(Address),
    /// `ChannelPlugin::person_url`. Ответ — [`Reply::Url`].
    PersonUrl(Participant),
    /// `ChannelPlugin::connect` с секретом учётки; хранилище — услугой ядра под номером `session`. Ответ —
    /// [`Reply::Opened`].
    Open { session: u64, account: AccountId, secret: Secret },
    /// `ChannelPlugin::sign_in`; хранилище — услугой под номером `flow`. Ответ — [`Reply::Unit`].
    SignIn { flow: u64 },
    /// Шаг входа или потока вопросов (`Flow::step`). Ответ — [`Reply::Prompt`].
    Step { flow: u64, answer: Option<Answer> },
    /// Вызов подключённой сессии.
    Session { session: u64, call: Call },
    /// Поиск сертификатов в каталоге (`Directory::certificates`, PLG-06). Ответ — [`Reply::Certs`].
    Certificates(DirectoryQuery),
    /// Войти в область учётной записи (`AreaPlugin::connect`, PLG-07); сессия — как у каналов. Ответ —
    /// [`Reply::AreaOpened`] (что нашлось на сервере). Возможности объединяет ядро (`Event::Features` области не
    /// шлют); `profile` — учётная запись профиля (`None` — основная) — один раз: события команды
    /// ([`FromPlugin::AreaEvent`]) плагин метит сам по ней и по адресу участника из `login`.
    AreaOpen { session: u64, url: String, profile: Option<String>, login: AreaLogin },
    /// Команда области (`Areas::handle`, PLG-07): сначала область («не моя — назад»), затем почтовый путь — так
    /// разводит ядро. События во время команды — [`FromPlugin::AreaEvent`]. Задачи (`Command::Tasks`) ядро не
    /// посылает: кэш у него, хранилище — [`Ask::AreaTask`]. Ответ — [`Reply::AreaCommand`].
    AreaCommand { session: u64, cmd: Box<SyncCommand> },
    /// Ответ на приглашение своей копией (`Areas::respond`, PLG-07). Ответ — [`Reply::AreaResponded`].
    AreaRespond { session: u64, invite: Box<Invitation>, status: Partstat, answer: InviteAnswer },
    /// Изменилось ли что-то без push (`Areas::changed`, PLG-07): опрос — таймер ядра, раз в 2 минуты. Ответ —
    /// [`Reply::AreaChanged`].
    AreaChanged { session: u64 },
    /// Операция хранилища задач плагина (услуга ядра, PLG-07). Ответ — [`Reply::AreaTask`].
    AreaTask { session: u64, op: TaskOp },
    /// Есть ли путь областей на узле (`AreaPlugin::probe`, PLG-07): вид сервера по адресу узнают и без учётки.
    /// Ответ — [`Reply::Url`].
    Probe(String),
    /// Скачать публичный ресурс без входа (картинка письма, подписка на календарь, занятость по ссылке, S8-42/S8-44;
    /// PLG-07): `etag` — условный запрос (`If-None-Match`). Ответ — [`Reply::Fetched`], тело — двоичной частью.
    Fetch { url: String, max: u32, etag: Option<String> },
    /// Запустить синхронизацию учётной записи (`MailPlugin::spawn`, PLG-08): путь-процесс ведёт свой поток, события —
    /// [`FromPlugin::MailEvent`], команды — [`Ask::MailCommand`]. База открыта только у ядра: хранилище учётки —
    /// [`FromPlugin::Cache`]. Ответ — [`Reply::MailOpened`].
    MailOpen { session: u64, url: String, profile: Option<String>, login: MailLogin },
    /// Команда учётки (`sync::Command`) — в поток её синхронизации (PLG-08). Ответ — [`Reply::Unit`]: `Err` —
    /// синхронизация кончилась (отказ входа), команд больше не принять.
    MailCommand { session: u64, cmd: Box<SyncCommand> },
}

/// Как входить в каталог ([`DirectoryQuery`], PLG-06): то же, что «Настройки → Безопасность», но пароль простого
/// входа — байтами ([`Secret`]: по обмену, `Debug` без значения, нулится при выходе).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DirectoryLogin {
    /// Учётная запись компьютера в домене (Kerberos: SSPI на Windows, `ldap3` GSSAPI на Linux, GSS.framework на macOS).
    Windows,
    /// Имя (DN или `user@domain`) и пароль — только по TLS: `ldaps://` или StartTLS.
    Password { user: String, password: Secret },
    /// Без входа: каталог открыт на чтение.
    Anonymous,
}

/// Запрос каталога ([`Ask::Certificates`], PLG-06): настройки «Настройки → Безопасность → Каталог» и адреса. Сервер
/// по умолчанию — контроллер домена — резолвит хост (функция ОС в интерфейсе, `app-gpui/src/sys`, ADR 0013):
/// песочнице плагина файлы и билет Kerberos не нужны.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectoryQuery {
    /// Сервер: `host`, `host:port`, `ldap://…` или `ldaps://…`; `None` — не нашёлся (компьютер не в домене).
    pub server: Option<String>,
    /// База поиска; `None` — из rootDSE: `defaultNamingContext`, иначе первый раздел данных `namingContexts`.
    pub base_dn: Option<String>,
    /// StartTLS по `ldap://`.
    pub starttls: bool,
    pub login: DirectoryLogin,
    /// Адреса для поиска сертификатов.
    pub addresses: Vec<String>,
}

/// Вход в область учётной записи ([`Ask::AreaOpen`], PLG-07): логин и пароль учётной записи (пароль — байтами
/// [`Secret`], как у каталога) и её адрес (участник встреч, если сервер не дал свой).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AreaLogin {
    pub user: String,
    pub password: Secret,
    pub email: String,
}

/// Вход почтового пути ([`Ask::MailOpen`], PLG-08): логин и пароль учётной записи; адрес ящика путь узнаёт у сервера.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailLogin {
    pub user: String,
    pub password: Secret,
}

/// Операция хранилища учётки ([`FromPlugin::Cache`], PLG-08): база открыта только у ядра, путь-процесс спрашивает
/// операциями. Записи ([`FromPlugin::CacheWrite`]) идут подряд без ответа, ошибка — на ближайшем вызове с ответом,
/// порядок сохраняется; чтения — вызовом с ответом ([`CacheDone`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CacheOp {
    /// Папки учётки — заново ([`postman_cache::Cache::replace_mailboxes`]).
    ReplaceMailboxes(Option<String>, Vec<MailboxRow>),
    /// Учётки профиля ([`postman_cache::Cache::replace_accounts_of`]).
    ReplaceAccountsOf(Option<String>, Vec<AccountRow>),
    /// Строки писем ([`postman_cache::Cache::upsert_emails`]).
    UpsertEmails(Vec<EmailRow>),
    /// Удалить письма ([`postman_cache::Cache::delete_emails`]).
    DeleteEmails(Vec<String>),
    /// Состояние синхронизации ([`postman_cache::Cache::set_state`]).
    SetState(String, String),
    /// Разобранное тело ([`postman_cache::Cache::put_body`]).
    PutBody(String, BodyRow),
    /// Байты вложения ([`postman_cache::Cache::put_blob`]).
    PutBlob(String, Vec<u8>),
    /// Письмо целиком — содержимым ([`postman_cache::Cache::put_content`]), части — от воркера.
    PutContent(LetterId, Vec<u8>, Vec<postman_cache::SplitPart>),
    /// Связь письма с сервером ([`postman_cache::Cache::attach_remote`]).
    AttachRemote(LetterId, RemoteRef),
    /// Связи после своего переноса ([`postman_cache::Cache::upsert_remotes`]).
    UpsertRemotes(String, Vec<(String, LetterRow)>),
    /// Папка ([`postman_cache::Cache::upsert_folder`]) — ответ [`CacheDone::FolderUpserted`] (RENAME у IMAP).
    UpsertFolder(FolderRow),
    /// Адреса «От» — в адреса бесед писем ([`crate::channel::mail::remember_senders`], ADR 0012).
    RememberSenders(Vec<Sender>),
    /// Состояние синхронизации ([`postman_cache::Cache::state`]) — ответ [`CacheDone::State`].
    State(String),
    /// Тело письма ([`postman_cache::Cache::body`]) — ответ [`CacheDone::Body`].
    Body(String),
    /// Строки писем по ключам ([`postman_cache::Cache::rows_by_ids`]) — ответ [`CacheDone::Rows`].
    RowsByIds(Vec<String>),
    /// Связи письма ([`postman_cache::Cache::remotes`]) — ответ [`CacheDone::Remotes`].
    Remotes(LetterId),
    /// Письмо целиком из хранилища ([`postman_cache::Cache::letter_content`]) — ответ [`CacheDone::Content`].
    LetterContent(LetterId),
    /// Сколько писем в папке ([`postman_cache::Cache::count`]) — ответ [`CacheDone::Count`].
    Count(String),
    /// Дерево папок ([`postman_cache::Cache::mailboxes`]) — ответ [`CacheDone::Mailboxes`].
    Mailboxes,
    /// Письмо по связи с сервером ([`postman_cache::Cache::letter_of`]) — ответ [`CacheDone::LetterOf`].
    LetterOf(String, String),
    /// Байты вложения ([`postman_cache::Cache::blob`]) — ответ [`CacheDone::Blob`].
    Blob(String),
    /// Папка ([`postman_cache::Cache::folder`]) — ответ [`CacheDone::Folder`].
    Folder(FolderId),
    /// Страница папки ([`postman_cache::Cache::page`]) — ответ [`CacheDone::Rows`].
    Page(String, usize, usize),
    /// Папка по связи с сервером ([`postman_cache::Cache::folder_of`]) — ответ [`CacheDone::FolderOf`].
    FolderOf(String, String),
    /// Строка письма ([`postman_cache::Cache::letter`]) — ответ [`CacheDone::Letter`].
    Letter(LetterId),
    /// Цепочка ([`postman_cache::Cache::thread`]) — ответ [`CacheDone::Rows`].
    Thread(String),
}

/// Ответ хранилища учётки на вызов с ответом ([`ToPlugin::Cached`], PLG-08).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CacheDone {
    /// Запись принята (вызовом, а не [`FromPlugin::CacheWrite`]).
    Done,
    State(Option<String>),
    Body(Option<BodyRow>),
    Rows(Vec<EmailRow>),
    Remotes(Vec<RemoteRef>),
    Content(Option<Vec<u8>>),
    Count(usize),
    Mailboxes(Vec<MailboxRow>),
    LetterOf(Option<LetterId>),
    Blob(Option<Vec<u8>>),
    Folder(Option<FolderRow>),
    /// Ключ папки после записи ([`postman_cache::Cache::upsert_folder`], RENAME у IMAP).
    FolderUpserted(FolderId),
    FolderOf(Option<FolderId>),
    Letter(Option<LetterRow>),
}

/// Разбор письма воркером-песочницей ([`FromPlugin::Worker`], PLG-08): чужие данные — только в воркере, и воркер
/// запускает ядро, не плагин. Ответ — [`ToPlugin::Worked`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkerOp {
    /// Разобранное письмо (`postman_worker::Client::mime`) — ответ [`WorkerDone::Mime`].
    Mime(Vec<u8>),
    /// Часть письма по номеру (`mime_part`) — ответ [`WorkerDone::Bytes`].
    Part { raw: Vec<u8>, index: u32 },
    /// Раскладка частей (`mime_split`) — ответ [`WorkerDone::Split`].
    Split(Vec<u8>),
    /// Цепочки по заголовкам писем (`threads`) — ответ [`WorkerDone::Threads`].
    Threads(Vec<Vec<u8>>),
}

/// Ответ воркера ([`ToPlugin::Worked`], PLG-08).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkerDone {
    /// `postman_worker::mime::Mime` — байтами postcard (тот же кодек, что у всего обмена), тем же значением.
    Mime(#[serde(with = "worker_bytes")] postman_worker::mime::Mime),
    Bytes(Vec<u8>),
    Split {
        #[serde(with = "worker_bytes")]
        split: postman_worker::mime::Split,
        decoded: Vec<u8>,
    },
    Threads(#[serde(with = "worker_bytes")] Vec<postman_worker::mime::Thread>),
}

/// Типы воркера — байтами postcard: тот же кодек, что у всего обмена (PLG-08).
mod worker_bytes {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(v: &impl Serialize, s: S) -> Result<S::Ok, S::Error> {
        let bytes = postcard::to_stdvec(v).map_err(serde::ser::Error::custom)?;
        s.serialize_bytes(&bytes)
    }

    pub fn deserialize<'de, D: Deserializer<'de>, T: serde::de::DeserializeOwned>(d: D) -> Result<T, D::Error> {
        let bytes: Vec<u8> = Deserialize::deserialize(d)?;
        postcard::from_bytes(&bytes).map_err(serde::de::Error::custom)
    }
}

/// S/MIME исходящего ([`FromPlugin::Smime`], PLG-08): шифрование и подпись — у ядра, ключи пользователя плагину
/// недоступны. Ответ — [`ToPlugin::Sealed`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SmimeOp {
    /// [`crate::smime::seal_outgoing`]: письмо «Конфиденциально» адресатам `to`.
    Seal { raw: Vec<u8>, to: Vec<postman_smime::seal::To> },
    /// [`crate::smime::sign_outgoing`]: подпись письма от `from`; `hwnd` — окно выбора сертификата у ядра.
    Sign { raw: Vec<u8>, from: String, hwnd: Option<isize>, now: i64 },
}

/// Операция хранилища задач плагина областей ([`Ask::AreaTask`], PLG-07): задачи поверх кэша — услуга ядра
/// ([`tasks::handle`](crate::tasks::handle) у хоста), само хранилище (VTODO календарей CalDAV) — у плагина.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TaskOp {
    Load,
    Changes(String),
    Get(Vec<String>),
    Create(#[serde(with = "json::value")] Value),
    Update {
        id: String,
        #[serde(with = "json::map")]
        patch: Map<String, Value>,
    },
    Destroy(String),
    DefaultCalendar,
}

/// Ответ на операцию хранилища задач.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TaskDone {
    Loaded {
        #[serde(with = "json::vec")]
        tasks: Vec<Value>,
        state: String,
    },
    Delta(Delta),
    Got(#[serde(with = "json::vec")] Vec<Value>),
    Created(String),
    /// Правка, удаление.
    Done,
    /// Календарь для новых задач.
    Calendar(String),
}

/// Итог публичной загрузки ([`Ask::Fetch`], PLG-07): тело — двоичной частью кадра.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Fetched {
    /// Ресурс не менялся с прошлого раза (ETag, S8-44: подписки).
    NotModified,
    Body {
        etag: Option<String>,
    },
}

/// Вызов `ChannelSession`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Call {
    /// Ответ — [`Reply::Unit`]; события — [`FromPlugin::Event`] до ответа.
    CatchUp(Option<Cursor>),
    /// Ответ — [`Reply::Page`].
    History { conversation: ConversationId, before: Option<MessageId>, limit: u32 },
    /// Ответ — [`Reply::Page`].
    Search { query: String, conversation: Option<ConversationId>, limit: u32 },
    /// Ответ — [`Reply::Done`].
    Act(Action),
    /// Ответ — [`Reply::Unit`], байты — двоичной частью.
    Download(AttachmentSource),
    /// Байты файла — двоичной частью. Ответ — [`Reply::Uploaded`].
    Upload(FileMeta),
    /// Поток вопросов сессии под номером `flow`. Ответ — [`Reply::Unit`], шаги — [`Ask::Step`].
    Flow { flow: u64, kind: FlowKind },
    /// Ответ — [`Reply::Unit`].
    SignOut,
}

/// Плагин → хост.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum FromPlugin {
    /// Ответ на [`ToPlugin::Ask`].
    Answer { id: u64, reply: Reply },
    /// Событие сессии `session` (`ConnectCtx::incoming`).
    Event { session: u64, event: ChannelEvent },
    /// Услуга ядра — вызовом связи: хранилище сессии или входа `scope`. Ответ — [`ToPlugin::Stored`].
    Store { scope: u64, op: StoreOp },
    /// Услуга SSO (ADR 0013 «Договор»): возврат из браузера на `127.0.0.1` слушает ядро, песочница плагина входящих
    /// не принимает. Ответ — [`ToPlugin::SsoReply`].
    Sso { flow: u64, op: SsoOp },
    /// Событие области учётки во время команды ([`Ask::AreaCommand`], PLG-07): тот же договор `sync::Event`, что у
    /// почтового пути; заместитель отдаёт их потоку учётки по номеру сессии.
    AreaEvent { session: u64, event: SyncEvent },
    /// Событие потока синхронизации учётки ([`Ask::MailOpen`], PLG-08): тот же договор `sync::Event`; заместитель
    /// отдаёт их каналу учётки.
    MailEvent { session: u64, event: SyncEvent },
    /// Услуга хранилища учётки — вызовом связи (PLG-08): чтение с ответом — [`ToPlugin::Cached`]. База открыта
    /// только у ядра (ADR 0006).
    Cache { session: u64, op: CacheOp },
    /// Запись хранилища учётки без ответа (PLG-08): идут подряд, ошибка — на ближайшем вызове с ответом
    /// ([`FromPlugin::Cache`]), порядок сохраняется.
    CacheWrite { session: u64, op: CacheOp },
    /// Разбор письма воркером — услуга ядра вызовом связи (PLG-08): воркер запускает ядро, не плагин. Ответ —
    /// [`ToPlugin::Worked`].
    Worker { op: WorkerOp },
    /// S/MIME исходящего — услуга ядра вызовом связи (PLG-08): ключи пользователя плагину недоступны. Ответ —
    /// [`ToPlugin::Sealed`].
    Smime { op: SmimeOp },
}

/// Вызов `PluginStore`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StoreOp {
    Get(String),
    Put(String, Vec<u8>),
    RemovePrefix(String),
}

/// Шаг услуги SSO у ядра ([`FromPlugin::Sso`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SsoOp {
    /// Слушать `127.0.0.1:0` для возврата браузера; ответ — порт ([`SsoDone::Port`]).
    Listen,
    /// Ждать возврата браузера (`loginToken`); ответ — токен ([`SsoDone::Token`]).
    Wait,
}

/// Ответ ядра на услугу SSO.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SsoDone {
    Port(u16),
    Token(String),
}

/// Ответ плагина.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Reply {
    Hello(PluginHello),
    /// Рабочий набор процесса, байт (0 — ОС не дала).
    Stats {
        rss: u64,
    },
    Features(ChannelFeatures),
    Address(Option<Address>),
    Url(Option<String>),
    Opened(Result<Opened, ChannelError>),
    Unit(Result<(), ChannelError>),
    Prompt(Result<Prompt, ChannelError>),
    Page(Result<Page, ChannelError>),
    Done(Result<Done, ChannelError>),
    Uploaded(Result<Uploaded, ChannelError>),
    /// Сертификаты из каталога ([`Ask::Certificates`], PLG-06) — DER как есть: чужие данные, их проверяет воркер.
    Certs(Result<Vec<Vec<u8>>, DirectoryError>),
    /// Что нашлось в области учётки ([`Ask::AreaOpen`], PLG-07): возможности сервера.
    AreaOpened(Result<Features, ModelError>),
    /// Команда области ([`Ask::AreaCommand`], PLG-07): `true` — область её выполнила, `false` — «не моя, назад».
    /// Ошибка обмена — как у каналов: заместитель лечит её подъёмом процесса.
    AreaCommand(Result<bool, ChannelError>),
    /// Что сделала область с ответом на приглашение ([`Ask::AreaRespond`], PLG-07).
    AreaResponded(Result<Responded, ModelError>),
    /// Изменилось ли что-то ([`Ask::AreaChanged`], PLG-07).
    AreaChanged(Result<bool, ModelError>),
    /// Данные хранилища задач плагина ([`Ask::AreaTask`], PLG-07).
    AreaTask(Result<TaskDone, ModelError>),
    /// Публичная загрузка ([`Ask::Fetch`], PLG-07): `Body` — тело двоичной частью кадра, `NotModified` — ресурс
    /// не менялся; ошибка — строкой, как её показал бы интерфейс.
    Fetched(Result<Fetched, String>),
    /// Синхронизация учётки запущена ([`Ask::MailOpen`], PLG-08); `Err` — поток не завёлся (ошибка — текстом).
    MailOpened(Result<(), String>),
}

#[cfg(test)]
mod tests {
    //! Каждый вариант проходит «туда и обратно» без потерь. Списки примеров сверяются с `match` без `_`: новый вариант
    //! без примера здесь не собирается.

    use super::*;
    use crate::channel::{
        Attachment, AttachmentKind, Body, ChannelFolder, ChannelState, CodeVia, Conversation, ConversationKind, Counts, Done, Extra, ExtraReply,
        FolderRef, Member, Message, MessageKind, Origin, Outgoing, ParticipantId, Quote, Reaction, Reader, Role, ServiceKind, text_doc,
    };

    fn round<T: Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug>(v: &T) {
        let bytes = postcard::to_stdvec(v).unwrap();
        assert_eq!(&postcard::from_bytes::<T>(&bytes).unwrap(), v);
    }

    fn conv() -> ConversationId {
        ConversationId::new("g-team")
    }

    fn msg() -> MessageId {
        MessageId::new("m7")
    }

    fn person() -> Participant {
        Participant {
            id: ParticipantId::new("u-anna"),
            name: "Анна".into(),
            handle: Some("anna".into()),
            phone: None,
            emails: vec!["anna@example.test".into()],
            avatar: Some(AttachmentSource(vec![1, 2])),
            bot: false,
        }
    }

    fn message() -> Message {
        Message {
            id: msg(),
            author: ParticipantId::new("u-anna"),
            sent: 1_768_467_600_000,
            edited: Some(1_768_467_660_000),
            kind: MessageKind::Service(ServiceKind::Renamed("Команда".into())),
            body: Body::Ir(text_doc("Привет\nвторая строка")),
            attachments: vec![Attachment {
                name: "photo.png".into(),
                mime: "image/png".into(),
                size: Some(4096),
                kind: AttachmentKind::Image { w: 64, h: 64 },
                source: AttachmentSource(vec![7]),
                thumb: None,
            }],
            reply_to: Some(MessageId::new("m1")),
            quotes: vec![Quote { text: "готова".into(), source: Some(MessageId::new("m1")) }],
            thread: None,
            forwarded: Some(Origin { name: "Борис".into(), author: None, sent: Some(1) }),
            reactions: vec![Reaction { key: "👍".into(), count: 2, mine: true, who: vec![ParticipantId::new("u-me")] }],
            mentions: vec![ParticipantId::new("u-me")],
            mine: false,
            extra: Extra(b"button\tOK\tok".to_vec()),
        }
    }

    fn outgoing() -> Outgoing {
        Outgoing {
            conversation: conv(),
            client_id: 42,
            body: text_doc("ответ"),
            attachments: Vec::new(),
            reply_to: Some(msg()),
            quotes: Vec::new(),
            mentions: Vec::new(),
            at: Some(5),
            silent: true,
        }
    }

    fn address() -> Address {
        Address::message(&crate::channel::ChannelId::new("demo"), &AccountId::new("me@example.test"), &conv(), &msg())
    }

    fn events() -> Vec<ChannelEvent> {
        let all = vec![
            ChannelEvent::Conversations(vec![Conversation {
                id: conv(),
                kind: ConversationKind::Group,
                title: "Команда".into(),
                unread: Some(Counts { unread: 1, mentions: 1 }),
                my_role: Role::Admin,
                ..Conversation::default()
            }]),
            ChannelEvent::ConversationGone(conv()),
            ChannelEvent::Messages { conversation: conv(), messages: vec![message()], live: true },
            ChannelEvent::Deleted { conversation: None, ids: vec![msg()], for_all: true },
            ChannelEvent::Reactions { conversation: conv(), message: msg(), reactions: Vec::new() },
            ChannelEvent::Read { conversation: conv(), who: Reader::Them(ParticipantId::new("u-anna")), up_to: msg() },
            ChannelEvent::Typing { conversation: conv(), who: ParticipantId::new("u-anna"), on: true },
            ChannelEvent::Members {
                conversation: conv(),
                changed: vec![Member { id: ParticipantId::new("u-me"), role: Role::Owner }],
                left: Vec::new(),
            },
            ChannelEvent::People(vec![person()]),
            ChannelEvent::Folders(vec![ChannelFolder {
                id: FolderRef::new("f"), name: "Работа".into(), parent: None, conversations: vec![conv()]
            }]),
            ChannelEvent::Gap { conversation: Some(conv()) },
            ChannelEvent::Cursor(Cursor(b"17".to_vec())),
            ChannelEvent::Secret(Secret::new(b"session".to_vec())),
            ChannelEvent::Features(ChannelFeatures { max_text: Some(4096), ..ChannelFeatures::default() }),
            ChannelEvent::State(ChannelState::Problem(ChannelError::Plugin("crashed".into()))),
        ];
        let covered = |e: &ChannelEvent| match e {
            ChannelEvent::Conversations(_) => 0,
            ChannelEvent::ConversationGone(_) => 1,
            ChannelEvent::Messages { .. } => 2,
            ChannelEvent::Deleted { .. } => 3,
            ChannelEvent::Reactions { .. } => 4,
            ChannelEvent::Read { .. } => 5,
            ChannelEvent::Typing { .. } => 6,
            ChannelEvent::Members { .. } => 7,
            ChannelEvent::People(_) => 8,
            ChannelEvent::Folders(_) => 9,
            ChannelEvent::Gap { .. } => 10,
            ChannelEvent::Cursor(_) => 11,
            ChannelEvent::Secret(_) => 12,
            ChannelEvent::Features(_) => 13,
            ChannelEvent::State(_) => 14,
        };
        assert_eq!(all.iter().map(covered).collect::<Vec<_>>(), (0..all.len()).collect::<Vec<_>>());
        all
    }

    fn actions() -> Vec<Action> {
        let all = vec![
            Action::Send(outgoing()),
            Action::Edit { conversation: conv(), message: msg(), body: text_doc("правка"), mentions: Vec::new() },
            Action::Delete { conversation: conv(), ids: vec![msg()], for_all: false },
            Action::React { conversation: conv(), message: msg(), key: "🎉".into(), on: true },
            Action::MarkRead { conversation: conv(), up_to: msg() },
            Action::Typing { conversation: conv(), on: false },
            Action::Forward { from: conv(), ids: vec![msg()], to: ConversationId::new("d-anna") },
            Action::Mute { conversation: conv(), until: Some(9) },
            Action::Pin { conversation: conv(), on: true },
            Action::SaveDraft { conversation: conv(), draft: Some(text_doc("черновик")) },
            Action::Start { with: vec![ParticipantId::new("u-anna")] },
            Action::Members { conversation: conv() },
            Action::Resolve { link: "https://chat.example.test/g-team".into() },
            Action::Extra { conversation: conv(), message: msg(), payload: Extra(vec![1]) },
        ];
        let covered = |a: &Action| match a {
            Action::Send(_) => 0,
            Action::Edit { .. } => 1,
            Action::Delete { .. } => 2,
            Action::React { .. } => 3,
            Action::MarkRead { .. } => 4,
            Action::Typing { .. } => 5,
            Action::Forward { .. } => 6,
            Action::Mute { .. } => 7,
            Action::Pin { .. } => 8,
            Action::SaveDraft { .. } => 9,
            Action::Start { .. } => 10,
            Action::Members { .. } => 11,
            Action::Resolve { .. } => 12,
            Action::Extra { .. } => 13,
        };
        assert_eq!(all.iter().map(covered).collect::<Vec<_>>(), (0..all.len()).collect::<Vec<_>>());
        all
    }

    fn prompts() -> Vec<Prompt> {
        let all = vec![
            Prompt::Server,
            Prompt::Phone,
            Prompt::Login,
            Prompt::Code { via: CodeVia::Sms, length: Some(5) },
            Prompt::Password { hint: Some("демо".into()) },
            Prompt::Qr { url: "tg://login?token=x".into(), expires: 30 },
            Prompt::Browser { url: "https://example.test/sso".into() },
            Prompt::Compare { emoji: vec![("🐶".into(), "Собака".into())] },
            Prompt::RecoveryKey,
            Prompt::Done { account: AccountId::new("me@example.test"), name: "Я".into(), secret: Secret::new(b"s".to_vec()) },
            Prompt::Finished,
        ];
        let covered = |p: &Prompt| match p {
            Prompt::Server => 0,
            Prompt::Phone => 1,
            Prompt::Login => 2,
            Prompt::Code { .. } => 3,
            Prompt::Password { .. } => 4,
            Prompt::Qr { .. } => 5,
            Prompt::Browser { .. } => 6,
            Prompt::Compare { .. } => 7,
            Prompt::RecoveryKey => 8,
            Prompt::Done { .. } => 9,
            Prompt::Finished => 10,
        };
        assert_eq!(all.iter().map(covered).collect::<Vec<_>>(), (0..all.len()).collect::<Vec<_>>());
        all
    }

    fn answers() -> Vec<Answer> {
        let all = vec![Answer::Text("+70000000000".into()), Answer::Secret(Secret::new(b"2fa".to_vec())), Answer::Confirm(true), Answer::Resend];
        let covered = |a: &Answer| match a {
            Answer::Text(_) => 0,
            Answer::Secret(_) => 1,
            Answer::Confirm(_) => 2,
            Answer::Resend => 3,
        };
        assert_eq!(all.iter().map(covered).collect::<Vec<_>>(), (0..all.len()).collect::<Vec<_>>());
        all
    }

    fn dones() -> Vec<Done> {
        let all = vec![
            Done::Ok,
            Done::Sent { client_id: 42, id: msg(), at: 3 },
            Done::Conversation(conv()),
            Done::Members(vec![Member { id: ParticipantId::new("u-me"), role: Role::Member }]),
            Done::Address(address()),
            Done::Reply(ExtraReply { text: Some("Перезапускаю".into()), alert: true, url: None }),
        ];
        let covered = |d: &Done| match d {
            Done::Ok => 0,
            Done::Sent { .. } => 1,
            Done::Conversation(_) => 2,
            Done::Members(_) => 3,
            Done::Address(_) => 4,
            Done::Reply(_) => 5,
        };
        assert_eq!(all.iter().map(covered).collect::<Vec<_>>(), (0..all.len()).collect::<Vec<_>>());
        all
    }

    fn errors() -> Vec<ChannelError> {
        let all = vec![
            ChannelError::Network("offline".into()),
            ChannelError::SignedOut("no session".into()),
            ChannelError::Wait { secs: 3 },
            ChannelError::Denied("code-invalid".into()),
            ChannelError::NotFound,
            ChannelError::TooLarge { max: 10 },
            ChannelError::Unsupported,
            ChannelError::Protocol("bad".into()),
            ChannelError::Storage("disk".into()),
            ChannelError::Plugin("exit 3".into()),
            ChannelError::Missing("postman-plugins-matrix".into()),
        ];
        let covered = |e: &ChannelError| match e {
            ChannelError::Network(_) => 0,
            ChannelError::SignedOut(_) => 1,
            ChannelError::Wait { .. } => 2,
            ChannelError::Denied(_) => 3,
            ChannelError::NotFound => 4,
            ChannelError::TooLarge { .. } => 5,
            ChannelError::Unsupported => 6,
            ChannelError::Protocol(_) => 7,
            ChannelError::Storage(_) => 8,
            ChannelError::Plugin(_) => 9,
            ChannelError::Missing(_) => 10,
        };
        assert_eq!(all.iter().map(covered).collect::<Vec<_>>(), (0..all.len()).collect::<Vec<_>>());
        all
    }

    fn calls() -> Vec<Call> {
        let all = vec![
            Call::CatchUp(Some(Cursor(b"1".to_vec()))),
            Call::History { conversation: conv(), before: Some(msg()), limit: 50 },
            Call::Search { query: "отчёт".into(), conversation: None, limit: 20 },
            Call::Act(Action::Pin { conversation: conv(), on: false }),
            Call::Download(AttachmentSource(vec![3])),
            Call::Upload(FileMeta { name: "a.txt".into(), mime: "text/plain".into(), size: 3 }),
            Call::Flow { flow: 2, kind: FlowKind::Verify },
            Call::SignOut,
        ];
        let covered = |c: &Call| match c {
            Call::CatchUp(_) => 0,
            Call::History { .. } => 1,
            Call::Search { .. } => 2,
            Call::Act(_) => 3,
            Call::Download(_) => 4,
            Call::Upload(_) => 5,
            Call::Flow { .. } => 6,
            Call::SignOut => 7,
        };
        assert_eq!(all.iter().map(covered).collect::<Vec<_>>(), (0..all.len()).collect::<Vec<_>>());
        all
    }

    fn asks() -> Vec<Ask> {
        let all = vec![
            Ask::Hello(HostHello { build: Build::current(), settings: Settings { log: "info".into(), tls13_only: true, proxy: None } }),
            Ask::Stats,
            Ask::Features,
            Ask::ParseLink("https://chat.example.test/g-team/m7".into()),
            Ask::Link(address()),
            Ask::PersonUrl(person()),
            Ask::Open { session: 1, account: AccountId::new("me@example.test"), secret: Secret::new(b"s".to_vec()) },
            Ask::SignIn { flow: 2 },
            Ask::Step { flow: 2, answer: Some(Answer::Resend) },
            Ask::Session { session: 1, call: Call::SignOut },
            Ask::Certificates(DirectoryQuery {
                server: Some("ldaps://dc.corp.example".into()),
                base_dn: None,
                starttls: true,
                login: DirectoryLogin::Password { user: "CN=svc".into(), password: Secret::new(b"pw".to_vec()) },
                addresses: vec!["a@corp.example".into()],
            }),
            Ask::AreaOpen {
                session: 3,
                url: "https://dav.example.test".into(),
                profile: None,
                login: AreaLogin { user: "me@example.test".into(), password: Secret::new(b"pw".to_vec()), email: "me@example.test".into() },
            },
            Ask::AreaCommand { session: 3, cmd: Box::new(area_command()) },
            Ask::AreaRespond {
                session: 3,
                invite: Box::new(invitation()),
                status: Partstat::Accepted,
                answer: InviteAnswer { comment: Some("буду".into()), silent: false },
            },
            Ask::AreaChanged { session: 3 },
            Ask::AreaTask { session: 3, op: task_op() },
            Ask::Probe("dav.example.test".into()),
            Ask::Fetch { url: "https://cal.example.test/f.ics".into(), max: 1024, etag: Some("\"1\"".into()) },
            Ask::MailOpen {
                session: 4,
                url: "https://mail.example.test/EWS/Exchange.asmx".into(),
                profile: Some("@2".into()),
                login: MailLogin { user: "user".into(), password: Secret::new(b"pw".to_vec()) },
            },
            Ask::MailCommand { session: 4, cmd: Box::new(mail_command()) },
        ];
        let covered = |a: &Ask| match a {
            Ask::Hello(_) => 0,
            Ask::Stats => 1,
            Ask::Features => 2,
            Ask::ParseLink(_) => 3,
            Ask::Link(_) => 4,
            Ask::PersonUrl(_) => 5,
            Ask::Open { .. } => 6,
            Ask::SignIn { .. } => 7,
            Ask::Step { .. } => 8,
            Ask::Session { .. } => 9,
            Ask::Certificates(_) => 10,
            Ask::AreaOpen { .. } => 11,
            Ask::AreaCommand { .. } => 12,
            Ask::AreaRespond { .. } => 13,
            Ask::AreaChanged { .. } => 14,
            Ask::AreaTask { .. } => 15,
            Ask::Probe(_) => 16,
            Ask::Fetch { .. } => 17,
            Ask::MailOpen { .. } => 18,
            Ask::MailCommand { .. } => 19,
        };
        assert_eq!(all.iter().map(covered).collect::<Vec<_>>(), (0..all.len()).collect::<Vec<_>>());
        all
    }

    fn replies() -> Vec<Reply> {
        let all = vec![
            Reply::Hello(PluginHello { build: Build::current(), id: "telegram".into(), kind: Kind::Channel, key: true }),
            Reply::Stats { rss: 8 << 20 },
            Reply::Features(ChannelFeatures::default()),
            Reply::Address(Some(address())),
            Reply::Url(None),
            Reply::Opened(Ok(Opened { me: person(), features: ChannelFeatures::default() })),
            Reply::Unit(Err(ChannelError::NotFound)),
            Reply::Prompt(Ok(Prompt::Phone)),
            Reply::Page(Ok(Page { messages: vec![(conv(), message())], more: true })),
            Reply::Done(Ok(Done::Ok)),
            Reply::Uploaded(Ok(Uploaded {
                name: "a".into(),
                mime: "b".into(),
                size: 1,
                kind: AttachmentKind::Voice { secs: 2 },
                source: AttachmentSource(vec![]),
            })),
            Reply::Certs(Ok(vec![b"der".to_vec(), b"pkcs7".to_vec()])),
            Reply::AreaOpened(Ok(Features::NONE.with(crate::model::Feature::Calendar, true).with(crate::model::Feature::Tasks, true))),
            Reply::AreaCommand(Ok(true)),
            Reply::AreaCommand(Err(ChannelError::Network("plugin process is not running".into()))),
            Reply::AreaResponded(Ok(Responded::Sent)),
            Reply::AreaResponded(Err(crate::model::Error::Protocol("no calendar".into()))),
            Reply::AreaChanged(Ok(true)),
            Reply::AreaTask(Ok(task_done())),
            Reply::Fetched(Ok(Fetched::Body { etag: Some("\"2\"".into()) })),
            Reply::Fetched(Ok(Fetched::NotModified)),
            Reply::Fetched(Err("bad public address".into())),
            Reply::MailOpened(Ok(())),
            Reply::MailOpened(Err("runtime: no threads".into())),
        ];
        let covered = |r: &Reply| match r {
            Reply::Hello(_) => 0,
            Reply::Stats { .. } => 1,
            Reply::Features(_) => 2,
            Reply::Address(_) => 3,
            Reply::Url(_) => 4,
            Reply::Opened(_) => 5,
            Reply::Unit(_) => 6,
            Reply::Prompt(_) => 7,
            Reply::Page(_) => 8,
            Reply::Done(_) => 9,
            Reply::Uploaded(_) => 10,
            Reply::Certs(_) => 11,
            Reply::AreaOpened(_) => 12,
            Reply::AreaCommand(Ok(_)) => 13,
            Reply::AreaCommand(Err(_)) => 14,
            Reply::AreaResponded(Ok(_)) => 15,
            Reply::AreaResponded(Err(_)) => 16,
            Reply::AreaChanged(_) => 17,
            Reply::AreaTask(_) => 18,
            Reply::Fetched(Ok(Fetched::Body { .. })) => 19,
            Reply::Fetched(Ok(Fetched::NotModified)) => 20,
            Reply::Fetched(Err(_)) => 21,
            Reply::MailOpened(Ok(())) => 22,
            Reply::MailOpened(Err(_)) => 23,
        };
        assert_eq!(all.iter().map(covered).collect::<Vec<_>>(), (0..all.len()).collect::<Vec<_>>());
        all
    }

    /// Команда областей для примера: неделя календаря.
    fn area_command() -> SyncCommand {
        SyncCommand::Events { from: 1_768_400_000, to: 1_768_900_000 }
    }

    /// Событие областей для примера.
    fn area_event() -> SyncEvent {
        SyncEvent::CalendarChanged
    }

    /// Приглашение из письма (`text/calendar`).
    fn invitation() -> Invitation {
        let v = serde_json::json!({ "method": "REQUEST", "data": {
            "@type": "Event", "uid": "mtg-7@example.test", "title": "Планёрка",
            "start": "2026-10-20T10:00:00", "timeZone": "Europe/Moscow", "duration": "PT1H",
            "participants": {
                "o": { "name": "Иван", "calendarAddress": "mailto:ivan@example.test", "roles": { "owner": true } },
                "m": { "calendarAddress": "mailto:me@example.test", "roles": { "attendee": true }, "participationStatus": "needs-action" }
            }
        }});
        Invitation::from_worker(&v, &jiff::tz::TimeZone::UTC).unwrap()
    }

    /// Операция хранилища задач плагина: правка с патчем JMAP.
    fn task_op() -> TaskOp {
        TaskOp::Update {
            id: "/dav/cal/u/default/t1.ics".into(),
            patch: serde_json::from_value::<Map<String, Value>>(serde_json::json!({ "title/xx": "готово", "keywords/x": null })).unwrap(),
        }
    }

    /// Ответ хранилища задач: записи и состояние.
    fn task_done() -> TaskDone {
        TaskDone::Loaded { tasks: vec![serde_json::json!({ "@type": "Task", "title": "Отчёт" })], state: "/dav/cal/u/default/\tsync:3".into() }
    }

    /// Задача едет байтами JSON, но остаётся тем же значением.
    #[test]
    fn task_values_cross_the_wire_as_json() {
        round(&TaskDone::Delta(Delta {
            changed: vec![serde_json::json!({ "@type": "Task", "uid": "t2" }), serde_json::json!([1, "x", null, { "a": true }])],
            destroyed: vec!["/dav/cal/u/default/gone.ics".into()],
            state: "sync:4".into(),
        }));
        round(&TaskOp::Create(serde_json::json!({ "@type": "Task", "title": "Новая" })));
    }

    #[test]
    fn every_variant_round_trips() {
        for kind in [Kind::Channel, Kind::Mail, Kind::Area, Kind::Directory] {
            round(&kind);
        }
        events().iter().for_each(round);
        actions().iter().for_each(round);
        prompts().iter().for_each(round);
        answers().iter().for_each(round);
        dones().iter().for_each(round);
        errors().iter().for_each(round);
        calls().iter().for_each(round);
        for ask in asks() {
            round(&ToPlugin::Ask { id: 9, ask });
        }
        for reply in replies() {
            round(&FromPlugin::Answer { id: 9, reply });
        }
        let to = [
            ToPlugin::Settings(Settings::default()),
            ToPlugin::Close { session: 1 },
            ToPlugin::CloseFlow { flow: 2 },
            ToPlugin::Leave,
            ToPlugin::Stored(Ok(Some(b"v".to_vec()))),
            ToPlugin::SsoReply(Ok(SsoDone::Port(7))),
            ToPlugin::SsoReply(Ok(SsoDone::Token("login-token".into()))),
            ToPlugin::SsoReply(Err(ChannelError::Denied(crate::channel::matrix::BROWSER_TIMEOUT.into()))),
            ToPlugin::Cached(Ok(cache_done())),
            ToPlugin::Worked(Ok(WorkerDone::Bytes(b"part".to_vec()))),
            ToPlugin::Sealed(Ok(b"sealed".to_vec())),
            ToPlugin::Cached(Err("cache is not writable".into())),
            ToPlugin::Worked(Err("worker did not answer".into())),
            ToPlugin::Sealed(Err("no certificate for the recipient".into())),
        ];
        to.iter().for_each(round);
        let covered = |m: &ToPlugin| match m {
            ToPlugin::Ask { .. } => 0,
            ToPlugin::Settings(_) => 1,
            ToPlugin::Close { .. } => 2,
            ToPlugin::CloseFlow { .. } => 3,
            ToPlugin::Leave => 4,
            ToPlugin::Stored(_) => 5,
            ToPlugin::SsoReply(Ok(SsoDone::Port(_))) => 6,
            ToPlugin::SsoReply(Ok(SsoDone::Token(_))) => 7,
            ToPlugin::SsoReply(Err(_)) => 8,
            ToPlugin::Cached(Ok(_)) => 9,
            ToPlugin::Worked(Ok(_)) => 10,
            ToPlugin::Sealed(Ok(_)) => 11,
            ToPlugin::Cached(Err(_)) => 12,
            ToPlugin::Worked(Err(_)) => 13,
            ToPlugin::Sealed(Err(_)) => 14,
        };
        assert_eq!(to.iter().map(covered).collect::<Vec<_>>(), (1..=to.len()).collect::<Vec<_>>());
        let from = [
            FromPlugin::Event { session: 1, event: ChannelEvent::Gap { conversation: None } },
            FromPlugin::Store { scope: 1, op: StoreOp::Get("k".into()) },
            FromPlugin::Store { scope: 1, op: StoreOp::Put("k".into(), b"v".to_vec()) },
            FromPlugin::Store { scope: 1, op: StoreOp::RemovePrefix("state/".into()) },
            FromPlugin::Sso { flow: 2, op: SsoOp::Listen },
            FromPlugin::Sso { flow: 2, op: SsoOp::Wait },
            FromPlugin::AreaEvent { session: 3, event: area_event() },
            FromPlugin::MailEvent { session: 4, event: mail_event() },
            FromPlugin::Cache { session: 4, op: cache_op() },
            FromPlugin::CacheWrite { session: 4, op: CacheOp::UpsertEmails(Vec::new()) },
            FromPlugin::Worker { op: WorkerOp::Part { raw: b"raw".to_vec(), index: 2 } },
            FromPlugin::Smime { op: SmimeOp::Seal { raw: b"raw".to_vec(), to: Vec::new() } },
        ];
        from.iter().for_each(round);
        let covered = |m: &FromPlugin| match m {
            FromPlugin::Answer { .. } => 0,
            FromPlugin::Event { .. } => 1,
            FromPlugin::Store { .. } => 2,
            FromPlugin::Sso { .. } => 3,
            FromPlugin::AreaEvent { .. } => 4,
            FromPlugin::MailEvent { .. } => 5,
            FromPlugin::Cache { .. } => 6,
            FromPlugin::CacheWrite { .. } => 7,
            FromPlugin::Worker { .. } => 8,
            FromPlugin::Smime { .. } => 9,
        };
        assert!(from.iter().map(covered).all(|i| i > 0), "ответы — выше");
    }

    /// Команда почтового пути для примера: тело письма.
    fn mail_command() -> SyncCommand {
        SyncCommand::Body("key".into())
    }

    /// Событие почтового пути для примера: список папок изменился.
    fn mail_event() -> SyncEvent {
        SyncEvent::MailboxesChanged
    }

    /// Операция хранилища учётки для примера: чтение состояния синхронизации.
    fn cache_op() -> CacheOp {
        CacheOp::State("ews.f1".into())
    }

    /// Ответ хранилища для примера: состояния нет.
    fn cache_done() -> CacheDone {
        CacheDone::State(None)
    }

    /// Запись папки ([`CacheOp::UpsertFolder`], PLG-09 — RENAME у IMAP) — по обмену туда и обратно: строка папки,
    /// ответ — её ключ в кэше.
    #[test]
    fn upsert_folder_crosses_the_wire() {
        round(&CacheOp::UpsertFolder(FolderRow {
            owner: postman_cache::Owner::Account("acc".into()),
            parent: None,
            name: "Архив".into(),
            role: None,
            remote: Some("f0123".into()),
        }));
        round(&CacheDone::FolderUpserted(postman_cache::FolderId(7)));
    }

    /// Разбор воркером по обмену — то же значение (`Mime` воркера байтами postcard).
    #[test]
    fn worker_result_crosses_the_wire_as_bytes() {
        use postman_worker::mime::{Addr, Mime};
        let mime = Mime {
            message_id: Some("<b@example.test>".into()),
            references: Vec::new(),
            to: vec![Addr { name: "Анна".into(), email: "anna@example.test".into() }],
            ..Mime::default()
        };
        round(&ToPlugin::Worked(Ok(WorkerDone::Mime(mime.clone()))));
        let bytes = postcard::to_stdvec(&ToPlugin::Worked(Ok(WorkerDone::Mime(mime)))).unwrap();
        let back = postcard::from_bytes::<ToPlugin>(&bytes).unwrap();
        let ToPlugin::Worked(Ok(WorkerDone::Mime(back))) = back else { panic!("не тот ответ") };
        assert_eq!(back.message_id.as_deref(), Some("<b@example.test>"));
        assert_eq!(back.to.len(), 1);
    }

    /// Секрет идёт по обмену байтами, но `Debug` по-прежнему без значения.
    #[test]
    fn secret_crosses_the_wire_but_not_the_log() {
        let open = ToPlugin::Ask { id: 1, ask: Ask::Open { session: 1, account: AccountId::new("a"), secret: Secret::new(b"key".to_vec()) } };
        round(&open);
        assert!(format!("{open:?}").contains("Secret(3 bytes)"), "{open:?}");
    }
}
