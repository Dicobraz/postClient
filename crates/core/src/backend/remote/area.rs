//! Заместитель пути областей (PLG-07, ADR 0013): тот же [`AreaPlugin`] для ядра, внутри — процесс набора. Команды
//! областей учётки ведёт ядро ([`crate::backend::areas`]), у процесса — сессия учётки: вход ([`Ask::AreaOpen`]),
//! команды ([`Ask::AreaCommand`], события — `FromPlugin::AreaEvent`), ответ на приглашение ([`Ask::AreaRespond`]),
//! опрос ([`Ask::AreaChanged`]) и хранилище задач ([`Ask::AreaTask`]) — по обмену. Задачи поверх кэша — услуга
//! ядра: [`WireTasks`] задаёт плагину операции ([`TaskOp`]), а [`crate::tasks::handle`] ядра работает с кэшем
//! учётки.
//!
//! Процесс стартует по первому запросу заместителя; открытая сессия области держит его, простой без неё —
//! [`IDLE_EXIT`] (публичные загрузки — [`Host::fetch`] — и каталог живут по тем же правилам). Процесс упал или
//! завершился — сессия учётки входит заново при следующей команде: календарю не нужно переподключение учётки.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde_json::{Map, Value};

use super::super::wire::{AreaLogin, Ask, Fetched, Reply, TaskDone, TaskOp, ToPlugin};
use super::{Claim, Conn, IDLE_EXIT, Inner, Phase, Plugin, Row, want};
use crate::backend::{AreaCtx, AreaPlugin, Areas, BoxFuture, Login};
use crate::model::{Error, ErrorKind, Features};
use crate::tasks::{Delta, TaskStore};

/// Вид сервера по адресу ждёт ответа плагина не дольше: проверка `/.well-known` у плагина ограничена, обмен — с
/// запасом.
const PROBE_WAIT: Duration = Duration::from_secs(10);
/// Публичная загрузка ждёт ответа не дольше: сеть у плагина ограничена 5 с, обмен — с запасом.
const FETCH_WAIT: Duration = Duration::from_secs(10);
/// Пауза между опросами фазы процесса в блокирующем ожидании (поток интерфейса) — как у каталога.
const PHASE_POLL: Duration = Duration::from_millis(5);

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Работающий процесс: поднять и дождаться (фоновый поток плагина, рантайм уже есть). «Проблема» — одна
/// повторная попытка, как у каналов; [`Phase::Idle`] после простоя — поднять ([`want`]).
async fn ready_async(host: &Arc<Inner>, plugin: &Arc<Plugin>) -> Result<Arc<Conn>, Error> {
    want(host, plugin);
    let mut rx = plugin.phase.subscribe();
    let mut retried = false;
    loop {
        let phase = rx.borrow_and_update().clone();
        match phase {
            Phase::Ready(conn) if !conn.link.is_closed() => return Ok(conn),
            Phase::Problem { .. } if !retried => {
                retried = true;
                plugin.retry();
            }
            Phase::Problem { error, .. } => return Err(Error::other(ErrorKind::Server, error.to_string())),
            _ => {}
        }
        if rx.changed().await.is_err() {
            return Err(Error::network("plugin process is not running"));
        }
    }
}

/// [`ready_async`] для потока без рантайма (публичные загрузки): фазы ведёт поток плагина — короткие паузы между
/// опросами, как у каталога ([`crate::backend::remote::RemoteDirectory`]).
fn ready_blocking(host: &Arc<Inner>, plugin: &Arc<Plugin>) -> Result<Arc<Conn>, Error> {
    want(host, plugin);
    let mut rx = plugin.phase.subscribe();
    let mut retried = false;
    loop {
        let mut ask_retry = false;
        {
            let phase = rx.borrow_and_update();
            if let Phase::Ready(conn) = &*phase
                && !conn.link.is_closed()
            {
                return Ok(Arc::clone(conn));
            }
            if let Phase::Problem { .. } = &*phase
                && !retried
            {
                retried = true;
                ask_retry = true;
            }
        }
        if ask_retry {
            plugin.retry();
        }
        while matches!(rx.has_changed(), Ok(false)) {
            std::thread::sleep(PHASE_POLL);
        }
    }
}

