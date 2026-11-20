//! Сторона плагина (ADR 0013): процесс набора отвечает хосту за свой `ChannelPlugin`. До первого вопроса, кроме
//! `Hello`, — ни рантайма, ни самого плагина: процесс только заперт и отвечает на приветствие. Рантайм tokio (поток и
//! `LocalSet`, как у потока учётки в `postman.exe`) и плагин создаются при первом настоящем вопросе (`Open`, вход).
//!
//! Вопросы описания (`features`, `parse_link`, `link`, `person_url`, память) отвечает поток чтения связи: они не ждут
//! занятого плагина, а хост не ждёт их, пока плагин ждёт ядро (вызов хранилища).

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use postman_calendar::Owner;
use postman_proc::link::Incoming;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use super::super::mail::{MailServer, MailServices, RemoteCache, RemoteIndex, RemoteWorker, WireSeal};
use super::super::wire::{
    AreaLogin, Ask, Build, Call, DirectoryQuery, Fetched, FromPlugin, HostHello, Kind, MailLogin, Opened, PluginHello, Reply, Settings, SsoDone,
    SsoOp, StoreOp, TaskDone, TaskOp, ToPlugin,
};
use super::{BROWSER_WAIT as HOST_BROWSER_WAIT, MAX_BIN, PluginLink};
use super::{Claim, Profile, Row};
use crate::backend::{AreaCtx, Areas, BoxFuture, Login};
use crate::channel::{
    AccountId, Answer, ByteSink, ByteSource, ChannelError, ChannelEvent, ChannelPlugin, ChannelSession, ConnectCtx, FlowCtx, PluginStore, Secret, Sso,
};
use crate::model::Error as ModelError;
use crate::smime::directory::{Directory, DirectoryError};
use crate::sync::{Command as SyncCommand, Event as SyncEvent};
use postman_cache::Cache;

/// Вызов хранилища ядра ждёт ответа не дольше (и услуг хранилища учётки, PLG-08).
pub(in crate::backend) const STORE_WAIT: Duration = Duration::from_secs(30);
/// Ожидание возврата браузера у ядра ([`HOST_BROWSER_WAIT`]) плюс запас на обмен.
const SSO_WAIT: Duration = Duration::from_secs(HOST_BROWSER_WAIT.as_secs() + 30);

/// Куда процесс набора применяет уровень журнала из `Hello` и смены настроек.
static LOG: OnceLock<fn(&str)> = OnceLock::new();
static SETTINGS: Mutex<Option<Settings>> = Mutex::new(None);
/// Рабочий набор процесса, если ОС отдаёт его не так, как `bench::rss` (Linux в песочнице: `/proc` закрыт).
static STATS: OnceLock<fn() -> u64> = OnceLock::new();

/// Задать замер рабочего набора процесса (процесс набора, один раз до [`serve_channel`]).
pub fn on_stats(rss: fn() -> u64) {
    let _ = STATS.set(rss);
}

/// Задать применение уровня журнала (процесс набора, один раз до [`serve_channel`]).
pub fn on_log(apply: fn(&str)) {
    let _ = LOG.set(apply);
}

/// Настройки ядра в процессе плагина (политика TLS, прокси): из `Hello` и последней смены.
pub fn settings() -> Settings {
    SETTINGS.lock().unwrap_or_else(PoisonError::into_inner).clone().unwrap_or_default()
}

fn apply(s: Settings) {
    if let Some(log) = LOG.get() {
        log(&s.log);
    }
    *SETTINGS.lock().unwrap_or_else(PoisonError::into_inner) = Some(s);
}

fn answer(link: &PluginLink, id: u64, reply: Reply, bin: &[u8]) {
    if let Err(e) = link.send(&FromPlugin::Answer { id, reply }, bin) {
        tracing::warn!("plugin answer not sent: {e}");
    }
}

/// Задача плагина не паникует молча (PLG-12, хвост PLG-07): паника в задаче `spawn_local` оставляла вопрос
/// заместителя без ответа — его `Conn::ask` ждёт без таймаута, заместитель висел навсегда. Паника — в журнал
/// и отказ по обмену тем же `id`.
fn spawn_quiet(link: &Arc<PluginLink>, id: u64, task: impl Future<Output = ()> + 'static) {
    use std::panic::AssertUnwindSafe;
    let link = Arc::clone(link);
    let done = async move {
        if futures::FutureExt::catch_unwind(AssertUnwindSafe(task)).await.is_err() {
            tracing::error!("plugin task panicked: answering a refusal so the host does not hang");
            answer(&link, id, Reply::Unit(Err(ChannelError::Protocol("plugin task panicked".into()))), &[]);
        }
    };
    tokio::task::spawn_local(done);
}

/// Вопрос, на который отвечают сразу, без рантайма. `false` — не такой.
fn quick(link: &PluginLink, id: &'static str, key: bool, plugin: Option<&dyn ChannelPlugin>, n: u64, ask: &Ask) -> bool {
    let reply = match (ask, plugin) {
        (Ask::Hello(h), _) => {
            apply(h.settings.clone());
            Reply::Hello(PluginHello { build: Build::current(), id: id.into(), kind: Kind::Channel, key })
        }
        (Ask::Stats, _) => Reply::Stats { rss: STATS.get().map_or_else(|| crate::bench::rss() as u64, |rss| rss()) },
        (Ask::Features, Some(p)) => Reply::Features(p.features()),
        (Ask::ParseLink(url), Some(p)) => Reply::Address(p.parse_link(url)),
        (Ask::Link(at), Some(p)) => Reply::Url(p.link(at)),
        (Ask::PersonUrl(person), Some(p)) => Reply::Url(p.person_url(person)),
        _ => return false,
    };
    answer(link, n, reply, &[]);
    true
}

