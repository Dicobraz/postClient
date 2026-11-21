//! Хост плагинов и заместитель плагина (ADR 0013): плагин работает отдельным процессом, в реестре вместо него стоит
//! заместитель с тем же трейтом ([`RemoteChannel`] — `ChannelPlugin`). Процессы, кадры, песочница и сторож — только
//! `postman-proc`; сообщения — [`super::wire`].
//!
//! - **Строка плагина** ([`Row`]): идентификатор, вид, как узнать свой адрес, файл набора и профиль песочницы — без
//!   запуска процесса. Строки собирает композиционный корень (`app-gpui/src/plugins.rs`).
//! - **Запуск после первого кадра** (ADR 0013 «Запуск»): заместитель отдаёт канал сразу; процесс создаёт фоновый поток
//!   плагина, и только после [`Host::first_frame`]. Процесс нужен, когда заместителя позвали (`connect`, `sign_in`):
//!   есть учётка вида или идёт вход. Первым — плагин с приоритетом [`Priority::First`] (активная учётка, исходящее),
//!   не больше [`MAX_STARTS`] стартов сразу; остальные — с пониженным приоритетом процессора и ввода-вывода до первой
//!   синхронизации: у каналов — первый курсор (`ChannelEvent::Cursor`), у почты — первый пакет событий синка
//!   ([`mail_sync_packet`]), у областей — первое событие команды. Пока плагин поднимается, `connect` ждёт, поток
//!   учётки — «подключение», запросы интерфейса копятся в его канале.
//! - **Договор:** `Hello` со сверкой сборки ([`Build`]), уровнем журнала, политикой и сетью; секрет учётки — в `Open`;
//!   хранилище плагина — услуга ядра по обмену (`PluginStore` учётки или входа, вызовом связи). Журнал процесса — в
//!   `tracing` с меткой плагина и его уровнем.
//! - **Падение и зависание:** процесс завершился — сессии получают «без связи» (поток учётки повторит), хост
//!   перезапускает с паузами [`PAUSES`]; [`CRASHES`] падений за [`CRASH_WINDOW`] — «проблема»
//!   (`ChannelError::Plugin`) до «Повторить» (`Request::Reconnect`). Нет `Pong` за [`PONG_WAIT`] — процесс убит и
//!   перезапущен. Сборки не совпали или файла набора нет (компонент не установлен, PLG-14 —
//!   `ChannelError::Missing`) — «проблема» сразу, без перезапусков.
//!
//! Сторона плагина — [`serve_channel`].

mod area;
mod serve;
mod sso;

pub use area::RemoteAreas;
pub(in crate::backend) use area::{PROBE_WAIT, ready_async};
pub(in crate::backend) use serve::STORE_WAIT;
pub use serve::{
    AreaServer, AreaSession, FetchedBody, SetEntry, on_log, on_stats, serve_area, serve_channel, serve_directory, serve_mail_area, set_main, settings,
};
pub(crate) use sso::BROWSER_WAIT;

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, mpsc};
use std::time::{Duration, Instant};

use postman_cache::Cache;
use postman_proc::journal::{Journal, JournalLevel};
use postman_proc::link::{CallId, Incoming, Link};
pub use postman_proc::sandbox::{Namespaces, Profile};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tokio::sync::{oneshot, watch};

use super::BoxFuture;
use super::mail::{run_cache, run_smime, run_worker};
use super::wire::{
    Ask, Build, Call, DirectoryQuery, FromPlugin, HostHello, Kind, Opened, PluginHello, Reply, Settings, SsoDone, SsoOp, StoreOp, ToPlugin,
};
use crate::channel::{
    AccountId, Address, Answer, AttachmentSource, ByteSink, ByteSource, ChannelError, ChannelEvent, ChannelFeatures, ChannelPlugin, ChannelSession,
    ChannelState, ConnectCtx, ConversationId, Cursor, Done, FileMeta, Flow, FlowCtx, FlowKind, MessageId, Page, Participant, PluginStore, Prompt,
    Uploaded,
};
use crate::smime::directory::{Directory as SmimeDirectory, DirectoryError};

/// Связь хоста с процессом плагина.
pub type HostLink = Link<ToPlugin, FromPlugin>;
/// Связь процесса плагина с хостом.
pub type PluginLink = Link<FromPlugin, ToPlugin>;

/// Предел двоичной части кадра (файл выгрузки и загрузки) в обе стороны.
pub const MAX_BIN: usize = postman_proc::frame::MAX_FRAME;
/// Сколько процессов плагинов стартует одновременно.
pub const MAX_STARTS: usize = 2;
/// Паузы перед перезапуском упавшего процесса: первое падение, второе, третье, дальше.
pub const PAUSES: [Duration; 4] = [Duration::from_secs(1), Duration::from_secs(5), Duration::from_secs(30), Duration::from_secs(120)];
/// Столько падений за [`CRASH_WINDOW`] — «проблема» до «Повторить».
pub const CRASHES: usize = 5;
pub const CRASH_WINDOW: Duration = Duration::from_secs(600);
/// Нет `Pong` (и ответа на `Hello`) столько — процесс убит.
pub const PONG_WAIT: Duration = Duration::from_secs(30);
/// Как часто хост проверяет, жив ли процесс, и спрашивает его память.
pub const PING_EVERY: Duration = Duration::from_secs(10);
/// Синхронные вопросы описания (`features`, `parse_link`…) ждут ответа не дольше; процесс не поднят — не ждут вовсе.
const SYNC_WAIT: Duration = Duration::from_secs(2);
/// Процесс каталога завершают после стольких простоя (PLG-06, ADR 0013 «по запросу»): начальный порог — 5 минут,
/// уточнит PLG-12. Следующий запрос поднимет процесс снова.
pub const IDLE_EXIT: Duration = Duration::from_secs(300);
/// Запрос каталога ждёт ответа не дольше: поиск в плагине ограничен [`TIMEOUT`] (5 с) — обмен с запасом на него.
const DIRECTORY_WAIT: Duration = Duration::from_secs(6);
/// Пауза между опросами фазы процесса каталога ([`RemoteDirectory::ready`]) — ждёт отдельный поток каталога.
const PHASE_POLL: Duration = Duration::from_millis(5);

/// Как плагин узнаёт свой адрес — без запуска процесса.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Claim {
    /// Канал: адреса `postman://<id>/…`.
    Channel,
    /// Своя схема адреса учётки: `imaps://`.
    Scheme(&'static str),
    /// Путь в адресе: `/ews/exchange.asmx`.
    Path(&'static str),
    /// Ключ области в адресе учётки: `dav` → `?dav=https://…`.
    Area(&'static str),
    /// Своего адреса нет (каталог, PLG-06): поиск по настройкам «Безопасность», не по адресу учётки.
    None,
}

/// Строка плагина в композиционном корне: всё, что о нём нужно знать без процесса.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Row {
    pub id: &'static str,
    pub kind: Kind,
    pub claim: Claim,
    /// Файл набора без расширения: `postman-plugins`.
    pub set: &'static str,
    /// Профиль песочницы процесса; тот же вшит в набор (`postman-plugins`), аргументом его не понизить.
    pub profile: Profile,
}

impl Row {
    /// Адрес — этого плагина (регистр не важен).
    pub fn claims(&self, url: &str) -> bool {
        let url = url.to_ascii_lowercase();
        match self.claim {
            Claim::Channel => url.starts_with(&format!("{}{}/", crate::channel::SCHEME, self.id)),
            Claim::Scheme(s) => url.starts_with(&s.to_ascii_lowercase()),
            // Путь в конце адреса (до запроса и фрагмента), как у `is_ews_url` пути EWS (PLG-08) — иначе путь
            // совпал бы и в середине чужого адреса.
            Claim::Path(p) => url.split(['?', '#']).next().is_some_and(|u| u.trim_end_matches('/').ends_with(&p.to_ascii_lowercase())),
            Claim::Area(key) => url.split_once('?').is_some_and(|(_, q)| q.split('&').any(|kv| kv.split_once('=').is_some_and(|(k, _)| k == key))),
            Claim::None => false,
        }
    }

    /// Файл набора в каталоге `dir`.
    pub fn exe(&self, dir: &Path) -> PathBuf {
        dir.join(format!("{}{}", self.set, std::env::consts::EXE_SUFFIX))
    }
}

/// Запущенный процесс плагина, как его видит хост.
pub struct Started {
    pub link: HostLink,
    pub kill: Box<dyn FnMut() + Send>,
    pub exit_code: Box<dyn FnMut() -> Option<u32> + Send>,
    /// Пониженный приоритет на время прогрева (`true`) и его возврат.
    pub background: Box<dyn FnMut(bool) -> io::Result<()> + Send>,
}

/// Как запустить процесс плагина: [`ProcLauncher`]; в тестах — подставной.
pub trait Launcher: Send + Sync {
    fn launch(&self, row: &Row) -> io::Result<Started>;
}

/// Запуск набора в песочнице (`postman_proc::sandbox`): `<dir>/<set> --plugin=<id>`, профиль — из строки.
pub struct ProcLauncher {
    /// Каталог файлов набора (рядом с `postman.exe`).
    pub dir: PathBuf,
    pub namespaces: Namespaces,
}

impl Launcher for ProcLauncher {
    fn launch(&self, row: &Row) -> io::Result<Started> {
        use postman_proc::sandbox::{Launch, Level, MEMORY_LIMIT, spawn};
        let exe = row.exe(&self.dir);
        let arg = format!("--plugin={}", row.id);
        let args = [arg.as_str()];
        let mut p = spawn(&Launch { exe: &exe, args: &args, memory: MEMORY_LIMIT, namespaces: self.namespaces, profile: row.profile })?;
        if p.level != Level::Full {
            tracing::warn!(plugin = row.id, "plugin sandbox is partial: {}", p.fallback.as_deref().unwrap_or("unknown"));
        }
        let stderr = std::mem::replace(&mut p.stderr, Box::new(io::empty()));
        let linked = postman_proc::journal::drain_stderr(stderr, journal(row.id), Arc::new(AtomicBool::new(false)))
            .and_then(|()| Link::new(p.stdout, p.stdin, MAX_BIN));
        match linked {
            Ok(link) => Ok(Started { link, kill: p.kill, exit_code: p.exit_code, background: p.background }),
            Err(e) => {
                (p.kill)();
                Err(e)
            }
        }
    }
}

/// Строки stderr процесса — в журнал приложения с меткой плагина и уровнем строки (`WARN …`, ` INFO …`); паника и
/// отказ в памяти — ERROR.
fn journal(id: &'static str) -> Journal {
    Arc::new(move |line: &str| {
        let trimmed = line.trim_start();
        let (level, text) = trimmed.split_once(' ').unwrap_or((trimmed, ""));
        match level {
            "ERROR" => tracing::error!(target: "plugin", plugin = id, "{text}"),
            "WARN" => tracing::warn!(target: "plugin", plugin = id, "{text}"),
            "INFO" => tracing::info!(target: "plugin", plugin = id, "{text}"),
            "DEBUG" => tracing::debug!(target: "plugin", plugin = id, "{text}"),
            "TRACE" => tracing::trace!(target: "plugin", plugin = id, "{text}"),
            _ if JournalLevel::of(line) == JournalLevel::Error => tracing::error!(target: "plugin", plugin = id, "{line}"),
            _ => tracing::warn!(target: "plugin", plugin = id, "{line}"),
        }
    })
}

/// Очередь старта: первыми — [`Priority::First`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum Priority {
    /// Плагин активной учётки и учёток с исходящим: стартует первым, с обычным приоритетом процесса.
    First,
    /// Остальные: следом, с пониженным приоритетом процесса до первой синхронизации.
    #[default]
    Rest,
}

