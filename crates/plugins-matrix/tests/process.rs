//! Канал Matrix настоящим процессом `postman-plugins-matrix --plugin=matrix` в песочнице «сеть без файлов»
//! (ADR 0013, PLG-04): `Hello` со сверкой сборки, вход паролем и через браузер (SSO: loopback слушает ядро, токен —
//! плагину по обмену), подключение, догон, хранилище плагина — в базе хоста услугой ядра. Сервер — подставной
//! (`fake`) на loopback; Windows: AppContainer к своей машине не подключается (изоляция loopback, PLG-01) — сетевая
//! часть пропускается, как у проверок профиля (`crates/proc/tests/network.rs`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // интеграционный тест: «не падать» — для кода приложения (S2-03)

use std::path::Path;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use postman_backend_matrix::fake::{self, FakeMatrix};
use postman_cache::Cache;
use postman_core::backend::remote::{self, Claim, Host, Launcher, Namespaces, ProcLauncher, Profile, Row, Started};
use postman_core::backend::wire::{Kind, Settings};
use postman_core::channel::{
    AccountId, Answer, ChannelError, ChannelEvent, ChannelId, ChannelPlugin, ConnectCtx, FlowCtx, LocalStore, PluginStore, Prompt, Secret,
};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

const ROW: Row = Row {
    id: postman_backend_matrix::ID,
    kind: Kind::Channel,
    claim: Claim::Channel,
    set: "postman-plugins-matrix",
    profile: Profile::Network { enterprise_auth: false },
};
const WAIT: Duration = Duration::from_secs(30);

type Kill = Arc<Mutex<Option<Box<dyn FnMut() + Send>>>>;

/// Настоящий запуск набора; тест может убить последний процесс.
struct Killable {
    inner: ProcLauncher,
    last: Kill,
}

impl Launcher for Killable {
    fn launch(&self, row: &Row) -> std::io::Result<Started> {
        let mut s = self.inner.launch(row)?;
        let kill = std::mem::replace(&mut s.kill, Box::new(|| {}));
        *self.last.lock().unwrap() = Some(kill);
        let last = Arc::clone(&self.last);
        s.kill = Box::new(move || {
            if let Some(k) = last.lock().unwrap().as_mut() {
                k();
            }
        });
        Ok(s)
    }
}

fn store(cache: &Cache) -> Rc<dyn PluginStore> {
    Rc::new(LocalStore::new(cache.clone(), &ChannelId::new(postman_backend_matrix::ID), &AccountId::new(fake::ME)))
}

async fn until(rx: &mut UnboundedReceiver<ChannelEvent>, what: &str, stop: impl Fn(&ChannelEvent) -> bool) -> Vec<ChannelEvent> {
    let mut seen = Vec::new();
    loop {
        match tokio::time::timeout(WAIT, rx.recv()).await {
            Ok(Some(ev)) => {
                let done = stop(&ev);
                seen.push(ev);
                if done {
                    return seen;
                }
            }
            other => panic!("{what}: {other:?}, seen {seen:?}"),
        }
    }
}

/// Ответ браузера после входа на сервере: GET на адрес возврата с `loginToken`.
async fn browser_returns(url: &str, token: &str) {
    let redirect = url.split("redirectUrl=").nth(1).unwrap();
    let redirect = postman_core::channel::decode_segment(redirect.split('&').next().unwrap()).unwrap();
    let addr = redirect.trim_start_matches("http://").trim_end_matches('/').to_owned();
    let mut sock = tokio::net::TcpStream::connect(addr).await.unwrap();
    sock.write_all(format!("GET /?loginToken={token} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").as_bytes()).await.unwrap();
    let mut page = String::new();
    sock.read_to_string(&mut page).await.unwrap();
    assert!(page.starts_with("HTTP/1.1 200") && page.contains("Postman"), "{page}");
}