impl super::Host {
    /// Заместитель пути областей по строке (PLG-07): процесс стартует по первому запросу; пока открыта сессия
    /// области учётки, процесс держится (не завершается по простою).
    pub fn area(&self, row: Row) -> RemoteAreas {
        let plugin = self.0.plugin(row);
        RemoteAreas { host: Arc::clone(&self.0), plugin }
    }

    /// Публичный ресурс процессом плагина ([`Ask::Fetch`], PLG-07): картинка письма, подписка на календарь,
    /// занятость по ссылке. Блокирует поток — звать из фона. Процесс стартует по первой загрузке; [`IDLE_EXIT`]
    /// простоя с последнего запроса завершает его до следующей (открытая сессия области учётки держит процесс).
    /// `max` — предел тела (обрезается пределом кадра); `etag` — условный запрос. Ответ — вид и тело.
    pub fn fetch(&self, row: Row, url: String, max: usize, etag: Option<String>) -> Result<(Fetched, Vec<u8>), String> {
        let plugin = self.0.plugin(row);
        if plugin.areas_open.load(Ordering::SeqCst) == 0 {
            *lock(&plugin.idle) = Some(IDLE_EXIT);
        }
        let conn = ready_blocking(&self.0, &plugin).map_err(|e| e.to_string())?;
        // Загрузка продлевает простой: процесс не завершится под ней ([`Conn::watch`]).
        *lock(&plugin.last_use) = Some(std::time::Instant::now());
        let max = max.min(super::MAX_BIN);
        let ask = Ask::Fetch { url, max: u32::try_from(max).unwrap_or(u32::MAX), etag };
        let (reply, body) = conn.ask_sync_full(ask, FETCH_WAIT).ok_or_else(|| "plugin process is not running".to_owned())?;
        match reply {
            Reply::Fetched(r) => r.map(|f| (f, body)),
            _ => Err("plugin: unexpected reply".into()),
        }
    }
}

/// Заместитель пути областей: [`AreaPlugin`] для ядра, внутри — процесс набора.
pub struct RemoteAreas {
    host: Arc<Inner>,
    plugin: Arc<Plugin>,
}

impl RemoteAreas {
    /// Ключ адреса учётки ([`Claim::Area`]): `dav` → `?dav=https://…`.
    fn key(&self) -> &'static str {
        match self.plugin.row.claim {
            Claim::Area(key) => key,
            _ => self.plugin.row.id,
        }
    }
}

impl AreaPlugin for RemoteAreas {
    fn key(&self) -> &'static str {
        RemoteAreas::key(self)
    }

    /// Вид сервера по адресу ([`AreaPlugin::probe`]): процесс стартует по запросу — автодополнение адреса учётки
    /// («Добавить учётную запись») само поднимает набор.
    fn probe<'a>(&'a self, host: &'a str) -> BoxFuture<'a, Option<String>> {
        Box::pin(async move {
            let conn = ready_async(&self.host, &self.plugin).await.ok()?;
            match conn.ask_sync(Ask::Probe(host.to_owned()), PROBE_WAIT) {
                Some(Reply::Url(found)) => found,
                _ => None,
            }
        })
    }

    fn set_tls13_only(&self, _on: bool) {
        // Настройки политики процесс получает с `Hello` и сменой ([`Host::set_settings`], [`super::super::wire::Settings`]);
        // путь областей применяет их сам.
    }

    /// Войти в область учётной записи: процесс поднимается, сессия учётки открывается по обмену. Пока сессия
    /// жива ([`RemoteAreasSession`]) — процесс держится.
    fn connect<'a>(&'a self, url: &'a str, login: &'a Login, profile: Option<&'a str>) -> BoxFuture<'a, Result<Box<dyn Areas>, Error>> {
        Box::pin(async move {
            let mut session = RemoteAreasSession {
                host: Arc::clone(&self.host),
                plugin: Arc::clone(&self.plugin),
                conn: None,
                session_no: 0,
                features: Features::NONE,
                url: url.to_owned(),
                login: login.clone(),
                profile: profile.map(str::to_owned),
                opened: false,
            };
            session.hello().await?;
            Ok(Box::new(session) as Box<dyn Areas>)
        })
    }
}