/// Замеры хоста для `--bench`: создание процесса → ответ на `Hello` у первого плагина, память запущенных процессов.
#[derive(Debug, Clone, Default)]
struct Metrics {
    start: Option<Duration>,
    rss: BTreeMap<&'static str, u64>,
}

static METRICS: Mutex<Metrics> = Mutex::new(Metrics { start: None, rss: BTreeMap::new() });

fn metrics_lock() -> MutexGuard<'static, Metrics> {
    METRICS.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `plugin_start_ms`: создание первого процесса плагина → его `Hello`; `None` — плагинов не запускали.
pub fn start_ms() -> Option<f32> {
    metrics_lock().start.map(|d| d.as_secs_f32() * 1000.0)
}

/// `plugins_rss_mb`: рабочий набор запущенных процессов плагинов (по последнему опросу), байт.
pub fn plugins_rss() -> u64 {
    metrics_lock().rss.values().sum()
}

pub(in crate::backend) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Сессия почтового пути на хосте (PLG-08): события учётки — в её канал, база — у ядра, ошибка записи без ответа —
/// до ближайшего чтения.
pub(in crate::backend) struct MailScope {
    pub(in crate::backend) events: mpsc::Sender<crate::sync::Event>,
    pub(in crate::backend) repaint: Mutex<Box<dyn Fn() + Send>>,
    pub(in crate::backend) cache: Cache,
    /// Поисковый индекс учётки (PLG-10): услуга `FromPlugin::Index` индексирует строки им.
    pub(in crate::backend) index: crate::sync::Index,
    pub(in crate::backend) failed: Mutex<Option<String>>,
    /// Возможности пути (`Event::Features`, AUD-02): по ним ядро решает, что держать у себя (например, «Отправить
    /// позже» — только у путей без `Feature::ServerSchedule`). До первого события — [`crate::model::Features::NONE`].
    pub(in crate::backend) features: Mutex<crate::model::Features>,
    /// Очередь отправки учётки (PLG-08): ответы пути на её билеты — сюда, интерфейсу — только её ошибки.
    pub(in crate::backend) queue: Arc<super::mail::MailQueue>,
}

/// Пакет событий синхронизации почтового пути (AUD-04): папки или письма дошли до ядра — либо сверка кончилась.
/// Статусы входа («подключение», «вошли», «загрузка») пакетом не считаются: по ним возвращать приоритет рано.
fn mail_sync_packet(event: &crate::sync::Event) -> bool {
    matches!(
        event,
        crate::sync::Event::MailboxesChanged
            | crate::sync::Event::EmailsChanged
            | crate::sync::Event::BodiesStored(_)
            | crate::sync::Event::Status(crate::sync::Status::Synced)
    )
}

fn gone() -> ChannelError {
    ChannelError::Network("plugin process is not running".into())
}

fn unexpected() -> ChannelError {
    ChannelError::Protocol("plugin: unexpected reply".into())
}

/// Хост плагинов процесса: один на программу. Клонируется дёшево.
#[derive(Clone)]
pub struct Host(pub(in crate::backend) Arc<Inner>);

pub(in crate::backend) struct Inner {
    launcher: Arc<dyn Launcher>,
    settings: Mutex<Settings>,
    gate: Mutex<Gate>,
    opened: Condvar,
    launches: AtomicUsize,
    plugins: Mutex<HashMap<&'static str, Arc<Plugin>>>,
    /// Номер «проблемы»: поток учётки отличает новую от той, что уже показал.
    serial: AtomicU64,
}

/// Ворота стартов: первый кадр показан, сколько стартует сейчас, кто ждёт.
#[derive(Default)]
struct Gate {
    shown: bool,
    starting: usize,
    queue: Vec<(Priority, u64, &'static str)>,
    next: u64,
}

impl Host {
    /// Хост без процессов: процессы появятся после [`Host::first_frame`] и только по запросу заместителей.
    pub fn new(launcher: Arc<dyn Launcher>, settings: Settings) -> Self {
        Self(Arc::new(Inner {
            launcher,
            settings: Mutex::new(settings),
            gate: Mutex::new(Gate::default()),
            opened: Condvar::new(),
            launches: AtomicUsize::new(0),
            plugins: Mutex::new(HashMap::new()),
            serial: AtomicU64::new(0),
        }))
    }

    /// Первый кадр показан (при запуске в трей — база открыта): процессы можно создавать. Без ввода-вывода.
    pub fn first_frame(&self) {
        lock(&self.0.gate).shown = true;
        self.0.opened.notify_all();
    }

    /// Сколько процессов создано (запусков).
    pub fn launches(&self) -> usize {
        self.0.launches.load(Ordering::SeqCst)
    }

    /// Заместитель канала по строке (процесса нет, пока его не позовут).
    pub fn channel(&self, row: Row) -> RemoteChannel {
        RemoteChannel { host: Arc::clone(&self.0), plugin: self.0.plugin(row), reported: Mutex::new(HashMap::new()), features: Mutex::new(None) }
    }

    /// Заместитель каталога по строке и настройкам запроса (PLG-06): процесс — по первому запросу сертификатов,
    /// после [`IDLE_EXIT`] простоя завершается до следующего. Процесса нет, пока заместителя не позовут.
    pub fn directory(&self, row: Row, query: DirectoryQuery) -> Arc<dyn SmimeDirectory> {
        let plugin = self.0.plugin(row);
        *lock(&plugin.idle) = Some(IDLE_EXIT);
        Arc::new(RemoteDirectory { host: Arc::clone(&self.0), plugin, query })
    }

    /// Плагин `id` — первым в очереди старта и без понижения приоритета (активная учётка, исходящее).
    pub fn prefer(&self, id: &'static str) {
        if let Some(p) = lock(&self.0.plugins).get(id) {
            *lock(&p.priority) = Priority::First;
        }
        let mut gate = lock(&self.0.gate);
        for entry in gate.queue.iter_mut().filter(|e| e.2 == id) {
            entry.0 = Priority::First;
        }
        drop(gate);
        self.0.opened.notify_all();
    }

    /// Компонент `id` установлен (PLG-15): разбудить «проблему» («Повторить» без пользователя) — поток запуска
    /// поднимет процесс, как только плагин понадобится. Не поднятый плагин просто начнёт запуск по запросу.
    pub fn retry(&self, id: &'static str) {
        if let Some(p) = lock(&self.0.plugins).get(id) {
            p.retry();
        }
    }