/// Канал Matrix — настоящим процессом: вход (пароль и браузер), подключение, догон, хранилище в базе хоста.
#[test]
fn matrix_channel_runs_as_a_process() {
    if cfg!(windows) {
        // Сервер фейка — loopback; изоляция loopback у AppContainer не даёт плагину подключиться к нему.
        eprintln!("Windows: изоляция loopback в AppContainer — сетевая часть теста процесса пропущена");
        return;
    }
    let server = FakeMatrix::new();
    let exe = Path::new(env!("CARGO_BIN_EXE_postman-plugins-matrix"));
    let last: Kill = Arc::new(Mutex::new(None));
    let launcher =
        Killable { inner: ProcLauncher { dir: exe.parent().unwrap().to_owned(), namespaces: Namespaces::Optional }, last: Arc::clone(&last) };
    let host = Host::new(Arc::new(launcher), Settings { log: "info".into(), ..Settings::default() });
    let plugin = host.channel(ROW);
    assert_eq!(host.launches(), 0, "заместитель без запроса процесс не создаёт");
    host.first_frame();

    let cache = Cache::open_in_memory().unwrap();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    tokio::task::LocalSet::new().block_on(&rt, async {
        // Вход паролем: шаги — в процессе плагина, секрет — ответом.
        let mut flow = plugin.sign_in(FlowCtx { store: store(&cache), sso: None }).await.unwrap();
        assert_eq!(flow.step(None).await.unwrap(), Prompt::Server);
        assert_eq!(flow.step(Some(Answer::Text(server.url()))).await.unwrap(), Prompt::Login);
        assert_eq!(flow.step(Some(Answer::Text(fake::LOGIN.into()))).await.unwrap(), Prompt::Password { hint: None });
        let Prompt::Done { account, secret, .. } = flow.step(Some(Answer::Secret(Secret::new(fake::PASSWORD.as_bytes().to_vec())))).await.unwrap()
        else {
            panic!()
        };
        assert_eq!(account.as_str(), fake::ME);
        drop(flow);
        assert_eq!(host.launches(), 1);
        assert!(remote::start_ms().is_some_and(|ms| ms > 0.0));

        // Подключение: секрет — в `Open`, состояние и ключи — услугой хранилища в базе хоста (файлов плагин не пишет).
        let (tx, mut rx) = unbounded_channel();
        let ctx = ConnectCtx { account: account.clone(), secret, incoming: tx, store: store(&cache) };
        let session = plugin.connect(ctx).await.unwrap();
        session.catch_up(None).await.unwrap();
        let seen = until(&mut rx, "catch_up", |e| matches!(e, ChannelEvent::Cursor(_))).await;
        assert!(seen.iter().any(|e| matches!(e, ChannelEvent::Conversations(c) if c.iter().any(|c| c.id.0 == fake::ROOM_TEAM))), "{seen:?}");
        // Запись устройства идёт очередью в своём потоке плагина: ждём её в базе ядра — файлы плагин не пишет.
        let deadline = std::time::Instant::now() + WAIT;
        while store(&cache).get("mx/device").unwrap().is_none() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(store(&cache).get("mx/device").unwrap().is_some(), "хранилище плагина — услуга ядра в базе хоста");
        drop(session);

        // Вход через браузер: порт возврата и токен — услугой ядра (песочница входящих не принимает).
        let mut flow = plugin.sign_in(FlowCtx { store: store(&cache), sso: None }).await.unwrap();
        flow.step(None).await.unwrap();
        flow.step(Some(Answer::Text(server.url()))).await.unwrap();
        let Prompt::Browser { url } = flow.step(Some(Answer::Confirm(true))).await.unwrap() else { panic!() };
        assert!(url.contains("redirectUrl=http%3A%2F%2F127.0.0.1"), "{url}");
        let (done, ()) = tokio::join!(flow.step(Some(Answer::Confirm(true))), browser_returns(&url, fake::LOGIN_TOKEN));
        assert!(matches!(done, Ok(Prompt::Done { .. })), "{done:?}");

        // Память процесса для `--bench`: сторож хоста спрашивает её после `Hello`.
        let deadline = std::time::Instant::now() + WAIT;
        while remote::plugins_rss() == 0 && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(remote::plugins_rss() > 0, "plugins_rss_mb: ОС не дала рабочий набор процесса плагина");
        eprintln!("PLG04-RSS: {} bytes", remote::plugins_rss());

        // Процесс убит: сессии — «без связи»; хост поднимает процесс снова.
        (last.lock().unwrap().as_mut().unwrap())();
        let (tx, mut rx) = unbounded_channel();
        // Событие «без связи» придёт сессии, которая ещё открыта: подключаем и рвем.
        let ctx = ConnectCtx { account: account.clone(), secret: fake::session(&server), incoming: tx, store: store(&cache) };
        let session = plugin.connect(ctx).await.unwrap();
        (last.lock().unwrap().as_mut().unwrap())();
        until(&mut rx, "offline", |e| matches!(e, ChannelEvent::State(postman_core::channel::ChannelState::Offline { .. }))).await;
        drop(session);
        let (tx, _rx) = unbounded_channel();
        let ctx = ConnectCtx { account: account.clone(), secret: fake::session(&server), incoming: tx, store: store(&cache) };
        let again = tokio::time::timeout(WAIT, plugin.connect(ctx)).await.unwrap().unwrap();
        assert!(host.launches() >= 2, "перезапуск после паузы");
        drop(again);

        // Вопросы описания — у поднятого процесса.
        assert!(plugin.parse_link("https://matrix.to/#/!team:example.test/$ev1").is_some());
        let missing =
            plugin.connect(ConnectCtx { account, secret: Secret::new(b"x".to_vec()), incoming: unbounded_channel().0, store: store(&cache) }).await;
        assert!(matches!(missing, Err(ChannelError::SignedOut(_))));
    });
}

/// Набор не запускает неизвестный плагин; строка корня — как мнение набора.
#[test]
fn unknown_plugin_is_refused() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_postman-plugins-matrix")).arg("--plugin=nope").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown plugin"));
    let entry = postman_plugins_matrix::find(postman_backend_matrix::ID).unwrap();
    assert_eq!((entry.kind, entry.profile), (ROW.kind, ROW.profile));
    assert!(entry.claims("postman://matrix/@a:b/c") && !entry.claims("postman://demo/a/b"));
}