/// Обслужить хост каналом `id`; `key` — есть ли у плагина ключ приложения (сообщается в `Hello`, PLG-05), `make`
/// создаёт плагин при первом настоящем вопросе. Ответ — код выхода процесса.
pub fn serve_channel(link: PluginLink, id: &'static str, key: bool, make: impl FnOnce() -> Arc<dyn ChannelPlugin>) -> i32 {
    let link = Arc::new(link);
    let first = loop {
        match link.recv() {
            Incoming::Message(ToPlugin::Ask { id: n, ask }, bin) => {
                if !quick(&link, id, key, None, n, &ask) {
                    break (ToPlugin::Ask { id: n, ask }, bin);
                }
            }
            Incoming::Message(ToPlugin::Settings(s), _) => apply(s),
            Incoming::Message(msg, bin) => break (msg, bin),
            Incoming::Call(..) => {}
            Incoming::Shutdown | Incoming::Closed => return 0,
        }
    };
    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!("plugin runtime: {e}");
            return 1;
        }
    };
    let plugin = make();
    let (tx, rx) = unbounded_channel();
    let _ = tx.send(first);
    let reader = {
        let (link, plugin) = (Arc::clone(&link), Arc::clone(&plugin));
        std::thread::Builder::new().name("plugin-reader".into()).spawn(move || {
            loop {
                match link.recv() {
                    Incoming::Message(ToPlugin::Ask { id: n, ask }, bin) => {
                        if !quick(&link, id, key, Some(&*plugin), n, &ask) && tx.send((ToPlugin::Ask { id: n, ask }, bin)).is_err() {
                            return;
                        }
                    }
                    Incoming::Message(ToPlugin::Settings(s), _) => apply(s),
                    Incoming::Message(msg, bin) => {
                        if tx.send((msg, bin)).is_err() {
                            return;
                        }
                    }
                    Incoming::Call(..) => {}
                    Incoming::Shutdown | Incoming::Closed => return,
                }
            }
        })
    };
    if let Err(e) = reader {
        tracing::error!("plugin reader thread: {e}");
        return 1;
    }
    let served = Rc::new(Served { link, plugin, sessions: RefCell::new(HashMap::new()), flows: RefCell::new(HashMap::new()) });
    tokio::task::LocalSet::new().block_on(&rt, served.run(rx));
    0
}