    /// Новые настройки — запущенным процессам сейчас, остальным — в `Hello`.
    /// Текущие настройки политики у процессов ([`Settings`]): чтобы дописать свою и отослать
    /// [`set_settings`](Self::set_settings) целиком.
    pub fn settings(&self) -> Settings {
        lock(&self.0.settings).clone()
    }

    pub fn set_settings(&self, settings: Settings) {
        *lock(&self.0.settings) = settings.clone();
        for conn in self.0.running() {
            if let Err(e) = conn.link.send(&ToPlugin::Settings(settings.clone()), &[]) {
                tracing::debug!("plugin settings not sent: {e}");
            }
        }
    }

    /// Выход программы: плагины досылают то, что держат (каналы не держат ничего — очередь у ядра).
    pub fn leave(&self) {
        for conn in self.0.running() {
            let _ = conn.link.send(&ToPlugin::Leave, &[]);
        }
    }
}

impl Inner {
    pub(in crate::backend) fn plugin(&self, row: Row) -> Arc<Plugin> {
        let mut plugins = lock(&self.plugins);
        Arc::clone(plugins.entry(row.id).or_insert_with(|| {
            Arc::new(Plugin {
                row,
                phase: watch::channel(Phase::Idle).0,
                priority: Mutex::new(Priority::default()),
                key: Mutex::new(None),
                supervised: AtomicBool::new(false),
                retry: Mutex::new(false),
                retried: Condvar::new(),
                idle: Mutex::new(None),
                last_use: Mutex::new(None),
                areas_open: AtomicUsize::new(0),
            })
        }))
    }

    fn running(&self) -> Vec<Arc<Conn>> {
        lock(&self.plugins)
            .values()
            .filter_map(|p| match &*p.phase.borrow() {
                Phase::Ready(c) => Some(Arc::clone(c)),
                _ => None,
            })
            .collect()
    }

    /// Ждать очереди старта: первый кадр показан, стартов меньше [`MAX_STARTS`], этот плагин — первый в очереди.
    /// Место в очереди живёт и после возврата — до [`Inner::entered`]: запись запуска идёт после слота, и пока
    /// она не сделана, место держит порядок (следующий не займёт слот раньше — иначе порядок стартов гоняется).
    fn acquire(&self, plugin: &Plugin) -> u64 {
        let mut gate = lock(&self.gate);
        let seq = gate.next;
        gate.next += 1;
        let prio = *lock(&plugin.priority);
        gate.queue.push((prio, seq, plugin.row.id));
        loop {
            let first = gate.queue.iter().min().map(|e| e.1);
            if gate.shown && gate.starting < MAX_STARTS && first == Some(seq) {
                gate.starting += 1;
                return seq;
            }
            gate = self.opened.wait(gate).unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Запуск записан: место в очереди свободно — следующий может брать слот.
    fn entered(&self, seq: u64) {
        let mut gate = lock(&self.gate);
        gate.queue.retain(|e| e.1 != seq);
        drop(gate);
        self.opened.notify_all();
    }

    fn release(&self) {
        let mut gate = lock(&self.gate);
        gate.starting = gate.starting.saturating_sub(1);
        drop(gate);
        self.opened.notify_all();
    }
}

/// Что сейчас с процессом плагина.
#[derive(Clone)]
enum Phase {
    /// Никто не звал.
    Idle,
    /// В очереди старта или ждёт первого кадра.
    Waiting,
    Starting,
    Ready(Arc<Conn>),
    /// Упал; перезапуск после паузы.
    Down,
    /// Не работает до «Повторить».
    Problem {
        serial: u64,
        error: ChannelError,
    },
}

pub(in crate::backend) struct Plugin {
    pub(in crate::backend) row: Row,
    phase: watch::Sender<Phase>,
    priority: Mutex<Priority>,
    /// Ключ приложения в сборке плагина — из его `Hello` (Telegram, PLG-05); `None` — процесса ещё не было.
    key: Mutex<Option<bool>>,
    /// Фоновый поток плагина уже есть.
    supervised: AtomicBool,
    retry: Mutex<bool>,
    retried: Condvar,
    /// Предел простоя, после которого процесс завершают до следующего запроса: у каталога [`IDLE_EXIT`] (PLG-06),
    /// у каналов `None` — держат до выхода программы.
    idle: Mutex<Option<Duration>>,
    /// Последний запрос каталога — простоя считает сторож ([`Conn::watch`]).
    last_use: Mutex<Option<Instant>>,
    /// Открытые сессии областей учёток (PLG-07): пока есть — процесс держится, простоя нет.
    areas_open: AtomicUsize,
}

impl Plugin {
    fn set(&self, phase: Phase) {
        self.phase.send_replace(phase);
    }

    /// «Повторить»: прервать паузу или «проблему».
    fn retry(&self) {
        *lock(&self.retry) = true;
        self.retried.notify_all();
    }

    /// Пауза до перезапуска (`Some`) или «проблема» до «Повторить» (`None`).
    fn wait_retry(&self, pause: Option<Duration>) {
        let deadline = pause.map(|p| Instant::now() + p);
        let mut asked = lock(&self.retry);
        while !*asked {
            asked = match deadline {
                None => self.retried.wait(asked).unwrap_or_else(PoisonError::into_inner),
                Some(d) => {
                    let now = Instant::now();
                    if now >= d {
                        break;
                    }
                    self.retried.wait_timeout(asked, d - now).unwrap_or_else(PoisonError::into_inner).0
                }
            };
        }
        *asked = false;
    }
}

/// Фоновый поток плагина — при первом запросе заместителя (не в потоке интерфейса).
fn want(host: &Arc<Inner>, plugin: &Arc<Plugin>) {
    if plugin.supervised.swap(true, Ordering::SeqCst) {
        return;
    }
    plugin.set(Phase::Waiting);
    let (h, p) = (Arc::clone(host), Arc::clone(plugin));
    let spawned = std::thread::Builder::new().name(format!("plugin {}", plugin.row.id)).spawn(move || supervise(&h, p));
    if let Err(e) = spawned {
        tracing::error!(plugin = plugin.row.id, "plugin thread: {e}");
        plugin.supervised.store(false, Ordering::SeqCst);
        plugin.set(Phase::Problem { serial: host.serial.fetch_add(1, Ordering::SeqCst) + 1, error: ChannelError::Plugin(format!("thread: {e}")) });
    }
}

/// Почему процесс не поднялся.
enum Failed {
    /// Сборки не совпали или плагин не тот: перезапуск не поможет.
    Build(String),
    /// Файла набора нет (PLG-14): компонент не установлен — интерфейс покажет установку.
    Missing(String),
    Crash(String),
}

/// Чем кончился обмен с процессом.
enum Ended {
    /// Связь закрылась: падение или хост убил — перезапуск по паузам (каналы держат до выхода).
    Crashed(String),
    /// Простой вышел (каталог, [`IDLE_EXIT`], PLG-06): процесс завершён до следующего запроса — не падение.
    Idle,
}

/// Жизнь процесса плагина: очередь старта, запуск, `Hello`, обмен до конца связи, перезапуск с паузами.
fn supervise(host: &Inner, plugin: Arc<Plugin>) {
    let id = plugin.row.id;
    let mut crashes: VecDeque<Instant> = VecDeque::new();
    loop {
        plugin.set(Phase::Waiting);
        let seq = host.acquire(&plugin);
        plugin.set(Phase::Starting);
        host.launches.fetch_add(1, Ordering::SeqCst);
        let started = Instant::now();
        let launched = host.launcher.launch(&plugin.row);
        host.entered(seq);
        let outcome = match launched {
            Ok(s) => hello(host, &plugin, s, started),
            // Файла набора нет (не установлен, PLG-14): перезапуски не помогут, интерфейс предложит компонент.
            Err(e) if e.kind() == io::ErrorKind::NotFound => Err(Failed::Missing(plugin.row.set.to_owned())),
            Err(e) => Err(Failed::Crash(format!("launch: {e}"))),
        };
        host.release();
        let ended = match outcome {
            Ok(conn) => {
                plugin.set(Phase::Ready(Arc::clone(&conn)));
                tracing::info!(plugin = id, "plugin started in {} ms", started.elapsed().as_millis());
                conn.serve(&plugin)
            }
            Err(Failed::Build(why)) => {
                tracing::error!(plugin = id, "plugin not used: {why}; restart Postman");
                problem(host, &plugin, ChannelError::Plugin(why));
                crashes.clear();
                continue;
            }
            Err(Failed::Missing(set)) => {
                tracing::error!(plugin = id, "plugin set {set} is not installed");
                problem(host, &plugin, ChannelError::Missing(set));
                crashes.clear();
                continue;
            }
            Err(Failed::Crash(why)) => Ended::Crashed(why),
        };
        let Ended::Crashed(cause) = ended else {
            // Простой вышел: не падение, перезапуска нет; следующий запрос поднимет процесс заново ([`want`]).
            tracing::info!(plugin = id, "plugin idle for {} min, stopped until the next request", IDLE_EXIT.as_secs() / 60);
            plugin.supervised.store(false, Ordering::SeqCst);
            plugin.set(Phase::Idle);
            return;
        };
        tracing::warn!(plugin = id, "plugin process ended: {cause}");
        let now = Instant::now();
        crashes.push_back(now);
        while crashes.front().is_some_and(|t| now.duration_since(*t) > CRASH_WINDOW) {
            crashes.pop_front();
        }
        if crashes.len() >= CRASHES {
            tracing::error!(plugin = id, "plugin crashed {} times in {} min, waiting for retry: {cause}", crashes.len(), CRASH_WINDOW.as_secs() / 60);
            problem(host, &plugin, ChannelError::Plugin(format!("crashed {} times: {cause}", crashes.len())));
            crashes.clear();
            continue;
        }
        let pause = PAUSES[(crashes.len() - 1).min(PAUSES.len() - 1)];
        plugin.set(Phase::Down);
        plugin.wait_retry(Some(pause));
    }
}

fn problem(host: &Inner, plugin: &Plugin, error: ChannelError) {
    plugin.set(Phase::Problem { serial: host.serial.fetch_add(1, Ordering::SeqCst) + 1, error });
    plugin.wait_retry(None);
}

/// `Hello`: сверка сборки и того, что плагин — тот самый. Ответ ждём здесь же (обмен ещё не начат), не дольше
/// [`PONG_WAIT`].
fn hello(host: &Inner, plugin: &Arc<Plugin>, s: Started, started: Instant) -> Result<Arc<Conn>, Failed> {
    let Started { link, kill, exit_code, mut background } = s;
    let lowered = *lock(&plugin.priority) == Priority::Rest;
    if lowered && let Err(e) = background(true) {
        tracing::debug!(plugin = plugin.row.id, "plugin priority not lowered: {e}");
    }
    let conn = Arc::new(Conn {
        link,
        next: AtomicU64::new(1),
        pending: Mutex::new(HashMap::new()),
        sessions: Mutex::new(HashMap::new()),
        scopes: Mutex::new(HashMap::new()),
        sso: Mutex::new(HashMap::new()),
        areas_events: Mutex::new(HashMap::new()),
        mails: Mutex::new(HashMap::new()),
        worker: Mutex::new(None),
        kill: Mutex::new(kill),
        exit_code: Mutex::new(exit_code),
        background: Mutex::new(background),
        lowered: AtomicBool::new(lowered),
        idle_exit: AtomicBool::new(false),
    });
    let ask = ToPlugin::Ask { id: 0, ask: Ask::Hello(HostHello { build: Build::current(), settings: lock(&host.settings).clone() }) };
    if let Err(e) = conn.link.send(&ask, &[]) {
        conn.kill();
        return Err(Failed::Crash(format!("hello not sent: {e}{}", conn.exit())));
    }
    let deadline = Instant::now() + PONG_WAIT;
    let theirs: PluginHello = loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match conn.link.recv_timeout(left) {
            Some(Incoming::Message(FromPlugin::Answer { id: 0, reply: Reply::Hello(h) }, _)) => break h,
            Some(Incoming::Closed) => return Err(Failed::Crash(format!("closed before hello{}", conn.exit()))),
            Some(_) => {}
            None => {
                conn.kill();
                return Err(Failed::Crash(format!("no hello in {} s{}", PONG_WAIT.as_secs(), conn.exit())));
            }
        }
    };
    let ours = Build::current();
    if theirs.build != ours || theirs.id != plugin.row.id || theirs.kind != plugin.row.kind {
        conn.kill();
        return Err(Failed::Build(format!(
            "plugin {} {:?} {:?} does not match host {} {:?} {:?}",
            theirs.id, theirs.kind, theirs.build, plugin.row.id, plugin.row.kind, ours
        )));
    }
    // Наличие ключа приложения — факт сборки плагина (PLG-05): помним и между перезапусками процесса.
    *lock(&plugin.key) = Some(theirs.key);
    let mut m = metrics_lock();
    m.start.get_or_insert(started.elapsed());
    drop(m);
    let watched = Arc::clone(&conn);
    let id = plugin.row.id;
    let watched_plugin = Arc::clone(plugin);
    if let Err(e) = std::thread::Builder::new().name(format!("plugin {id} ping")).spawn(move || watched.watch(id, watched_plugin)) {
        tracing::warn!(plugin = id, "plugin ping thread: {e}");
    }
    Ok(conn)
}

/// Ответ плагина: сообщение и двоичная часть.
type Answered = (Reply, Vec<u8>);

/// Кто ждёт ответа: задача потока учётки или синхронный вопрос.
enum Waiter {
    Async(oneshot::Sender<Answered>),
    Sync(mpsc::Sender<Answered>),
}

/// Работающий процесс плагина.
pub(in crate::backend) struct Conn {
    pub(in crate::backend) link: HostLink,
    next: AtomicU64,
    pending: Mutex<HashMap<u64, Waiter>>,
    /// Сессия → куда её события (`ConnectCtx::incoming`).
    sessions: Mutex<HashMap<u64, crate::channel::Incoming>>,
    /// Сессия или вход → кто выполняет вызовы хранилища (задача в потоке учётки, где живёт её `PluginStore`).
    scopes: Mutex<HashMap<u64, UnboundedSender<(CallId, StoreOp)>>>,
    /// Вход → слушатель возврата SSO из браузера (ADR 0013: слушает ядро, не плагин).
    sso: Mutex<HashMap<u64, Arc<std::net::TcpListener>>>,
    /// Сессия области учётки (PLG-07) → события её команды (`FromPlugin::AreaEvent`): ждущий ответ задачи
    /// заберёт их после ответа ([`Conn::drain_area_events`]).
    areas_events: Mutex<HashMap<u64, VecDeque<crate::sync::Event>>>,
    /// Сессия почтового пути (PLG-08) → её учётка: события, база (у ядра) и ошибка записей без ответа.
    pub(in crate::backend) mails: Mutex<HashMap<u64, MailScope>>,
    /// Воркер-песочница разбора писем (PLG-08, услуга [`FromPlugin::Worker`]): один на процесс плагина.
    worker: Mutex<Option<postman_worker::Client>>,
    kill: Mutex<Box<dyn FnMut() + Send>>,
    exit_code: Mutex<Box<dyn FnMut() -> Option<u32> + Send>>,
    background: Mutex<Box<dyn FnMut(bool) -> io::Result<()> + Send>>,
    /// Приоритет понижен до первой синхронизации.
    lowered: AtomicBool,
    /// Простой вышел — процесс завершают до следующего запроса ([`Conn::watch`], PLG-06), не падение.
    idle_exit: AtomicBool,
}

impl Conn {
    fn kill(&self) {
        (lock(&self.kill))();
    }