/// Сессия области учётной записи в процессе плагина: те же [`Areas`] для ядра. Процесс завершился — сессия входит
/// заново при следующей команде (`hello`), учётка не переподключается.
pub(super) struct RemoteAreasSession {
    host: Arc<Inner>,
    plugin: Arc<Plugin>,
    conn: Option<Arc<Conn>>,
    /// Номер сессии в процессе ([`Conn::id`]) — им помечены команды, события и закрытие.
    session_no: u64,
    features: Features,
    /// Свои данные входа — для повторного входа ([`Ask::AreaOpen`]).
    url: String,
    login: Login,
    profile: Option<String>,
    /// Сессия сейчас открыта (счётчик [`Plugin::areas_open`] и простой процесса учтены).
    opened: bool,
}

impl RemoteAreasSession {
    /// Номер сессии в процессе (после [`RemoteAreasSession::hello`]).
    fn no(&self) -> u64 {
        self.session_no
    }

    /// Вход сессии по обмену ([`Ask::AreaOpen`]); прежнюю — закрыть, если была.
    async fn hello(&mut self) -> Result<(), Error> {
        self.close();
        let conn = ready_async(&self.host, &self.plugin).await?;
        let session = conn.id();
        let ask = Ask::AreaOpen {
            session,
            url: self.url.clone(),
            profile: self.profile.clone(),
            login: AreaLogin {
                user: self.login.user.clone(),
                password: crate::channel::Secret::new(self.login.password.clone().into_bytes()),
                email: self.login.email.clone(),
            },
        };
        let features = match conn.ask(ask, &[]).await {
            Ok((Reply::AreaOpened(r), _)) => r?,
            Ok(_) => return Err(Error::Protocol("plugin: unexpected reply".into())),
            Err(e) => return Err(Error::other(ErrorKind::Network, e.to_string())),
        };
        self.conn = Some(conn);
        self.session_no = session;
        self.features = features;
        // Пока сессия жива — процесс не завершают по простою ([`Conn::watch`], [`Host::fetch`]).
        self.opened = true;
        self.plugin.areas_open.fetch_add(1, Ordering::SeqCst);
        *lock(&self.plugin.idle) = None;
        Ok(())
    }

    /// Сессию закрыть ([`ToPlugin::Close`]) и разконтровать простой процесса.
    fn close(&mut self) {
        if !self.opened {
            return;
        }
        self.opened = false;
        self.plugin.areas_open.fetch_sub(1, Ordering::SeqCst);
        if self.plugin.areas_open.load(Ordering::SeqCst) == 0 {
            *lock(&self.plugin.idle) = Some(IDLE_EXIT);
            *lock(&self.plugin.last_use) = Some(std::time::Instant::now());
        }
        if let Some(conn) = &self.conn {
            lock(&conn.areas_events).remove(&self.session_no);
            let _ = conn.link.send(&ToPlugin::Close { session: self.session_no }, &[]);
        }
    }

    /// Работающая связь сессии: процесс завершился — войти заново ([`RemoteAreasSession::hello`]).
    async fn ensure(&mut self) -> Result<&Arc<Conn>, Error> {
        if self.conn.as_ref().is_none_or(|c| c.link.is_closed()) {
            self.hello().await?;
        }
        match self.conn.as_ref() {
            Some(conn) => Ok(conn),
            None => Err(Error::Protocol("area session is not open".into())),
        }
    }

    /// События команды, пришедшие во время ожидания ответа ([`FromPlugin::AreaEvent`]), — куда скажет ядро.
    fn drain(&self, notify: &dyn Fn(crate::sync::Event)) {
        if let Some(events) = self.conn.as_ref().and_then(|c| lock(&c.areas_events).remove(&self.session_no)) {
            for e in events {
                notify(e);
            }
        }
    }
}

impl Drop for RemoteAreasSession {
    fn drop(&mut self) {
        self.close();
    }
}