/// Обслужить хост каталогом `id` (PLG-06): процесс отвечает на поиск сертификатов ([`Ask::Certificates`]) и вопросы
/// описания (`Hello`, память). Рантайма у каталога нет: поиск — [`Directory::certificates`], он сам заводит свой
/// рантайм и ограничен [`TIMEOUT`](crate::smime::directory::TIMEOUT) — в своём потоке, чтобы не задерживать ответы
/// сторожу. Настройки — с каждым запросом («Настройки → Безопасность», пароль — ядро), `make` делает каталог из
/// запроса. Услуги ядра у каталога нет, сессий и входов тоже. Ответ — код выхода процесса.
pub fn serve_directory(link: PluginLink, id: &'static str, make: impl Fn(&DirectoryQuery) -> Arc<dyn Directory>) -> i32 {
    let link = Arc::new(link);
    loop {
        match link.recv() {
            Incoming::Message(ToPlugin::Ask { id: n, ask: Ask::Hello(h) }, _) => {
                apply(h.settings.clone());
                answer(&link, n, Reply::Hello(PluginHello { build: Build::current(), id: id.into(), kind: Kind::Directory, key: false }), &[]);
            }
            Incoming::Message(ToPlugin::Ask { id: n, ask: Ask::Stats }, _) => {
                let rss = STATS.get().map_or_else(|| crate::bench::rss() as u64, |rss| rss());
                answer(&link, n, Reply::Stats { rss }, &[]);
            }
            Incoming::Message(ToPlugin::Ask { id: n, ask: Ask::Certificates(q) }, _) => {
                let dir = make(&q);
                let spawned = std::thread::Builder::new().name("plugin-directory".into()).spawn({
                    let link = Arc::clone(&link);
                    move || answer(&link, n, Reply::Certs(dir.certificates(&q.addresses)), &[])
                });
                if spawned.is_err() {
                    answer(&link, n, Reply::Certs(Err(DirectoryError::Unavailable("directory thread not started".into()))), &[]);
                }
            }
            Incoming::Message(ToPlugin::Settings(s), _) => apply(s),
            // Каталог ничего не держит: выход программы — завершение процесса.
            Incoming::Message(ToPlugin::Leave, _) => return 0,
            // У каталога нет услуг ядра, сессий и входов: остальное — ответы сторожу.
            Incoming::Message(..) | Incoming::Call(..) => {}
            Incoming::Shutdown | Incoming::Closed => return 0,
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Области учётной записи (PLG-07).

/// Путь областей на стороне процесса (PLG-07): вид сервера, вход и публичные загрузки. Команды сессии —
/// [`AreaSession`].
pub trait AreaServer: Send + Sync {
    /// Есть ли путь на узле `host` (`AreaPlugin::probe`); `Some` — его адрес.
    fn probe<'a>(&'a self, host: &'a str) -> BoxFuture<'a, Option<String>>;

    /// Войти в область учётной записи (`AreaPlugin::connect`): сессия команд, ответов на приглашения и опроса.
    /// `profile` — учётная запись профиля (`None` — основная): события сессии плагин метит сам по ней и по адресу
    /// участника из `login`.
    fn connect<'a>(&'a self, url: &'a str, login: &'a Login, profile: Option<&'a str>) -> BoxFuture<'a, Result<Box<dyn AreaSession>, ModelError>>;

    /// Скачать публичный ресурс без входа (`Fetch`, PLG-07): картинка письма, подписка, занятость по ссылке.
    /// `None` — не менялся с `etag` (`If-None-Match`). Тело едет двоичной частью кадра — не больше [`MAX_BIN`].
    fn fetch<'a>(&'a self, url: &'a str, max: u32, etag: Option<&'a str>) -> BoxFuture<'a, Result<Option<FetchedBody>, String>>;
}

/// Публичный ресурс: тело и его ETag (для следующего условного запроса).
pub struct FetchedBody {
    pub body: Vec<u8>,
    pub etag: Option<String>,
}

/// Подключённая область учётной записи в процессе (PLG-07): те же [`Areas`], плюс операции хранилища задач по
/// обмену ([`TaskOp`]). Задачи поверх кэша ведёт ядро ([`crate::tasks::handle`]) — команда `Command::Tasks` сюда
/// не доходит, хранилище плагина спрашивают операциями.
pub trait AreaSession: Areas {
    fn task_op<'a>(&'a mut self, op: TaskOp) -> BoxFuture<'a, Result<TaskDone, ModelError>>;
}

/// Обслужить хост областями `id` (PLG-07): вход (`AreaOpen`), команды (`AreaCommand`, события —
/// `FromPlugin::AreaEvent`), ответ на приглашение (`AreaRespond`), опрос (`AreaChanged`), хранилище задач
/// (`AreaTask`), вид сервера (`Probe`) и публичные загрузки (`Fetch`). Рантайм и плагин — при первом настоящем
/// вопросе, как у каналов; сессии в реестре плагина, кэша у процесса нет — команды задач ядро не посылает, а
/// [`AreaCtx::cache`] нужен договору: держится памятью процесса, свои файлы он не пишет. Ответ — код выхода.
pub fn serve_area(link: PluginLink, id: &'static str, make: impl FnOnce() -> Arc<dyn AreaServer>) -> i32 {
    serve_mail_area(link, id, None, make)
}

/// Приветствие плагина областей или почтового пути: настройки применены, кто он — ответом.
fn hello_reply(link: &Arc<PluginLink>, n: u64, id: &'static str, kind: Kind, h: &HostHello) {
    apply(h.settings.clone());
    answer(link, n, Reply::Hello(PluginHello { build: Build::current(), id: id.into(), kind, key: false }), &[]);
}

/// Обслужить хост почтовым путём и областями `id` (PLG-08): путь — вход синхронизации учётки ([`Ask::MailOpen`]),
/// команды ([`Ask::MailCommand`], события — `FromPlugin::MailEvent`) и вид сервера (`Probe`); области — как
/// [`serve_area`]. Услуги ядра путь зовёт сам по связи (хранилище учётки — `FromPlugin::Cache`, разбор письма —
/// `FromPlugin::Worker`, S/MIME — `FromPlugin::Smime`); кэша у процесса нет — база открыта только у ядра. Ответ —
/// код выхода.
pub fn serve_mail_area(link: PluginLink, id: &'static str, mail: Option<Arc<dyn MailServer>>, make: impl FnOnce() -> Arc<dyn AreaServer>) -> i32 {
    let kind = if mail.is_some() { Kind::Mail } else { Kind::Area };
    let link = Arc::new(link);
    let first = loop {
        match link.recv() {
            Incoming::Message(ToPlugin::Ask { id: n, ask }, bin) => {
                let quick = match &ask {
                    Ask::Hello(h) => {
                        hello_reply(&link, n, id, kind, h);
                        true
                    }
                    Ask::Stats => {
                        let rss = STATS.get().map_or_else(|| crate::bench::rss() as u64, |rss| rss());
                        answer(&link, n, Reply::Stats { rss }, &[]);
                        true
                    }
                    _ => false,
                };
                if !quick {
                    break (ToPlugin::Ask { id: n, ask }, bin);
                }
            }
            Incoming::Message(ToPlugin::Settings(s), _) => apply(s),
            Incoming::Message(..) | Incoming::Call(..) => {}
            Incoming::Shutdown | Incoming::Closed => return 0,
        }
    };
    // Кэш договору областей (`AreaCtx`) нужен только для команд задач, которые ядро не посылает: память процесса.
    let cache = match Cache::open_in_memory() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("area plugin {id}: in-memory cache: {e}");
            return 1;
        }
    };
    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!("plugin runtime: {e}");
            return 1;
        }
    };
    let server = make();
    let (tx, rx) = unbounded_channel();
    let _ = tx.send(first);
    let reader = {
        let link = Arc::clone(&link);
        std::thread::Builder::new().name("plugin-reader".into()).spawn(move || {
            loop {
                match link.recv() {
                    Incoming::Message(ToPlugin::Ask { id: n, ask }, bin) => {
                        let quick = match &ask {
                            Ask::Hello(h) => {
                                hello_reply(&link, n, id, kind, h);
                                true
                            }
                            Ask::Stats => {
                                let rss = STATS.get().map_or_else(|| crate::bench::rss() as u64, |rss| rss());
                                answer(&link, n, Reply::Stats { rss }, &[]);
                                true
                            }
                            _ => false,
                        };
                        if !quick && tx.send((ToPlugin::Ask { id: n, ask }, bin)).is_err() {
                            return;
                        }
                    }
                    Incoming::Message(ToPlugin::Settings(s), _) => apply(s),
                    Incoming::Message(msg, bin) => {
                        if tx.send((msg, bin)).is_err() {
                            return;
                        }
                    }
                    Incoming::Call(..) => {}
                    Incoming::Shutdown | Incoming::Closed => return,
                }
            }
        })
    };
    if let Err(e) = reader {
        tracing::error!("plugin reader thread: {e}");
        return 1;
    }
    let served = Rc::new(ServedAreas { link, server, mail, cache, sessions: RefCell::new(HashMap::new()), mails: RefCell::new(HashMap::new()) });
    tokio::task::LocalSet::new().block_on(&rt, served.run(rx));
    0
}