    /// Запись хранилища учётки без ответа (PLG-08): выполняется сразу; ошибка — запомнится до ближайшего чтения
    /// (порядок записей сохраняется, ответа на запись нет).
    fn cache_write(&self, session: u64, op: &super::wire::CacheOp) -> Result<(), String> {
        let mails = lock(&self.mails);
        let scope = mails.get(&session).ok_or_else(|| "no such mail session".to_owned())?;
        match run_cache(&scope.cache, op) {
            Ok(_) => Ok(()),
            Err(e) => {
                *lock(&scope.failed) = Some(e.to_string());
                Err(e.to_string())
            }
        }
    }

    /// Воркер-песочница этой связи (PLG-08): запускается при первом разборе письма и живёт до конца связи.
    fn with_worker<T>(&self, f: impl FnOnce(&mut postman_worker::Client) -> T) -> T {
        let mut slot = lock(&self.worker);
        let worker = slot.get_or_insert_with(|| postman_worker::Client::new(postman_worker::Client::default_exe(), super::mail::WORKER_TIMEOUT));
        f(worker)
    }

    /// Код выхода для журнала: `, exit code …` или пусто.
    fn exit(&self) -> String {
        (lock(&self.exit_code))().map(|c| format!(", exit code {}", postman_proc::exit_name(c))).unwrap_or_default()
    }

    /// Первая синхронизация — приоритет процесса обратно ([`Conn::lowered`]): у каналов — первый курсор, у почты —
    /// первый пакет событий синхронизации, у областей — любое событие команды (AUD-04).
    fn restore_priority(&self, plugin: &Plugin) {
        if self.lowered.swap(false, Ordering::SeqCst)
            && let Err(e) = (lock(&self.background))(false)
        {
            tracing::debug!(plugin = plugin.row.id, "plugin priority not restored: {e}");
        }
    }

    pub(in crate::backend) fn id(&self) -> u64 {
        self.next.fetch_add(1, Ordering::Relaxed)
    }

    fn post(&self, ask: Ask, bin: &[u8], waiter: Waiter) -> Result<u64, ChannelError> {
        let id = self.id();
        lock(&self.pending).insert(id, waiter);
        // Связь закрылась до записи — ответа не будет: очистка `serve` могла уже пройти.
        if self.link.is_closed() || self.link.send(&ToPlugin::Ask { id, ask }, bin).is_err() {
            lock(&self.pending).remove(&id);
            return Err(gone());
        }
        Ok(id)
    }

    pub(in crate::backend) async fn ask(&self, ask: Ask, bin: &[u8]) -> Result<Answered, ChannelError> {
        let (tx, rx) = oneshot::channel();
        self.post(ask, bin, Waiter::Async(tx))?;
        rx.await.map_err(|_| gone())
    }