impl Areas for RemoteAreasSession {
    fn features(&self) -> Features {
        self.features
    }

    fn handle<'a>(&'a mut self, cmd: crate::sync::Command, ctx: AreaCtx<'a>) -> BoxFuture<'a, Option<crate::sync::Command>> {
        Box::pin(async move {
            // Задачи поверх кэша — услуга ядра: кэш у ядра, хранилище плагина — по обмену ([`WireTasks`]).
            if let crate::sync::Command::Tasks(request) = cmd {
                return match self.ensure().await {
                    Ok(conn) => {
                        let store = WireTasks { conn: Arc::clone(conn), session: self.no() };
                        let replies = |r: crate::tasks::Reply| (ctx.notify)(crate::sync::Event::Tasks(r));
                        match crate::tasks::handle(&store, ctx.cache, request, &replies).await {
                            Ok(()) => None,
                            Err(e) => {
                                (ctx.notify)(crate::sync::Event::Tasks(crate::tasks::Reply::Failed { uid: None, error: e.to_string() }));
                                None
                            }
                        }
                    }
                    Err(e) => {
                        (ctx.notify)(crate::sync::Event::Tasks(crate::tasks::Reply::Failed { uid: None, error: e.to_string() }));
                        None
                    }
                };
            }
            let conn = match self.ensure().await {
                Ok(conn) => Arc::clone(conn),
                Err(e) => {
                    (ctx.notify)(crate::sync::Event::CalendarFailed(e.to_string()));
                    return None;
                }
            };
            let back = match conn.ask(Ask::AreaCommand { session: self.no(), cmd: cmd.clone() }, &[]).await {
                Ok((Reply::AreaCommand(Ok(true)), _)) => None,
                // «Не моя» — назад почтовому пути; ошибка обмена — процесс не донёс: команды нет и ответа нет.
                Ok((Reply::AreaCommand(Ok(false)), _)) => Some(cmd),
                Ok((Reply::AreaCommand(Err(e)), _)) => {
                    tracing::warn!("area command through the plugin: {e}");
                    Some(cmd)
                }
                Ok(_) => {
                    tracing::warn!("area command through the plugin: unexpected reply");
                    Some(cmd)
                }
                Err(e) => {
                    (ctx.notify)(crate::sync::Event::CalendarFailed(e.to_string()));
                    return None;
                }
            };
            self.drain(ctx.notify);
            back
        })
    }

    fn respond<'a>(
        &'a mut self,
        invite: &'a postman_calendar::Invitation,
        status: postman_calendar::Partstat,
        answer: &'a crate::invite::Answer,
    ) -> BoxFuture<'a, Result<crate::invite::Responded, Error>> {
        Box::pin(async move {
            let conn = self.ensure().await?;
            let ask = Ask::AreaRespond { session: self.no(), invite: invite.clone(), status, answer: answer.clone() };
            match conn.ask(ask, &[]).await {
                Ok((Reply::AreaResponded(r), _)) => r,
                Ok(_) => Err(Error::Protocol("plugin: unexpected reply".into())),
                Err(e) => Err(Error::other(ErrorKind::Network, e.to_string())),
            }
        })
    }

    fn flush_tasks<'a>(&'a mut self, ctx: AreaCtx<'a>) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            let conn = match self.ensure().await {
                Ok(conn) => Arc::clone(conn),
                Err(e) => return Err(e),
            };
            let store = WireTasks { conn, session: self.no() };
            let replies = |r: crate::tasks::Reply| (ctx.notify)(crate::sync::Event::Tasks(r));
            crate::tasks::handle(&store, ctx.cache, crate::tasks::Request::Flush, &replies).await
        })
    }

    fn changed(&mut self) -> BoxFuture<'_, Result<bool, Error>> {
        Box::pin(async move {
            let conn = self.ensure().await?;
            match conn.ask(Ask::AreaChanged { session: self.no() }, &[]).await {
                Ok((Reply::AreaChanged(r), _)) => r,
                Ok(_) => Err(Error::Protocol("plugin: unexpected reply".into())),
                Err(e) => Err(Error::other(ErrorKind::Network, e.to_string())),
            }
        })
    }
}