/// Подключённая сессия области: очередь операций к рабочей задаче. Плагин ведёт одна задача-владелец — по одной
/// операции, как локальный раннер ядра; реестр держит только отправителя, заём на время операции живёт в задаче.
struct Session {
    jobs: UnboundedSender<Job>,
}

/// Операция сессии области — из очереди по одной ([`Session::jobs`]).
enum Job {
    Command { id: u64, cmd: crate::sync::Command },
    Respond { id: u64, respond: Box<(postman_calendar::Invitation, postman_calendar::Partstat, crate::invite::Answer)> },
    Changed { id: u64 },
    TaskOp { id: u64, op: TaskOp },
}

impl Job {
    /// Ответ хосту, когда сессии нет или очередь уже закрыта.
    fn miss(&self) -> Reply {
        match self {
            Job::Command { .. } => Reply::AreaCommand(Ok(false)),
            Job::Respond { .. } => Reply::AreaResponded(Err(ModelError::Protocol("no such area session".into()))),
            Job::Changed { .. } => Reply::AreaChanged(Err(ModelError::Protocol("no such area session".into()))),
            Job::TaskOp { .. } => Reply::AreaTask(Err(ModelError::Protocol("no such area session".into()))),
        }
    }
}

/// Область учётной записи в процессе плагина (и почтовый путь, PLG-08).
struct ServedAreas {
    link: Arc<PluginLink>,
    server: Arc<dyn AreaServer>,
    /// Почтовый путь ([`MailServer`]), если процесс ведёт и синхронизации учёток; `None` — только области.
    mail: Option<Arc<dyn MailServer>>,
    /// Кэш договору областей ([`AreaCtx::cache`]) нужен только для команд задач, которые ядро не посылает:
    /// память процесса, своих файлов он не пишет.
    cache: Cache,
    sessions: RefCell<HashMap<u64, Session>>,
    /// Синхронизации учёток (PLG-08): номер сессии → команды в её поток ([`Ask::MailOpen`]).
    mails: RefCell<HashMap<u64, UnboundedSender<SyncCommand>>>,
}

impl ServedAreas {
    async fn run(self: Rc<Self>, mut rx: UnboundedReceiver<(ToPlugin, Vec<u8>)>) {
        while let Some((msg, _bin)) = rx.recv().await {
            match msg {
                ToPlugin::Ask { id, ask } => self.ask(id, ask),
                ToPlugin::Close { session } => {
                    self.sessions.borrow_mut().remove(&session);
                    // Синхронизация учётки кончается сама, когда канал команд закрылся ([`Ask::MailOpen`]).
                    self.mails.borrow_mut().remove(&session);
                }
                // Потоков вопросов у областей нет — закрывать нечего.
                ToPlugin::CloseFlow { .. } => {}
                ToPlugin::Settings(s) => apply(s),
                // Области держат сессию учётки: `Leave` — только ответ на выход программы. `Cached`/`Worked`/
                // `Sealed`/`Stored`/`SsoReply` — ответы услуг ядра, их забирает поток чтения связи.
                ToPlugin::Leave | ToPlugin::Stored(_) | ToPlugin::SsoReply(_) | ToPlugin::Cached(_) | ToPlugin::Worked(_) | ToPlugin::Sealed(_) => {}
            }
        }
    }

    fn ask(self: &Rc<Self>, id: u64, ask: Ask) {
        match ask {
            Ask::AreaOpen { session, url, profile, login } => {
                spawn_quiet(&self.link, id, Rc::clone(self).open(id, session, url, profile, login));
            }
            Ask::AreaCommand { session, cmd } => self.area_send(session, id, Job::Command { id, cmd: *cmd }),
            Ask::AreaRespond { session, invite, status, answer } => {
                self.area_send(session, id, Job::Respond { id, respond: Box::new((*invite, status, answer)) });
            }
            Ask::AreaChanged { session } => self.area_send(session, id, Job::Changed { id }),
            Ask::AreaTask { session, op } => self.area_send(session, id, Job::TaskOp { id, op }),
            Ask::Probe(url) => {
                spawn_quiet(&self.link, id, Rc::clone(self).probe(id, url));
            }
            Ask::Fetch { url, max, etag } => {
                spawn_quiet(&self.link, id, Rc::clone(self).fetch(id, url, max, etag));
            }
            Ask::MailOpen { session, url, profile, login } => {
                spawn_quiet(&self.link, id, Rc::clone(self).open_mail(id, session, url, profile, login));
            }
            Ask::MailCommand { session, cmd } => {
                let sent = self.mails.borrow().get(&session).is_some_and(|tx| tx.send(*cmd).is_ok());
                answer(&self.link, id, if sent { Reply::Unit(Ok(())) } else { Reply::Unit(Err(ChannelError::NotFound)) }, &[]);
            }
            // Вопросы описания (`Hello`, память) отвечает поток чтения; остальное — не про области.
            other => {
                let _ = other;
                answer(&self.link, id, Reply::Unit(Err(ChannelError::Unsupported)), &[]);
            }
        }
    }

