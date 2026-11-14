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

use postman_proc::link::Incoming;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use super::super::wire::{Ask, Build, Call, FromPlugin, Kind, Opened, PluginHello, Reply, Settings, StoreOp, ToPlugin};
use super::{MAX_BIN, PluginLink};
use crate::channel::{
    AccountId, Answer, ByteSink, ByteSource, ChannelError, ChannelEvent, ChannelPlugin, ChannelSession, ConnectCtx, FlowCtx, PluginStore, Secret,
};

/// Вызов хранилища ядра ждёт ответа не дольше.
const STORE_WAIT: Duration = Duration::from_secs(30);

/// Куда процесс набора применяет уровень журнала из `Hello` и смены настроек.
static LOG: OnceLock<fn(&str)> = OnceLock::new();
static SETTINGS: Mutex<Option<Settings>> = Mutex::new(None);

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

/// Вопрос, на который отвечают сразу, без рантайма. `false` — не такой.
fn quick(link: &PluginLink, id: &'static str, plugin: Option<&dyn ChannelPlugin>, n: u64, ask: &Ask) -> bool {
    let reply = match (ask, plugin) {
        (Ask::Hello(h), _) => {
            apply(h.settings.clone());
            Reply::Hello(PluginHello { build: Build::current(), id: id.into(), kind: Kind::Channel })
        }
        (Ask::Stats, _) => Reply::Stats { rss: crate::bench::rss() as u64 },
        (Ask::Features, Some(p)) => Reply::Features(p.features()),
        (Ask::ParseLink(url), Some(p)) => Reply::Address(p.parse_link(url)),
        (Ask::Link(at), Some(p)) => Reply::Url(p.link(at)),
        (Ask::PersonUrl(person), Some(p)) => Reply::Url(p.person_url(person)),
        _ => return false,
    };
    answer(link, n, reply, &[]);
    true
}

/// Обслужить хост каналом `id`; `make` создаёт плагин при первом настоящем вопросе. Ответ — код выхода процесса.
pub fn serve_channel(link: PluginLink, id: &'static str, make: impl FnOnce() -> Arc<dyn ChannelPlugin>) -> i32 {
    let link = Arc::new(link);
    let first = loop {
        match link.recv() {
            Incoming::Message(ToPlugin::Ask { id: n, ask }, bin) => {
                if !quick(&link, id, None, n, &ask) {
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
                        if !quick(&link, id, Some(&*plugin), n, &ask) && tx.send((ToPlugin::Ask { id: n, ask }, bin)).is_err() {
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
                // Каналы исходящего не держат: очередь отправки — у ядра.
                ToPlugin::Leave | ToPlugin::Stored(_) => {}
            }
        }
    }

    fn ask(self: &Rc<Self>, id: u64, ask: Ask, bin: Vec<u8>) {
        match ask {
            Ask::Open { session, account, secret } => {
                tokio::task::spawn_local(Rc::clone(self).open(id, session, account, secret));
            }
            Ask::SignIn { flow } => {
                tokio::task::spawn_local(Rc::clone(self).sign_in(id, flow));
            }
            Ask::Step { flow, answer: a } => {
                let sent = self.flows.borrow().get(&flow).is_some_and(|tx| tx.send((id, a)).is_ok());
                if !sent {
                    answer(&self.link, id, Reply::Prompt(Err(ChannelError::NotFound)), &[]);
                }
            }
            Ask::Session { session, call } => {
                tokio::task::spawn_local(Rc::clone(self).call(id, session, call, bin));
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
        let plugin = Arc::clone(&self.plugin);
        match plugin.sign_in(FlowCtx { store }).await {
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