    pub(in crate::backend) fn ask_sync(&self, ask: Ask, wait: Duration) -> Option<Reply> {
        let (tx, rx) = mpsc::channel();
        let id = self.post(ask, &[], Waiter::Sync(tx)).ok()?;
        let reply = rx.recv_timeout(wait).ok().map(|(r, _)| r);
        if reply.is_none() {
            lock(&self.pending).remove(&id);
        }
        reply
    }

    /// [`Conn::ask_sync`] с двоичной частью ответа (публичная загрузка, PLG-07).
    fn ask_sync_full(&self, ask: Ask, wait: Duration) -> Option<Answered> {
        let (tx, rx) = mpsc::channel();
        let id = self.post(ask, &[], Waiter::Sync(tx)).ok()?;
        let answered = rx.recv_timeout(wait).ok();
        if answered.is_none() {
            lock(&self.pending).remove(&id);
        }
        answered
    }

    /// Обмен до конца связи (поток плагина): ответы — ждущим, события — сессиям, вызовы хранилища — владельцам.
    /// Ответ — чем кончился обмен ([`Ended`]).
    fn serve(self: &Arc<Self>, plugin: &Plugin) -> Ended {
        loop {
            match self.link.recv() {
                Incoming::Message(FromPlugin::Answer { id, reply }, bin) => match lock(&self.pending).remove(&id) {
                    Some(Waiter::Async(tx)) => {
                        let _ = tx.send((reply, bin));
                    }
                    Some(Waiter::Sync(tx)) => {
                        let _ = tx.send((reply, bin));
                    }
                    None => {}
                },
                Incoming::Message(FromPlugin::Event { session, event }, _) => {
                    // Первая синхронизация — приоритет процесса обратно.
                    if matches!(event, ChannelEvent::Cursor(_)) {
                        self.restore_priority(plugin);
                    }
                    if let Some(tx) = lock(&self.sessions).get(&session) {
                        let _ = tx.send(event);
                    }
                }
                // События команды области (PLG-07) ждёт заместитель в потоке учётки — до ответа.
                Incoming::Message(FromPlugin::AreaEvent { session, event }, _) => {
                    // Области статусов входа не шлют: любое событие — работа синхронизации, приоритет обратно (AUD-04).
                    self.restore_priority(plugin);
                    lock(&self.areas_events).entry(session).or_default().push_back(event);
                }
                // Событие потока синхронизации учётки (PLG-08) — в её канал, интерфейсу.
                Incoming::Message(FromPlugin::MailEvent { session, event }, _) => {
                    // Первый пакет событий синхронизации почты — приоритет обратно (AUD-04, как курсор у каналов).
                    if mail_sync_packet(&event) {
                        self.restore_priority(plugin);
                    }
                    if let Some(scope) = lock(&self.mails).get(&session) {
                        // Возможности пути (`Event::Features`, AUD-02): запомнить до конца сессии — по ним ядро
                        // решает, что держать в очереди.
                        if let crate::sync::Event::Features(f) = &event {
                            *lock(&scope.features) = *f;
                        }
                        // Ответы пути на билеты очереди ядра — очереди; интерфейсу — только её ошибки.
                        let internal = match &event {
                            crate::sync::Event::Sent { ticket, .. } | crate::sync::Event::ComposeFailed { ticket, .. } => {
                                *ticket >= super::mail::INTERNAL_TICKET
                            }
                            _ => false,
                        };
                        if internal {
                            if let Some(error) = scope.queue.confirm(&event) {
                                let _ = scope.events.send(error);
                                (lock(&scope.repaint))();
                            }
                        } else {
                            let _ = scope.events.send(event);
                            (lock(&scope.repaint))();
                        }
                    }
                }
                // Запись хранилища учётки без ответа (PLG-08): подряд, ошибка — на ближайшем чтении.
                Incoming::Message(FromPlugin::CacheWrite { session, op }, _) => {
                    if let Err(e) = self.cache_write(session, &op) {
                        tracing::error!(plugin = plugin.row.id, session, "cache write failed: {e}; the error surfaces on the next read");
                    }
                }
                // Поисковый индекс (PLG-10): строки писем — в индекс ядра; текст и база — у ядра, ответа нет.
                // Нет сессии или индекса — молча: путь без поиска и не зовёт, а после падения сессии строка
                // переиндексируется при следующем входе (письмо сохранено, поиск отстанет только на него).
                Incoming::Message(FromPlugin::Index { session, op: super::wire::IndexOp::Rows(rows) }, _) => {
                    let mails = lock(&self.mails);
                    let scope = mails.get(&session);
                    if let (Some(scope), Some(i)) = (scope, scope.and_then(|s| s.index.as_ref())) {
                        crate::indexer::rows(i, &scope.cache, &rows);
                    }
                }
                Incoming::Call(call, FromPlugin::Store { scope, op }, _) => {
                    let sent = lock(&self.scopes).get(&scope).is_some_and(|tx| tx.send((call, op)).is_ok());
                    if !sent {
                        let _ = self.link.reply(call, &ToPlugin::Stored(Err(ChannelError::Storage("no store for the scope".into()))), &[]);
                    }
                }
                // Чтение хранилища учётки (PLG-08) и ошибка прошлых записей без ответа.
                Incoming::Call(call, FromPlugin::Cache { session, op }, _) => {
                    let reply = match lock(&self.mails).get(&session) {
                        Some(scope) => match lock(&scope.failed).take() {
                            Some(e) => Err(e),
                            None => run_cache(&scope.cache, &op).map_err(|e| e.to_string()),
                        },
                        None => Err("no such mail session".into()),
                    };
                    let _ = self.link.reply(call, &ToPlugin::Cached(reply), &[]);
                }
                // Разбор письма воркером (PLG-08): воркер запускает ядро, не плагин.
                Incoming::Call(call, FromPlugin::Worker { op }, _) => {
                    let reply = self.with_worker(|w| run_worker(w, op));
                    let _ = self.link.reply(call, &ToPlugin::Worked(reply), &[]);
                }
                // S/MIME исходящего (PLG-08): ключи пользователя — у ядра.
                Incoming::Call(call, FromPlugin::Smime { op }, _) => {
                    let _ = self.link.reply(call, &ToPlugin::Sealed(run_smime(op)), &[]);
                }
                Incoming::Call(call, FromPlugin::Sso { flow, op }, _) => self.sso(call, flow, op),
                // Незнакомый вызов — отказом: плагин не виснет до своего предела ожидания.
                Incoming::Call(call, ..) => {
                    let _ = self.link.reply(call, &ToPlugin::Stored(Err(ChannelError::Storage("unexpected call".into()))), &[]);
                }
                Incoming::Message(..) | Incoming::Shutdown => {}
                Incoming::Closed => break,
            }
        }
        metrics_lock().rss.remove(plugin.row.id);
        // Простой вышел (каталог, PLG-06): процесс завершён до следующего запроса — не падение; ждущим — «процесса
        // нет», перезапуска не будет ([`supervise`]).
        if self.idle_exit.load(Ordering::SeqCst) {
            plugin.set(Phase::Idle);
            lock(&self.pending).clear();
            lock(&self.areas_events).clear();
            return Ended::Idle;
        }
        // Сначала — что процесса нет (повторное подключение не возьмёт мёртвую связь), затем — ждущим и сессиям.
        plugin.set(Phase::Down);
        lock(&self.pending).clear();
        lock(&self.scopes).clear();
        lock(&self.sso).clear();
        lock(&self.areas_events).clear();
        lock(&self.mails).clear();
        *lock(&self.worker) = None;
        for (_, tx) in lock(&self.sessions).drain() {
            let _ = tx.send(ChannelEvent::State(ChannelState::Offline { retry_in: 0 }));
        }
        Ended::Crashed(format!("link closed{}", self.exit()))
    }

    /// Сторож процесса: `Ping` раз в [`PING_EVERY`], нет `Pong` за [`PONG_WAIT`] — убить (обмен кончится, поток
    /// плагина перезапустит). Заодно — память процесса для `--bench` и простой каталога (PLG-06): вышел
    /// [`IDLE_EXIT`] с последнего запроса — процесс завершают до следующего (ADR 0013 «по запросу»).
    fn watch(&self, id: &'static str, plugin: Arc<Plugin>) {
        loop {
            match self.ask_sync(Ask::Stats, SYNC_WAIT) {
                Some(Reply::Stats { rss }) => {
                    metrics_lock().rss.insert(id, rss);
                }
                _ if self.link.is_closed() => return,
                _ => {}
            }
            std::thread::sleep(PING_EVERY);
            if self.link.is_closed() {
                return;
            }
            if let Some(idle) = *lock(&plugin.idle)
                && lock(&plugin.last_use).is_some_and(|last| last.elapsed() >= idle)
            {
                tracing::info!(plugin = id, "plugin idle for {} min, stopping until the next request", idle.as_secs() / 60);
                self.idle_exit.store(true, Ordering::SeqCst);
                self.kill();
                return;
            }
            if !self.link.ping(PONG_WAIT) {
                if !self.link.is_closed() {
                    tracing::warn!(plugin = id, "plugin did not answer ping in {} s, killing", PONG_WAIT.as_secs());
                    self.kill();
                }
                return;
            }
        }
    }