    async fn open(self: Rc<Self>, id: u64, session: u64, url: String, profile: Option<String>, login: AreaLogin) {
        let login = Login { user: login.user, password: String::from_utf8_lossy(login.password.expose()).into_owned(), email: login.email };
        let reply = match self.server.connect(&url, &login, profile.as_deref()).await {
            Ok(area) => {
                let features = area.features();
                let owner = profile.as_ref().map(|_| Owner { email: login.email.clone(), name: login.email.clone() });
                let (jobs, rx) = unbounded_channel();
                tokio::task::spawn_local(Rc::clone(&self).area_worker(session, area, profile.clone(), owner.clone(), rx));
                self.sessions.borrow_mut().insert(session, Session { jobs });
                Ok(features)
            }
            Err(e) => Err(e),
        };
        answer(&self.link, id, Reply::AreaOpened(reply), &[]);
    }

    /// Событие команды сессии — хосту (`FromPlugin::AreaEvent`): тот же договор `sync::Event`, что у почтового пути.
    fn notify(&self, session: u64) -> impl Fn(crate::sync::Event) {
        let link = Arc::clone(&self.link);
        move |e: crate::sync::Event| {
            if let Err(e) = link.send(&FromPlugin::AreaEvent { session, event: e }, &[]) {
                tracing::debug!("plugin area event not sent: {e}");
            }
        }
    }

    /// Операция — в очередь сессии; выполнит задача-владелец ([`ServedAreas::area_worker`]) по одной.
    fn area_send(&self, session: u64, id: u64, job: Job) {
        let miss = job.miss();
        let sent = self.sessions.borrow().get(&session).is_some_and(|s| s.jobs.send(job).is_ok());
        if !sent {
            answer(&self.link, id, miss, &[]);
        }
    }

    /// Владелец сессии области: операции из очереди по одной — заём на время операции не пересекается с соседним
    /// (команды одной сессии ядро ведёт по одной, как в локальном раннере), повторный вход в `RefCell` невозможен.
    async fn area_worker(
        self: Rc<Self>,
        session: u64,
        mut area: Box<dyn AreaSession>,
        profile: Option<String>,
        owner: Option<Owner>,
        mut rx: UnboundedReceiver<Job>,
    ) {
        let notify = self.notify(session);
        while let Some(job) = rx.recv().await {
            let ctx = AreaCtx { cache: &self.cache, profile: profile.as_deref(), owner: owner.as_ref(), notify: &notify };
            match job {
                Job::Command { id, cmd } => {
                    // Задачи поверх кэша — услуга ядра: кэш у ядра, хранилище плагина спрашивают операциями ([`Ask::AreaTask`]).
                    if matches!(cmd, crate::sync::Command::Tasks(_)) {
                        answer(&self.link, id, Reply::AreaCommand(Ok(false)), &[]);
                        continue;
                    }
                    let done = area.handle(cmd, ctx).await.is_none();
                    answer(&self.link, id, Reply::AreaCommand(Ok(done)), &[]);
                }
                Job::Respond { id, respond } => {
                    let (invite, status, a) = *respond;
                    let reply = area.respond(&invite, status, &a).await;
                    answer(&self.link, id, Reply::AreaResponded(reply), &[]);
                }
                Job::Changed { id } => {
                    let reply = area.changed().await;
                    answer(&self.link, id, Reply::AreaChanged(reply), &[]);
                }
                Job::TaskOp { id, op } => {
                    let reply = area.task_op(op).await;
                    answer(&self.link, id, Reply::AreaTask(reply), &[]);
                }
            }
        }
    }

    /// Запустить синхронизацию учётки ([`Ask::MailOpen`], PLG-08): услуги ядра — по связи, события —
    /// `FromPlugin::MailEvent`, команды — в канал потока ([`ServedAreas::mails`]).
    async fn open_mail(self: Rc<Self>, id: u64, session: u64, url: String, profile: Option<String>, login: MailLogin) {
        let link = Arc::clone(&self.link);
        let services = MailServices {
            store: Box::new(RemoteCache::new(Arc::clone(&link), session)),
            worker: Box::new(RemoteWorker::new(Arc::clone(&link))),
            seal: Box::new(WireSeal::new(Arc::clone(&link))),
            index: Box::new(RemoteIndex::new(Arc::clone(&link), session)),
        };
        let notify: Arc<dyn Fn(SyncEvent) + Send + Sync> = {
            let link = Arc::clone(&link);
            Arc::new(move |event: SyncEvent| {
                if let Err(e) = link.send(&FromPlugin::MailEvent { session, event }, &[]) {
                    tracing::debug!("plugin mail event not sent: {e}");
                }
            })
        };
        let (tx, rx) = unbounded_channel();
        let reply = match &self.mail {
            Some(mail) => mail.open(url, profile, login, services, rx, notify).await,
            None => Err("plugin does not serve mail".into()),
        };
        if reply.is_ok() {
            self.mails.borrow_mut().insert(session, tx);
        }
        answer(&self.link, id, Reply::MailOpened(reply), &[]);
    }