/// Хранилище задач плагина по обмену ([`Ask::AreaTask`]): услуги ядра ([`crate::tasks::handle`]) спрашивают
/// операции ([`TaskOp`]) — записи JSCalendar едут байтами JSON ([`super::super::wire::json`]).
struct WireTasks {
    conn: Arc<Conn>,
    session: u64,
}

impl WireTasks {
    async fn ask(&self, op: TaskOp) -> Result<TaskDone, Error> {
        match self.conn.ask(Ask::AreaTask { session: self.session, op }, &[]).await {
            Ok((Reply::AreaTask(r), _)) => r,
            Ok(_) => Err(Error::Protocol("plugin: unexpected task reply".into())),
            Err(e) => Err(Error::other(ErrorKind::Network, e.to_string())),
        }
    }
}

impl TaskStore for WireTasks {
    async fn load(&self) -> crate::model::Result<(Vec<Value>, String)> {
        match self.ask(TaskOp::Load).await? {
            TaskDone::Loaded { tasks, state } => Ok((tasks, state)),
            _ => Err(Error::Protocol("plugin: unexpected task reply".into())),
        }
    }

    async fn changes(&self, since: &str) -> crate::model::Result<Delta> {
        match self.ask(TaskOp::Changes(since.to_owned())).await? {
            TaskDone::Delta(delta) => Ok(delta),
            _ => Err(Error::Protocol("plugin: unexpected task reply".into())),
        }
    }

    async fn get(&self, ids: &[String]) -> crate::model::Result<Vec<Value>> {
        match self.ask(TaskOp::Get(ids.to_vec())).await? {
            TaskDone::Got(tasks) => Ok(tasks),
            _ => Err(Error::Protocol("plugin: unexpected task reply".into())),
        }
    }

    async fn create(&self, task: &Value) -> crate::model::Result<String> {
        match self.ask(TaskOp::Create(task.clone())).await? {
            TaskDone::Created(id) => Ok(id),
            _ => Err(Error::Protocol("plugin: unexpected task reply".into())),
        }
    }

    async fn update(&self, id: &str, patch: &Map<String, Value>) -> crate::model::Result<()> {
        match self.ask(TaskOp::Update { id: id.to_owned(), patch: patch.clone() }).await? {
            TaskDone::Done => Ok(()),
            _ => Err(Error::Protocol("plugin: unexpected task reply".into())),
        }
    }

    async fn destroy(&self, id: &str) -> crate::model::Result<()> {
        match self.ask(TaskOp::Destroy(id.to_owned())).await? {
            TaskDone::Done => Ok(()),
            _ => Err(Error::Protocol("plugin: unexpected task reply".into())),
        }
    }