    /// Услуга SSO (ADR 0013 «Договор»): loopback для возврата браузера слушает ядро. `Listen` — порт сейчас, `Wait` —
    /// своим потоком (до [`sso::BROWSER_WAIT`]), ответ плагину по обмену; слушатель живёт до конца входа, повтор
    /// входа ждёт тот же порт.
    fn sso(self: &Arc<Self>, call: CallId, flow: u64, op: SsoOp) {
        match op {
            SsoOp::Listen => {
                let reply = sso::listen().map(|(l, port)| {
                    lock(&self.sso).insert(flow, Arc::new(l));
                    SsoDone::Port(port)
                });
                let _ = self.link.reply(call, &ToPlugin::SsoReply(reply.map_err(|e| ChannelError::Storage(format!("loopback: {e}")))), &[]);
            }
            SsoOp::Wait => {
                let Some(listener) = lock(&self.sso).get(&flow).cloned() else {
                    let _ = self.link.reply(call, &ToPlugin::SsoReply(Err(ChannelError::NotFound)), &[]);
                    return;
                };
                let conn = Arc::clone(self);
                let spawned = std::thread::Builder::new().name("plugin sso".into()).spawn(move || {
                    let token = sso::wait_token(&listener);
                    if let Err(e) = conn.link.reply(call, &ToPlugin::SsoReply(token.map(SsoDone::Token)), &[]) {
                        tracing::debug!("plugin sso reply: {e}");
                    }
                });
                if let Err(e) = spawned {
                    tracing::warn!("plugin sso thread: {e}");
                    let _ = self.link.reply(call, &ToPlugin::SsoReply(Err(ChannelError::Plugin(format!("sso thread: {e}")))), &[]);
                }
            }
        }
    }
}

/// Вызовы хранилища плагина — задачей в потоке, где живёт `PluginStore` (поток учётки или входа; там всегда
/// `LocalSet`, договор каналов). Задача кончается, когда сессию или вход закрыли.
fn serve_store(conn: &Arc<Conn>, store: Rc<dyn PluginStore>) -> UnboundedSender<(CallId, StoreOp)> {
    let (tx, mut rx) = unbounded_channel::<(CallId, StoreOp)>();
    let conn = Arc::clone(conn);
    tokio::task::spawn_local(async move {
        while let Some((call, op)) = rx.recv().await {
            let result = match op {
                StoreOp::Get(key) => store.get(&key),
                StoreOp::Put(key, value) => store.put(&key, &value).map(|()| None),
                StoreOp::RemovePrefix(prefix) => store.remove_prefix(&prefix).map(|()| None),
            };
            if let Err(e) = conn.link.reply(call, &ToPlugin::Stored(result), &[]) {
                tracing::debug!("plugin store reply: {e}");
            }
        }
    });
    tx
}

/// Заместитель канала: тот же `ChannelPlugin`, внутри — процесс набора.
pub struct RemoteChannel {
    host: Arc<Inner>,
    plugin: Arc<Plugin>,
    /// Какую «проблему» учётка уже показала: следующий `connect` с той же — это «Повторить».
    reported: Mutex<HashMap<String, u64>>,
    features: Mutex<Option<ChannelFeatures>>,
}

impl RemoteChannel {
    /// Работающий процесс: позвать его (фоновый поток плагина) и дождаться. «Проблема», которую учётка `account`
    /// ещё не показывала, — ошибкой; показанная (и любая у входа, `account = None`) — «Повторить».
    async fn ready(&self, account: Option<&AccountId>) -> Result<Arc<Conn>, ChannelError> {
        want(&self.host, &self.plugin);
        let mut rx = self.plugin.phase.subscribe();
        let mut retried: Option<u64> = None;
        loop {
            let phase = rx.borrow_and_update().clone();
            match phase {
                Phase::Ready(conn) if !conn.link.is_closed() => return Ok(conn),
                Phase::Problem { serial, error } if retried != Some(serial) => {
                    let shown = account.is_none_or(|a| lock(&self.reported).insert(a.0.clone(), serial) == Some(serial));
                    if retried.is_some() || !shown {
                        return Err(error);
                    }
                    retried = Some(serial);
                    self.plugin.retry();
                }
                _ => {}
            }
            if rx.changed().await.is_err() {
                return Err(gone());
            }
        }
    }

    /// Процесс поднят — его связь; иначе `None` (синхронные вопросы старта не ждут).
    fn running(&self) -> Option<Arc<Conn>> {
        match &*self.plugin.phase.borrow() {
            Phase::Ready(c) if !c.link.is_closed() => Some(Arc::clone(c)),
            _ => None,
        }
    }

    /// Ключ приложения в сборке плагина есть — из его `Hello` (PLG-05, ADR 0013: ключ — у плагина, интерфейсу — факт
    /// наличия). Процесс ещё не поднимался — не знаем: показываем вход (`true`), отказ «нет ключа» вернёт сам плагин.
    pub fn has_key(&self) -> bool {
        lock(&self.plugin.key).unwrap_or(true)
    }
}

/// Заместитель каталога (PLG-06): тот же [`SmimeDirectory`], внутри — процесс набора. Запросы редки («Найти в
/// каталоге», поиск сертификата при отправке): процесс стартует по первому запросу (после первого кадра, как у всех
/// плагинов) и завершается после [`IDLE_EXIT`] простоя — следующий запрос поднимет его снова (ADR 0013 «по запросу»).
/// Ответ ждёт в том же потоке, что и раньше ждал плагин ([`crate::smime::directory::lookup`]).
pub struct RemoteDirectory {
    host: Arc<Inner>,
    plugin: Arc<Plugin>,
    /// Настройки каталога («Настройки → Безопасность»); пароль простого входа читает ядро. Адреса запроса — при
    /// каждом [`SmimeDirectory::certificates`].
    query: DirectoryQuery,
}

impl RemoteDirectory {
    /// Работающий процесс: поднять и дождаться. «Проблема» — ещё одна попытка, как у каналов ([`RemoteChannel`]);
    /// после [`IDLE_EXIT`] процесс в [`Phase::Idle`] — поднимем ([`want`]). Смена фазы приходит от потока плагина:
    /// короткие паузы между опросами (`watch` без блокирующего ожидания), ждёт отдельный поток каталога.
    fn ready(&self) -> Result<Arc<Conn>, DirectoryError> {
        want(&self.host, &self.plugin);
        let mut rx = self.plugin.phase.subscribe();
        let mut retried: Option<u64> = None;
        loop {
            let mut ask_retry = false;
            {
                let phase = rx.borrow_and_update();
                if let Phase::Ready(conn) = &*phase
                    && !conn.link.is_closed()
                {
                    return Ok(Arc::clone(conn));
                }
                if let Phase::Problem { serial, error } = &*phase
                    && retried != Some(*serial)
                {
                    if retried.is_some() {
                        return Err(unavailable(error));
                    }
                    retried = Some(*serial);
                    ask_retry = true;
                }
            }
            if ask_retry {
                self.plugin.retry();
            }
            while matches!(rx.has_changed(), Ok(false)) {
                std::thread::sleep(PHASE_POLL);
            }
        }
    }
}

/// Отказ плагина каталога — ошибкой каталога, для строки «каталог недоступен».
fn unavailable(e: &ChannelError) -> DirectoryError {
    match e {
        ChannelError::Plugin(why) => DirectoryError::Unavailable(why.clone()),
        other => DirectoryError::Unavailable(other.to_string()),
    }
}

impl SmimeDirectory for RemoteDirectory {
    fn certificates(&self, addresses: &[String]) -> Result<Vec<Vec<u8>>, DirectoryError> {
        for _ in 0..2 {
            let conn = self.ready()?;
            // Запрос продлевает простой: процесс не завершится под ним ([`Conn::watch`]).
            *lock(&self.plugin.last_use) = Some(Instant::now());
            let mut query = self.query.clone();
            query.addresses = addresses.to_vec();
            match conn.ask_sync(Ask::Certificates(query), DIRECTORY_WAIT) {
                Some(Reply::Certs(r)) => return r,
                Some(_) => return Err(DirectoryError::Unavailable("unexpected directory reply".into())),
                // Процесс завершился под запросом (простой успел выйти) — поднимем снова; тайм-аут плагина — как есть.
                None if conn.link.is_closed() => {}
                None => return Err(DirectoryError::Timeout),
            }
        }
        Err(DirectoryError::Unavailable("plugin process is not running".into()))
    }
}

fn pick<T>(r: Result<Answered, ChannelError>, f: impl FnOnce(Reply) -> Option<Result<T, ChannelError>>) -> Result<T, ChannelError> {
    let (reply, _) = r?;
    f(reply).unwrap_or_else(|| Err(unexpected()))
}

impl ChannelPlugin for RemoteChannel {
    fn id(&self) -> &'static str {
        self.plugin.row.id
    }

    fn features(&self) -> ChannelFeatures {
        if let Some(Reply::Features(f)) = self.running().and_then(|c| c.ask_sync(Ask::Features, SYNC_WAIT)) {
            *lock(&self.features) = Some(f);
        }
        lock(&self.features).clone().unwrap_or_default()
    }