    async fn probe(self: Rc<Self>, id: u64, url: String) {
        let found = self.server.probe(&url).await;
        answer(&self.link, id, Reply::Url(found), &[]);
    }

    async fn fetch(self: Rc<Self>, id: u64, url: String, max: u32, etag: Option<String>) {
        let max = max.min(MAX_BIN as u32);
        let got = self.server.fetch(&url, max, etag.as_deref()).await;
        match got {
            Ok(Some(body)) => answer(&self.link, id, Reply::Fetched(Ok(Fetched::Body { etag: body.etag })), &body.body),
            Ok(None) => answer(&self.link, id, Reply::Fetched(Ok(Fetched::NotModified)), &[]),
            Err(e) => answer(&self.link, id, Reply::Fetched(Err(e)), &[]),
        }
    }
}

/// События сессии — хосту по порядку; перед ответом на вызов — всё, что сессия успела послать.
struct Events {
    link: Arc<PluginLink>,
    session: u64,
    rx: RefCell<UnboundedReceiver<ChannelEvent>>,
}

impl Events {
    fn send(&self, event: ChannelEvent) {
        if let Err(e) = self.link.send(&FromPlugin::Event { session: self.session, event }, &[]) {
            tracing::debug!("plugin event not sent: {e}");
        }
    }

    fn flush(&self) {
        loop {
            let next = self.rx.borrow_mut().try_recv();
            match next {
                Ok(ev) => self.send(ev),
                Err(_) => return,
            }
        }
    }

    async fn pump(self: Rc<Self>) {
        while let Some(ev) = std::future::poll_fn(|cx| self.rx.borrow_mut().poll_recv(cx)).await {
            self.send(ev);
        }
    }
}

/// Хранилище плагина — услуга ядра по обмену: вызов связи, ответ — `ToPlugin::Stored`.
struct RemoteStore {
    link: Arc<PluginLink>,
    scope: u64,
}

impl RemoteStore {
    fn call(&self, op: StoreOp) -> Result<Option<Vec<u8>>, ChannelError> {
        match self.link.call(&FromPlugin::Store { scope: self.scope, op }, &[], STORE_WAIT) {
            Ok((ToPlugin::Stored(r), _)) => r,
            Ok(_) => Err(ChannelError::Storage("unexpected store reply".into())),
            Err(e) => Err(ChannelError::Storage(format!("store: {e}"))),
        }
    }
}

impl PluginStore for RemoteStore {
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, ChannelError> {
        self.call(StoreOp::Get(key.to_owned()))
    }

    fn put(&self, key: &str, value: &[u8]) -> Result<(), ChannelError> {
        self.call(StoreOp::Put(key.to_owned(), value.to_vec())).map(drop)
    }

    fn remove_prefix(&self, prefix: &str) -> Result<(), ChannelError> {
        self.call(StoreOp::RemovePrefix(prefix.to_owned())).map(drop)
    }
}

/// Возврат SSO из браузера — услуга ядра по обмену (ADR 0013): листенер loopback у хоста. Ожидание браузера — до
/// получаса, вызов связи не занимает рантайм плагина: блокирующий вызов — в потоке пула.
struct RemoteSso {
    link: Arc<PluginLink>,
    flow: u64,
}

impl Sso for RemoteSso {
    fn port(&self) -> Result<u16, ChannelError> {
        match self.link.call(&FromPlugin::Sso { flow: self.flow, op: SsoOp::Listen }, &[], STORE_WAIT) {
            Ok((ToPlugin::SsoReply(Ok(SsoDone::Port(port))), _)) => Ok(port),
            Ok((ToPlugin::SsoReply(Err(e)), _)) => Err(e),
            Ok(_) => Err(ChannelError::Protocol("unexpected sso reply".into())),
            Err(e) => Err(ChannelError::Storage(format!("sso: {e}"))),
        }
    }

    fn token<'a>(&'a self) -> BoxFuture<'a, Result<String, ChannelError>> {
        let link = Arc::clone(&self.link);
        let flow = self.flow;
        Box::pin(async move {
            let asked = tokio::task::spawn_blocking(move || link.call(&FromPlugin::Sso { flow, op: SsoOp::Wait }, &[], SSO_WAIT));
            match asked.await.map_err(|e| ChannelError::Plugin(format!("sso wait: {e}")))? {
                Ok((ToPlugin::SsoReply(Ok(SsoDone::Token(token))), _)) => Ok(token),
                Ok((ToPlugin::SsoReply(Err(e)), _)) => Err(e),
                Ok(_) => Err(ChannelError::Protocol("unexpected sso reply".into())),
                Err(e) => Err(ChannelError::Storage(format!("sso: {e}"))),
            }
        })
    }
}

struct Sink(Vec<u8>);

impl ByteSink for Sink {
    fn write(&mut self, chunk: &[u8]) -> Result<(), ChannelError> {
        if self.0.len() + chunk.len() > MAX_BIN {
            return Err(ChannelError::TooLarge { max: MAX_BIN as u64 });
        }
        self.0.extend_from_slice(chunk);
        Ok(())
    }
}

struct Source(Vec<u8>, usize);