    async fn default_calendar(&self) -> crate::model::Result<String> {
        match self.ask(TaskOp::DefaultCalendar).await? {
            TaskDone::Calendar(href) => Ok(href),
            _ => Err(Error::Protocol("plugin: unexpected task reply".into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::pipe;
    use std::sync::Arc;

    use super::*;
    use crate::backend::remote::{AreaServer, FetchedBody, serve_area};
    use crate::model::Feature;
    use crate::sync::Command;
    use postman_proc::link::Link;

    const ROW: Row = Row {
        id: "dav",
        kind: super::super::super::wire::Kind::Area,
        claim: Claim::Area("dav"),
        set: "postman-plugins",
        profile: super::super::Profile::Network { enterprise_auth: false },
    };

    /// Подключённая область подставного плагина: неделя — с событием `CalendarChanged`, остальное — «не моя».
    struct MockArea;

    impl Areas for MockArea {
        fn features(&self) -> Features {
            Features::NONE.with(Feature::Calendar, true)
        }

        fn handle<'a>(&'a mut self, cmd: Command, ctx: AreaCtx<'a>) -> BoxFuture<'a, Option<Command>> {
            Box::pin(async move {
                match cmd {
                    Command::Events { from, to } => {
                        assert_eq!((from, to), (1, 2));
                        (ctx.notify)(crate::sync::Event::CalendarChanged);
                        None
                    }
                    other => Some(other),
                }
            })
        }

        fn respond<'a>(
            &'a mut self,
            _invite: &'a postman_calendar::Invitation,
            status: postman_calendar::Partstat,
            _answer: &'a crate::invite::Answer,
        ) -> BoxFuture<'a, Result<crate::invite::Responded, Error>> {
            Box::pin(async move {
                if status == postman_calendar::Partstat::Accepted {
                    Ok(crate::invite::Responded::Sent)
                } else {
                    Err(Error::Protocol("mock: only accepted".into()))
                }
            })
        }

        fn flush_tasks<'a>(&'a mut self, _ctx: AreaCtx<'a>) -> BoxFuture<'a, Result<(), Error>> {
            Box::pin(async move { Ok(()) })
        }

        fn changed(&mut self) -> BoxFuture<'_, Result<bool, Error>> {
            Box::pin(async move { Ok(true) })
        }
    }

    impl AreaSession for MockArea {
        fn task_op<'a>(&'a mut self, op: TaskOp) -> BoxFuture<'a, Result<TaskDone, Error>> {
            Box::pin(async move {
                match op {
                    TaskOp::Create(_) => Ok(TaskDone::Created("/dav/cal/default/t1.ics".into())),
                    _ => Err(Error::Protocol("mock: only create".into())),
                }
            })
        }
    }

    /// Путь областей на стороне процесса — тот же `serve_area`, что у набора.
    struct MockServer;

    impl AreaServer for MockServer {
        fn probe<'a>(&'a self, _host: &'a str) -> BoxFuture<'a, Option<String>> {
            Box::pin(async move { None })
        }

        fn connect<'a>(&'a self, url: &'a str, _login: &'a Login, profile: Option<&'a str>) -> BoxFuture<'a, Result<Box<dyn AreaSession>, Error>> {
            assert_eq!(url, "https://dav.example.test");
            assert_eq!(profile, Some("@2"), "профиль учётки доезжает до плагина");
            Box::pin(async move { Ok(Box::new(MockArea) as Box<dyn AreaSession>) })
        }

        fn fetch<'a>(&'a self, url: &'a str, max: u32, etag: Option<&'a str>) -> BoxFuture<'a, Result<Option<FetchedBody>, String>> {
            Box::pin(async move {
                assert_eq!(url, "https://cal.example.test/f.ics");
                assert_eq!(etag, Some("\"7\""));
                Ok(Some(FetchedBody { body: vec![0xAB; max as usize], etag: Some("\"8\"".into()) }))
            })
        }
    }

    /// Сторона плагина — поток теста с точкой входа набора.
    struct InThread;

    impl super::super::Launcher for InThread {
        fn launch(&self, _row: &Row) -> std::io::Result<super::super::Started> {
            let (from_plugin, plugin_out) = pipe()?;
            let (plugin_in, to_plugin) = pipe()?;
            std::thread::spawn(move || {
                let link = Link::new(plugin_in, plugin_out, super::super::MAX_BIN).unwrap();
                serve_area(link, "dav", || Arc::new(MockServer))
            });
            Ok(super::super::Started {
                link: Link::new(from_plugin, to_plugin, super::super::MAX_BIN)?,
                kill: Box::new(|| {}),
                exit_code: Box::new(|| None),
                background: Box::new(|_| Ok(())),
            })
        }
    }

    fn host() -> (super::super::Host, Arc<Plugin>) {
        let host = super::super::Host::new(Arc::new(InThread), super::super::Settings::default());
        host.first_frame();
        let plugin = host.0.plugin(ROW);
        (host, plugin)
    }

    fn login() -> Login {
        Login { user: "me@example.test".into(), password: "pw".into(), email: "me@example.test".into() }
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
    }

    /// Контекст команды: `AreaCtx` строится в выражении вызова — замыкание и заём живут до конца `block_on`.
    macro_rules! ctx {
        ($cache:expr, $seen:expr) => {
            AreaCtx { cache: &$cache, profile: None, owner: None, notify: &|e: crate::sync::Event| $seen.lock().unwrap().push(e) }
        };
    }

    /// Команды учётки через заместитель: «не моя» — назад, неделя — области с событием, ответ на приглашение,
    /// опрос. Процесс поднимается по первому подключению и держится, пока сессия открыта.
    #[test]
    fn area_commands_cross_the_process() {
        let (host, plugin) = host();
        let area = host.area(ROW);
        let mut session: Box<dyn Areas> = rt().block_on(async { area.connect("https://dav.example.test", &login(), Some("@2")).await.unwrap() });
        assert_eq!(plugin.areas_open.load(Ordering::SeqCst), 1, "сессия держит процесс");
        assert_eq!(*lock(&plugin.idle), None, "простоя у сессии учётки нет");
        assert_eq!(session.features(), Features::NONE.with(Feature::Calendar, true));
        let cache = postman_cache::Cache::open_in_memory().unwrap();
        let seen = std::sync::Mutex::new(Vec::new());
        // Не её команда — назад почтовому пути.
        let back = rt().block_on(session.handle(Command::Reconnect, ctx!(cache, seen)));
        assert!(back.is_some(), "Reconnect — почте");
        assert!(seen.lock().unwrap().is_empty());
        // Неделя — области: событие доходит до ядра.
        let back = rt().block_on(session.handle(Command::Events { from: 1, to: 2 }, ctx!(cache, seen)));
        assert!(back.is_none(), "неделя — области");
        assert_eq!(*seen.lock().unwrap(), [crate::sync::Event::CalendarChanged], "событие команды — до ответа");
        // Ответ на приглашение своей копией.
        let invite = crate::invite::tests::inv();
        let answered = rt().block_on(session.respond(&invite, postman_calendar::Partstat::Accepted, &crate::invite::Answer::default()));
        assert_eq!(answered, Ok(crate::invite::Responded::Sent));
        // Изменилось ли что-то.
        assert_eq!(rt().block_on(session.changed()), Ok(true));
        drop(session);
        assert_eq!(plugin.areas_open.load(Ordering::SeqCst), 0, "сессия закрыта");
        assert_eq!(*lock(&plugin.idle), Some(IDLE_EXIT), "процесс держат до простоя");
    }

    /// Задачи поверх кэша — услуга ядра: правка «не отправлена» уходит в хранилище плагина операциями
    /// ([`WireTasks`]) и до почтового пути не доходит.
    #[test]
    fn tasks_go_to_the_plugin_store_by_ops() {
        let (host, _plugin) = host();
        let area = host.area(ROW);
        let mut session: Box<dyn Areas> = rt().block_on(async { area.connect("https://dav.example.test", &login(), None).await.unwrap() });
        let cache = postman_cache::Cache::open_in_memory().unwrap();
        let task = postman_calendar::Task::from_jmap(&serde_json::json!({ "@type": "Task", "uid": "t-1", "title": "Отчёт" })).unwrap();
        crate::tasks::save_local(&cache, task).unwrap();
        let seen = std::sync::Mutex::new(Vec::new());
        let back = rt().block_on(session.handle(Command::Tasks(crate::tasks::Request::Flush), ctx!(cache, seen)));
        assert!(back.is_none(), "команда задач — услуга ядра");
        let seen = seen.lock().unwrap();
        assert!(seen.iter().any(|e| matches!(e, crate::sync::Event::Tasks(_))), "правка дошла до хранилища плагина: {seen:?}");
    }

    /// Публичная загрузка ([`Host::fetch`]) — телом кадра; сессий области нет — процесс завершают по простою.
    #[test]
    fn fetch_returns_the_body_through_the_process() {
        let (host, plugin) = host();
        let got = host.fetch(ROW, "https://cal.example.test/f.ics".into(), 4, Some("\"7\"".into())).unwrap();
        assert_eq!(got, (Fetched::Body { etag: Some("\"8\"".into()) }, vec![0xAB; 4]));
        assert_eq!(*lock(&plugin.idle), Some(IDLE_EXIT), "сессий нет — простой возвращён");
    }
}