    fn parse_link(&self, url: &str) -> Option<Address> {
        match self.running()?.ask_sync(Ask::ParseLink(url.to_owned()), SYNC_WAIT)? {
            Reply::Address(a) => a,
            _ => None,
        }
    }

    fn link(&self, at: &Address) -> Option<String> {
        match self.running()?.ask_sync(Ask::Link(at.clone()), SYNC_WAIT)? {
            Reply::Url(u) => u,
            _ => None,
        }
    }

    fn person_url(&self, p: &Participant) -> Option<String> {
        match self.running()?.ask_sync(Ask::PersonUrl(p.clone()), SYNC_WAIT)? {
            Reply::Url(u) => u,
            _ => None,
        }
    }

    fn sign_in<'a>(&'a self, ctx: FlowCtx) -> BoxFuture<'a, Result<Box<dyn Flow + 'a>, ChannelError>> {
        Box::pin(async move {
            let conn = self.ready(None).await?;
            let flow = conn.id();
            lock(&conn.scopes).insert(flow, serve_store(&conn, ctx.store));
            let flow = RemoteFlow { conn, flow, scope: true };
            pick(flow.conn.ask(Ask::SignIn { flow: flow.flow }, &[]).await, |r| match r {
                Reply::Unit(r) => Some(r),
                _ => None,
            })?;
            Ok(Box::new(flow) as Box<dyn Flow>)
        })
    }

    fn connect<'a>(&'a self, ctx: ConnectCtx) -> BoxFuture<'a, Result<Box<dyn ChannelSession>, ChannelError>> {
        Box::pin(async move {
            let conn = self.ready(Some(&ctx.account)).await?;
            let session = conn.id();
            lock(&conn.sessions).insert(session, ctx.incoming);
            lock(&conn.scopes).insert(session, serve_store(&conn, ctx.store));
            // Сессия закрывается и при отказе: `Drop` убирает её события и хранилище.
            let mut remote = RemoteSession { conn, session, me: Participant::default(), features: ChannelFeatures::default() };
            let opened = pick(remote.conn.ask(Ask::Open { session, account: ctx.account, secret: ctx.secret }, &[]).await, |r| match r {
                Reply::Opened(r) => Some(r),
                _ => None,
            })?;
            let Opened { me, features } = opened;
            remote.me = me;
            remote.features = features;
            Ok(Box::new(remote) as Box<dyn ChannelSession>)
        })
    }
}

/// Сессия в процессе плагина.
struct RemoteSession {
    conn: Arc<Conn>,
    session: u64,
    me: Participant,
    features: ChannelFeatures,
}

impl RemoteSession {
    async fn call(&self, call: Call, bin: &[u8]) -> Result<Answered, ChannelError> {
        self.conn.ask(Ask::Session { session: self.session, call }, bin).await
    }

    async fn unit(&self, call: Call) -> Result<(), ChannelError> {
        pick(self.call(call, &[]).await, |r| match r {
            Reply::Unit(r) => Some(r),
            _ => None,
        })
    }

    async fn page(&self, call: Call) -> Result<Page, ChannelError> {
        pick(self.call(call, &[]).await, |r| match r {
            Reply::Page(r) => Some(r),
            _ => None,
        })
    }
}

impl Drop for RemoteSession {
    fn drop(&mut self) {
        lock(&self.conn.sessions).remove(&self.session);
        lock(&self.conn.scopes).remove(&self.session);
        let _ = self.conn.link.send(&ToPlugin::Close { session: self.session }, &[]);
    }
}

impl ChannelSession for RemoteSession {
    fn me(&self) -> Participant {
        self.me.clone()
    }

    fn features(&self) -> ChannelFeatures {
        self.features.clone()
    }

    fn catch_up<'a>(&'a self, cursor: Option<Cursor>) -> BoxFuture<'a, Result<(), ChannelError>> {
        Box::pin(self.unit(Call::CatchUp(cursor)))
    }

    fn history<'a>(&'a self, conv: &'a ConversationId, before: Option<&'a MessageId>, limit: u32) -> BoxFuture<'a, Result<Page, ChannelError>> {
        Box::pin(self.page(Call::History { conversation: conv.clone(), before: before.cloned(), limit }))
    }

    fn search<'a>(&'a self, query: &'a str, conv: Option<&'a ConversationId>, limit: u32) -> BoxFuture<'a, Result<Page, ChannelError>> {
        Box::pin(self.page(Call::Search { query: query.to_owned(), conversation: conv.cloned(), limit }))
    }

    fn act<'a>(&'a self, action: crate::channel::Action) -> BoxFuture<'a, Result<Done, ChannelError>> {
        Box::pin(async move {
            pick(self.call(Call::Act(action), &[]).await, |r| match r {
                Reply::Done(r) => Some(r),
                _ => None,
            })
        })
    }

    fn download<'a>(&'a self, src: &'a AttachmentSource, out: &'a mut dyn ByteSink) -> BoxFuture<'a, Result<(), ChannelError>> {
        Box::pin(async move {
            let (reply, bytes) = self.call(Call::Download(src.clone()), &[]).await?;
            match reply {
                Reply::Unit(r) => r?,
                _ => return Err(unexpected()),
            }
            bytes.chunks(64 * 1024).try_for_each(|chunk| out.write(chunk))
        })
    }

    fn upload<'a>(&'a self, meta: &'a FileMeta, src: &'a mut dyn ByteSource) -> BoxFuture<'a, Result<Uploaded, ChannelError>> {
        Box::pin(async move {
            if src.len() > MAX_BIN as u64 {
                return Err(ChannelError::TooLarge { max: MAX_BIN as u64 });
            }
            let mut bytes = Vec::with_capacity(usize::try_from(src.len()).unwrap_or(0));
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                let n = src.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                bytes.extend_from_slice(&buf[..n]);
            }
            pick(self.call(Call::Upload(meta.clone()), &bytes).await, |r| match r {
                Reply::Uploaded(r) => Some(r),
                _ => None,
            })
        })
    }

    fn flow<'a>(&'a self, kind: FlowKind) -> BoxFuture<'a, Result<Box<dyn Flow + 'a>, ChannelError>> {
        Box::pin(async move {
            let flow = RemoteFlow { conn: Arc::clone(&self.conn), flow: self.conn.id(), scope: false };
            self.unit(Call::Flow { flow: flow.flow, kind }).await?;
            Ok(Box::new(flow) as Box<dyn Flow>)
        })
    }

    fn sign_out<'a>(&'a self) -> BoxFuture<'a, Result<(), ChannelError>> {
        Box::pin(self.unit(Call::SignOut))
    }
}

/// Вход или поток вопросов сессии в процессе плагина.
struct RemoteFlow {
    conn: Arc<Conn>,
    flow: u64,
    /// Своё хранилище (вход); у потока сессии — хранилище сессии.
    scope: bool,
}

impl Flow for RemoteFlow {
    fn step<'a>(&'a mut self, answer: Option<Answer>) -> BoxFuture<'a, Result<Prompt, ChannelError>> {
        Box::pin(async move {
            pick(self.conn.ask(Ask::Step { flow: self.flow, answer }, &[]).await, |r| match r {
                Reply::Prompt(r) => Some(r),
                _ => None,
            })
        })
    }
}

impl Drop for RemoteFlow {
    fn drop(&mut self) {
        if self.scope {
            lock(&self.conn.scopes).remove(&self.flow);
        }
        let _ = self.conn.link.send(&ToPlugin::CloseFlow { flow: self.flow }, &[]);
    }
}

#[cfg(test)]
mod tests {
    use std::io::{PipeReader, PipeWriter, pipe};

    use super::*;
    use crate::backend::Registry;
    use crate::channel::{Action, ChanAccount, ChannelId, MemorySecrets, Request, Secret, Secrets, Start, Update};
    use crate::sync::Event;

    const WAIT: Duration = Duration::from_secs(10);

    const fn row(id: &'static str) -> Row {
        Row { id, kind: Kind::Channel, claim: Claim::Channel, set: "postman-plugins", profile: Profile::Network { enterprise_auth: false } }
    }

    /// Подставной запуск: процесс «создан», но не отвечает никогда (концы каналов держит сам запуск).
    #[derive(Default)]
    struct Silent {
        started: Mutex<Vec<&'static str>>,
        ends: Mutex<Vec<(PipeReader, PipeWriter)>>,
    }

    impl Silent {
        fn started(&self) -> Vec<&'static str> {
            lock(&self.started).clone()
        }

        /// Ждать `n` запусков; срок — только предел ожидания, проверка — по счётчику.
        fn wait_for(&self, n: usize) {
            let deadline = Instant::now() + WAIT;
            while lock(&self.started).len() < n && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }

    impl Launcher for Silent {
        fn launch(&self, row: &Row) -> io::Result<Started> {
            let (from_plugin, plugin_out) = pipe()?;
            let (plugin_in, to_plugin) = pipe()?;
            lock(&self.ends).push((plugin_in, plugin_out));
            lock(&self.started).push(row.id);
            Ok(Started {
                link: Link::new(from_plugin, to_plugin, MAX_BIN)?,
                kill: Box::new(|| {}),
                exit_code: Box::new(|| None),
                background: Box::new(|_| Ok(())),
            })
        }
    }