impl ByteSource for Source {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, ChannelError> {
        let rest = self.0.get(self.1..).unwrap_or_default();
        let n = rest.len().min(buf.len());
        buf[..n].copy_from_slice(&rest[..n]);
        self.1 += n;
        Ok(n)
    }

    fn len(&self) -> u64 {
        self.0.len() as u64
    }
}

/// Шаги входа или потока вопросов: номер вопроса хоста и ответ пользователя.
type Steps = UnboundedSender<(u64, Option<Answer>)>;

/// Открытая сессия и её события.
type Open = (Rc<dyn ChannelSession>, Rc<Events>);

struct Served {
    link: Arc<PluginLink>,
    plugin: Arc<dyn ChannelPlugin>,
    sessions: RefCell<HashMap<u64, Open>>,
    flows: RefCell<HashMap<u64, Steps>>,
}

impl Served {
    async fn run(self: Rc<Self>, mut rx: UnboundedReceiver<(ToPlugin, Vec<u8>)>) {
        while let Some((msg, bin)) = rx.recv().await {
            match msg {
                ToPlugin::Ask { id, ask } => self.ask(id, ask, bin),
                ToPlugin::Close { session } => {
                    self.sessions.borrow_mut().remove(&session);
                }
                ToPlugin::CloseFlow { flow } => {
                    self.flows.borrow_mut().remove(&flow);
                }
                ToPlugin::Settings(s) => apply(s),
                // Каналы исходящего не держат: очередь отправки — у ядра. `Cached`/`Worked`/`Sealed`/`SsoReply` —
                // ответы услуг ядра, сюда сообщением не приходят.
                ToPlugin::Leave | ToPlugin::Stored(_) | ToPlugin::SsoReply(_) | ToPlugin::Cached(_) | ToPlugin::Worked(_) | ToPlugin::Sealed(_) => {}
            }
        }
    }

    fn ask(self: &Rc<Self>, id: u64, ask: Ask, bin: Vec<u8>) {
        match ask {
            Ask::Open { session, account, secret } => {
                spawn_quiet(&self.link, id, Rc::clone(self).open(id, session, account, secret));
            }
            Ask::SignIn { flow } => {
                spawn_quiet(&self.link, id, Rc::clone(self).sign_in(id, flow));
            }
            Ask::Step { flow, answer: a } => {
                let sent = self.flows.borrow().get(&flow).is_some_and(|tx| tx.send((id, a)).is_ok());
                if !sent {
                    answer(&self.link, id, Reply::Prompt(Err(ChannelError::NotFound)), &[]);
                }
            }
            Ask::Session { session, call } => {
                spawn_quiet(&self.link, id, Rc::clone(self).call(id, session, call, bin));
            }
            // Вопросы описания отвечает поток чтения; сюда они не доходят.
            other => {
                let reply = match other {
                    Ask::Features => Reply::Features(self.plugin.features()),
                    _ => Reply::Unit(Err(ChannelError::Unsupported)),
                };
                answer(&self.link, id, reply, &[]);
            }
        }
    }

    async fn open(self: Rc<Self>, id: u64, session: u64, account: AccountId, secret: Secret) {
        let (tx, rx) = unbounded_channel();
        let events = Rc::new(Events { link: Arc::clone(&self.link), session, rx: RefCell::new(rx) });
        tokio::task::spawn_local(Rc::clone(&events).pump());
        let store: Rc<dyn PluginStore> = Rc::new(RemoteStore { link: Arc::clone(&self.link), scope: session });
        let reply = match self.plugin.connect(ConnectCtx { account, secret, incoming: tx, store }).await {
            Ok(s) => {
                let s: Rc<dyn ChannelSession> = Rc::from(s);
                let opened = Opened { me: s.me(), features: s.features() };
                self.sessions.borrow_mut().insert(session, (s, Rc::clone(&events)));
                Ok(opened)
            }
            Err(e) => Err(e),
        };
        events.flush();
        answer(&self.link, id, Reply::Opened(reply), &[]);
    }

    /// Шаги потока: ответы пользователя по одному, пока хост не закрыл поток.
    async fn steps(&self, flow: u64, f: &mut dyn crate::channel::Flow) {
        let (tx, mut steps) = unbounded_channel();
        self.flows.borrow_mut().insert(flow, tx);
        while let Some((n, a)) = steps.recv().await {
            let r = f.step(a).await;
            answer(&self.link, n, Reply::Prompt(r), &[]);
        }
    }

    async fn sign_in(self: Rc<Self>, id: u64, flow: u64) {
        let store: Rc<dyn PluginStore> = Rc::new(RemoteStore { link: Arc::clone(&self.link), scope: flow });
        let sso: Rc<dyn Sso> = Rc::new(RemoteSso { link: Arc::clone(&self.link), flow });
        let plugin = Arc::clone(&self.plugin);
        match plugin.sign_in(FlowCtx { store, sso: Some(sso) }).await {
            Ok(mut f) => {
                answer(&self.link, id, Reply::Unit(Ok(())), &[]);
                self.steps(flow, &mut *f).await;
            }
            Err(e) => answer(&self.link, id, Reply::Unit(Err(e)), &[]),
        }
    }