    /// Сторож запуска (ADR 0013 «Запуск», п. 8): поток учётки создаётся сразу, процесс до первого кадра — нет, запросы
    /// интерфейса копятся. Только счётчики, без замеров времени.
    #[test]
    fn nothing_starts_before_the_first_frame_and_requests_wait() {
        let launcher = Arc::new(Silent::default());
        let host = Host::new(launcher.clone(), Settings::default());
        let registry = Registry::default().with_channel(host.channel(row("demo")));
        let plugin = Arc::clone(registry.channel("demo").unwrap());
        let account = AccountId::new("me@example.test");
        let secrets = Arc::new(MemorySecrets::default());
        secrets.put(&ChannelId::new("demo"), &account, &Secret::new(b"session".to_vec())).unwrap();
        let start =
            Start { cache: postman_cache::Cache::open_in_memory().unwrap(), secrets, account, name: "demo".into(), render: None, index: None };
        let (events, rx) = std::sync::mpsc::channel();
        let commands = crate::sync::spawn_channel(plugin, start, events, Box::new(|| {}));
        for ticket in 0..3 {
            let action = Action::MarkRead { conversation: ConversationId::new("c"), up_to: MessageId::new("m") };
            commands.send(Request::Act { ticket, action }).unwrap();
        }
        let chan = ChanAccount::new("demo", "me@example.test");
        // Поток учётки дошёл до подключения и ждёт плагин.
        assert!(matches!(rx.recv_timeout(WAIT), Ok(Event::Channel(a, Update::State(ChannelState::Connecting))) if a == chan));
        assert_eq!(host.launches(), 0, "до первого кадра процессов нет");
        assert!(launcher.started().is_empty());
        assert!(rx.try_recv().is_err(), "запросы ждут в очереди, ответов нет");

        host.first_frame();
        launcher.wait_for(1);
        assert_eq!(launcher.started(), ["demo"]);
        assert_eq!(host.launches(), 1);
        // Плагин не ответил на `Hello`: учётка всё ещё подключается, запросы — в очереди.
        assert!(rx.try_recv().is_err());
        assert!(!commands.is_closed());
    }

    /// Не больше двух стартов сразу, первым — плагин с приоритетом.
    #[test]
    fn two_starts_at_most_preferred_first() {
        let launcher = Arc::new(Silent::default());
        let host = Host::new(launcher.clone(), Settings::default());
        let plugins: Vec<Arc<Plugin>> = ["a", "b", "c"].into_iter().map(|id| host.0.plugin(row(id))).collect();
        host.prefer("c");
        for p in &plugins {
            want(&host.0, p);
        }
        // Все в очереди и ждут первого кадра.
        let deadline = Instant::now() + WAIT;
        while lock(&host.0.gate).queue.len() < 3 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(host.launches(), 0);
        host.first_frame();
        launcher.wait_for(2);
        let started = launcher.started();
        assert_eq!(started.len(), 2, "третий ждёт, пока двое не ответят на Hello: {started:?}");
        assert_eq!(started[0], "c", "первым — плагин активной учётки: {started:?}");
        // Место второго выходит из очереди сразу после записи его запуска — ждём, пока очередь придёт к одному.
        let settled = Instant::now() + WAIT;
        while lock(&host.0.gate).queue.len() != 1 && Instant::now() < settled {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(lock(&host.0.gate).queue.len(), 1, "третий в очереди: {:?}", lock(&host.0.gate).queue);
    }

    /// Процесс другой сборки (обновление прервано): отвечает на `Hello` и только.
    #[derive(Default)]
    struct Stale(AtomicUsize);

    impl Launcher for Stale {
        fn launch(&self, row: &Row) -> io::Result<Started> {
            let (from_plugin, plugin_out) = pipe()?;
            let (plugin_in, to_plugin) = pipe()?;
            self.0.fetch_add(1, Ordering::SeqCst);
            let id = row.id;
            std::thread::spawn(move || {
                let link: PluginLink = Link::new(plugin_in, plugin_out, MAX_BIN).unwrap();
                while let Incoming::Message(ToPlugin::Ask { id: n, ask: Ask::Hello(_) }, _) = link.recv() {
                    let build = Build { build: "other".into(), ..Build::current() };
                    let reply = Reply::Hello(PluginHello { build, id: id.into(), kind: Kind::Channel, key: false });
                    link.send(&FromPlugin::Answer { id: n, reply }, &[]).unwrap();
                }
            });
            Ok(Started {
                link: Link::new(from_plugin, to_plugin, MAX_BIN)?,
                kill: Box::new(|| {}),
                exit_code: Box::new(|| None),
                background: Box::new(|_| Ok(())),
            })
        }
    }

    /// Сборки не совпали — «проблема» сразу, без перезапусков; следующий `connect` той же учётки — «Повторить».
    #[test]
    fn another_build_is_a_problem_until_retry() {
        let launcher = Arc::new(Stale::default());
        let host = Host::new(launcher.clone(), Settings::default());
        host.first_frame();
        let plugin = host.channel(row("demo"));
        let connect = |plugin: &RemoteChannel| {
            let ctx = ConnectCtx {
                account: AccountId::new("me@example.test"),
                secret: Secret::new(b"s".to_vec()),
                incoming: unbounded_channel().0,
                store: Rc::new(crate::channel::LocalStore::new(
                    postman_cache::Cache::open_in_memory().unwrap(),
                    &ChannelId::new("demo"),
                    &AccountId::new("me@example.test"),
                )),
            };
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            tokio::task::LocalSet::new().block_on(&rt, async { plugin.connect(ctx).await.err() })
        };
        assert!(matches!(connect(&plugin), Some(ChannelError::Plugin(e)) if e.contains("other")));
        assert_eq!(launcher.0.load(Ordering::SeqCst), 1, "без перезапусков");
        assert!(matches!(connect(&plugin), Some(ChannelError::Plugin(_))), "«Повторить» — новая попытка и новая проблема");
        assert_eq!(launcher.0.load(Ordering::SeqCst), 2);
    }

    /// Файла набора нет (PLG-14, компонент не установлен): «проблема» с той причиной, без перезапусков;
    /// «Повторить» — новая попытка и та же причина, пока файл не появится.
    #[test]
    fn missing_set_is_a_problem_until_retry() {
        #[derive(Default)]
        struct Absent(AtomicUsize);

        impl Launcher for Absent {
            fn launch(&self, _row: &Row) -> io::Result<Started> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Err(io::Error::from(io::ErrorKind::NotFound))
            }
        }
        let launcher = Arc::new(Absent::default());
        let host = Host::new(launcher.clone(), Settings::default());
        host.first_frame();
        let plugin = host.channel(row("demo"));
        let connect = |plugin: &RemoteChannel| {
            let ctx = ConnectCtx {
                account: AccountId::new("me@example.test"),
                secret: Secret::new(b"s".to_vec()),
                incoming: unbounded_channel().0,
                store: Rc::new(crate::channel::LocalStore::new(
                    postman_cache::Cache::open_in_memory().unwrap(),
                    &ChannelId::new("demo"),
                    &AccountId::new("me@example.test"),
                )),
            };
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            tokio::task::LocalSet::new().block_on(&rt, async { plugin.connect(ctx).await.err() })
        };
        let set = row("demo").set;
        assert!(matches!(connect(&plugin), Some(ChannelError::Missing(s)) if s == set));
        assert_eq!(launcher.0.load(Ordering::SeqCst), 1, "без перезапусков");
        assert!(matches!(connect(&plugin), Some(ChannelError::Missing(_))), "«Повторить» — новая попытка и та же причина");
        assert_eq!(launcher.0.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn rows_claim_addresses_without_a_process() {
        assert!(row("demo").claims("postman://demo/me@example.test/c"));
        assert!(!row("demo").claims("postman://telegram/1/c") && !row("demo").claims("postman://demonstration/1/c"));
        let imap = Row { claim: Claim::Scheme("imaps://"), kind: Kind::Mail, ..row("imap") };
        assert!(imap.claims("IMAPS://mail.example.test") && !imap.claims("https://mail.example.test"));
        let ews = Row { claim: Claim::Path("/EWS/Exchange.asmx"), kind: Kind::Mail, ..row("ews") };
        assert!(ews.claims("https://mail.example.test/ews/exchange.asmx") && !ews.claims("https://mail.example.test/?x=/EWS/Exchange.asmx"));
        let dav = Row { claim: Claim::Area("dav"), kind: Kind::Area, ..row("dav") };
        assert!(dav.claims("imaps://h?smtp=x&dav=https://h/dav") && !dav.claims("imaps://h?davx=1"));
        let directory = Row { claim: Claim::None, kind: Kind::Directory, ..row("ldap") };
        assert!(!directory.claims("ldap://dc.corp.example") && !directory.claims("postman://ldap/me/c"));
        assert_eq!(row("demo").exe(Path::new("bin")), Path::new("bin").join(format!("postman-plugins{}", std::env::consts::EXE_SUFFIX)));
    }
}