    async fn call(self: Rc<Self>, id: u64, session: u64, call: Call, bin: Vec<u8>) {
        let Some((s, events)) = self.sessions.borrow().get(&session).cloned() else {
            return answer(&self.link, id, refused(&call, ChannelError::Network("no such session".into())), &[]);
        };
        let mut out = Vec::new();
        let reply = match call {
            Call::CatchUp(cursor) => Reply::Unit(s.catch_up(cursor).await),
            Call::History { conversation, before, limit } => Reply::Page(s.history(&conversation, before.as_ref(), limit).await),
            Call::Search { query, conversation, limit } => Reply::Page(s.search(&query, conversation.as_ref(), limit).await),
            Call::Act(action) => Reply::Done(s.act(action).await),
            Call::Download(src) => {
                let mut sink = Sink(Vec::new());
                let r = s.download(&src, &mut sink).await;
                out = sink.0;
                Reply::Unit(r)
            }
            Call::Upload(meta) => Reply::Uploaded(s.upload(&meta, &mut Source(bin, 0)).await),
            Call::Flow { flow, kind } => {
                match s.flow(kind).await {
                    Ok(mut f) => {
                        answer(&self.link, id, Reply::Unit(Ok(())), &[]);
                        self.steps(flow, &mut *f).await;
                    }
                    Err(e) => answer(&self.link, id, Reply::Unit(Err(e)), &[]),
                }
                return;
            }
            Call::SignOut => Reply::Unit(s.sign_out().await),
        };
        events.flush();
        answer(&self.link, id, reply, &out);
    }
}

/// Отказ на вызов сессии ответом нужного вида.
fn refused(call: &Call, e: ChannelError) -> Reply {
    match call {
        Call::History { .. } | Call::Search { .. } => Reply::Page(Err(e)),
        Call::Act(_) => Reply::Done(Err(e)),
        Call::Upload(_) => Reply::Uploaded(Err(e)),
        Call::CatchUp(_) | Call::Download(_) | Call::Flow { .. } | Call::SignOut => Reply::Unit(Err(e)),
    }
}

/// Плагин набора для запуска процессом: то, что нужно файлу набора (`postman-plugins`, `postman-plugins-matrix`, ADR
/// 0013) без интерфейса. Строки тех же плагинов в композиционном корне (`app-gpui/src/plugins.rs`) сверяет с мнением
/// самого плагина тест корня.
#[derive(Clone, Copy)]
pub struct SetEntry {
    pub id: &'static str,
    pub kind: Kind,
    /// Профиль песочницы процесса; вшит в набор, аргументом его не понизить.
    pub profile: Profile,
    /// Как плагин узнаёт свой адрес — тот же [`Claim`], что в строке корня: тест сверяет мнения.
    pub claim: Claim,
    /// Точка входа процесса: связь с хостом уже открыта, песочница заперта. Ответ — код выхода.
    pub serve: fn(PluginLink) -> i32,
}

impl SetEntry {
    /// Адрес — этого плагина по мнению самого набора; то же правило, что у строки корня ([`Row::claims`]).
    pub fn claims(&self, url: &str) -> bool {
        Row { id: self.id, kind: self.kind, claim: self.claim, set: "", profile: self.profile }.claims(url)
    }
}

/// Общий `main` файла набора плагинов: `postman-plugins* --plugin=<id>` (ADR 0013). До первого сообщения — только
/// запирание песочницы (профиль — из набора) и ответ на `Hello`; рантайм и сам плагин — при первом настоящем вопросе
/// хоста. Своих файлов процесс не пишет: состояние плагина — услуга ядра по обмену. Журнал процесса — строками с
/// уровнем в stderr (настройку делает `log`, уровень приходит в `Hello` и сменами). Ответ — код выхода.
pub fn set_main(set: &'static str, find: fn(&str) -> Option<&'static SetEntry>, log: fn()) -> i32 {
    let id = std::env::args().find_map(|a| a.strip_prefix("--plugin=").map(str::to_owned));
    let Some(entry) = id.as_deref().and_then(find) else {
        eprintln!("ERROR {set}: unknown plugin {id:?}");
        return 2;
    };
    #[cfg(target_os = "linux")]
    if let Ok(f) = std::fs::File::open("/proc/self/status") {
        static STATUS: OnceLock<std::fs::File> = OnceLock::new();
        /// `/proc/self/status`, открытый до запирания: после него Landlock файлов не откроет, а читать открытый можно.
        fn rss() -> u64 {
            use std::os::unix::fs::FileExt as _;
            let Some(f) = STATUS.get() else { return 0 };
            let mut buf = [0u8; 4096];
            let n = f.read_at(&mut buf, 0).unwrap_or(0);
            let status = String::from_utf8_lossy(&buf[..n]);
            status.lines().find_map(|l| l.strip_prefix("VmRSS:")?.trim().strip_suffix("kB")?.trim().parse::<u64>().ok()).map_or(0, |kb| kb * 1024)
        }
        let _ = STATUS.set(f);
        on_stats(rss);
    }
    if let Err(e) = postman_proc::sandbox::isolate(entry.profile).and_then(|()| postman_proc::sandbox::lockdown(entry.profile)) {
        eprintln!("ERROR {set}: {}{e}", postman_proc::SANDBOX_REFUSAL);
        return postman_proc::EXIT_SANDBOX;
    }
    // Сторож смерти хоста и связь — после запирания: запреты Linux действуют на потоки, созданные после него.
    let _watchdog = postman_proc::watchdog(None);
    log();
    let link = match PluginLink::stdio(MAX_BIN) {
        Ok(link) => link,
        Err(e) => {
            eprintln!("ERROR {set}: link: {e}");
            return 1;
        }
    };
    (entry.serve)(link)
}
