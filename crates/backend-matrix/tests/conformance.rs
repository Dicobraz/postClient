//! Канал Matrix проходит проверки договора каналов (ADR 0012, `postman_core::channel::conformance`) на подставном
//! сервере (`postman_backend_matrix::fake`): Client-Server API на loopback, без сети. Плюс то, чего общий набор не
//! видит: вход через браузер (SSO, возврат на loopback), неверный пароль, пространства — папками, ответы и
//! упоминания, своё хранилище переживает переподключение.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // интеграционный тест: «не падать» — для кода приложения (S2-03)

use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use postman_backend_matrix::fake::{self, FakeMatrix};
use postman_backend_matrix::{BAD_PASSWORD, MatrixChannel};
use postman_cache::Cache;
use postman_core::backend::BoxFuture;
use postman_core::channel::conformance::{Harness, Script, run_all};
use postman_core::channel::{
    AccountId, Answer, ChannelError, ChannelEvent, ChannelId, ChannelPlugin, ChannelSession, ConnectCtx, ConversationKind, FlowCtx, LocalStore,
    Prompt, Secret, Sso,
};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

fn harness(server: FakeMatrix) -> Harness {
    let script: Rc<dyn Script> = Rc::new(server.clone());
    Harness {
        plugin: Arc::new(MatrixChannel::new()),
        account: AccountId::new(fake::ME),
        secret: fake::session(&server),
        sign_in: Some(vec![
            Answer::Text(server.url()),
            Answer::Text(fake::LOGIN.into()),
            Answer::Secret(Secret::new(fake::PASSWORD.as_bytes().to_vec())),
        ]),
        script: Some(script),
        wait: Duration::from_secs(10),
    }
}

#[test]
fn matrix_passes_conformance() {
    let report = run_all(&harness(FakeMatrix::new()));
    assert!(report.ok(), "{report}");
    // Удаления без беседы у Matrix нет — его проверка не к этому каналу.
    assert_eq!(report.skipped, ["script_delete_without_conversation"], "{report}");
    assert!(report.passed.len() >= 24, "{report}");
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
}

fn store(cache: &Cache) -> Rc<dyn postman_core::channel::PluginStore> {
    Rc::new(LocalStore::new(cache.clone(), &ChannelId::new("matrix"), &AccountId::new(fake::ME)))
}

/// Возврат SSO в тесте без процесса: слушает сам тест — ядро даёт плагину ту же услугу по обмену (`FlowCtx::sso`,
/// ADR 0013), в песочнице плагин входящих не принимает.
struct Loopback(std::net::TcpListener);

impl Loopback {
    fn new() -> Self {
        Self(std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap())
    }
}

impl Sso for Loopback {
    fn port(&self) -> Result<u16, ChannelError> {
        self.0.local_addr().map(|a| a.port()).map_err(|e| ChannelError::Storage(e.to_string()))
    }

    fn token<'a>(&'a self) -> BoxFuture<'a, Result<String, ChannelError>> {
        const WAIT: Duration = Duration::from_secs(5);
        const PAGE: &str = "<!doctype html><meta charset=utf-8><title>Postman</title>";
        let listener = self.0.try_clone().map_err(|e| ChannelError::Storage(e.to_string())).and_then(|l| {
            l.set_nonblocking(true).map_err(|e| ChannelError::Storage(e.to_string()))?;
            Ok(l)
        });
        Box::pin(async move {
            let listener = tokio::net::TcpListener::from_std(listener?).map_err(|e| ChannelError::Storage(e.to_string()))?;
            let wait = async {
                loop {
                    let (mut sock, _) = listener.accept().await.map_err(|e| ChannelError::Network(e.to_string()))?;
                    let mut buf = vec![0u8; 8192];
                    let n = match tokio::time::timeout(Duration::from_secs(10), sock.read(&mut buf)).await {
                        Ok(Ok(n)) => n,
                        _ => 0,
                    };
                    let head = String::from_utf8_lossy(&buf[..n]);
                    let token = head.lines().next().and_then(|l| l.split(' ').nth(1)).and_then(|path| {
                        path.split_once('?')?.1.split('&').find_map(|kv| kv.strip_prefix("loginToken=")).and_then(postman_core::channel::decode_segment)
                    });
                    let reply = match &token {
                        Some(_) => format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{PAGE}", PAGE.len()),
                        None => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned(),
                    };
                    let _ = sock.write_all(reply.as_bytes()).await;
                    if let Some(t) = token.filter(|t| !t.is_empty()) {
                        return Ok(t);
                    }
                }
            };
            tokio::time::timeout(WAIT, wait).await.map_err(|_| ChannelError::Denied(postman_core::channel::matrix::BROWSER_TIMEOUT.into()))?
        })
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

/// Вход: неверный пароль — отказ с кодом, шаг тот же; затем вход паролем; вход через браузер — по токену возврата.
#[test]
fn sign_in_password_and_browser() {
    let server = FakeMatrix::new();
    let plugin = MatrixChannel::new();
    let cache = Cache::open_in_memory().unwrap();
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt(), async {
        let mut flow = plugin.sign_in(FlowCtx { store: store(&cache), sso: None }).await.unwrap();
        assert_eq!(flow.step(None).await.unwrap(), Prompt::Server);
        assert!(matches!(flow.step(Some(Answer::Text("not a server".into()))).await, Err(ChannelError::Denied(_))));
        assert_eq!(flow.step(Some(Answer::Text(server.url()))).await.unwrap(), Prompt::Login);
        assert_eq!(flow.step(Some(Answer::Text(fake::LOGIN.into()))).await.unwrap(), Prompt::Password { hint: None });
        let wrong = flow.step(Some(Answer::Secret(Secret::new(b"not it".to_vec())))).await;
        assert!(matches!(wrong, Err(ChannelError::Denied(ref c)) if c == BAD_PASSWORD), "{wrong:?}");
        let Prompt::Done { account, secret, name } = flow.step(Some(Answer::Secret(Secret::new(fake::PASSWORD.as_bytes().to_vec())))).await.unwrap()
        else {
            panic!()
        };
        assert_eq!((account.as_str(), name.as_str()), (fake::ME, "ivan"));
        assert!(!format!("{secret:?}").contains("tok-"), "секрет без значения в журнале");
        drop(flow);

        // Браузер: ссылка SSO с возвратом на loopback (порт — у услуги SSO), возврат с токеном — вход.
        let sso = Rc::new(Loopback::new());
        let mut flow = plugin.sign_in(FlowCtx { store: store(&cache), sso: Some(sso) }).await.unwrap();
        flow.step(None).await.unwrap();
        flow.step(Some(Answer::Text(server.url()))).await.unwrap();
        let Prompt::Browser { url } = flow.step(Some(Answer::Confirm(true))).await.unwrap() else { panic!() };
        assert!(url.contains("/login/sso/redirect") && url.contains("redirectUrl=http%3A%2F%2F127.0.0.1"), "{url}");
        let (done, ()) = tokio::join!(flow.step(Some(Answer::Confirm(true))), browser_returns(&url, fake::LOGIN_TOKEN));
        assert!(matches!(done, Ok(Prompt::Done { ref account, .. }) if account.as_str() == fake::ME), "{done:?}");
    });
}

struct Conn {
    session: Box<dyn ChannelSession>,
    rx: UnboundedReceiver<ChannelEvent>,
}

async fn connect(plugin: &MatrixChannel, server: &FakeMatrix, cache: &Cache) -> Conn {
    let (tx, rx) = unbounded_channel();
    let ctx = ConnectCtx { account: AccountId::new(fake::ME), secret: fake::session(server), incoming: tx, store: store(cache) };
    Conn { session: plugin.connect(ctx).await.unwrap(), rx }
}

async fn until(rx: &mut UnboundedReceiver<ChannelEvent>, mut f: impl FnMut(&ChannelEvent) -> bool) -> Vec<ChannelEvent> {
    let mut out = Vec::new();
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(10), rx.recv()).await.expect("event in time").expect("events open");
        let stop = f(&ev);
        out.push(ev);
        if stop {
            return out;
        }
    }
}

/// Догон: личная беседа, группа с ролью, «только чтение», пространство — папкой; ответ, упоминание меня, HTML-тело;
/// живое сообщение — `live`; переподключение с той же базой — ключи устройства те же (своё хранилище в базе).
#[test]
fn catch_up_rooms_spaces_and_store() {
    let server = FakeMatrix::new();
    let plugin = MatrixChannel::new();
    let cache = Cache::open_in_memory().unwrap();
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt(), async {
        let mut c = connect(&plugin, &server, &cache).await;
        c.session.catch_up(None).await.unwrap();
        let evs = until(&mut c.rx, |e| matches!(e, ChannelEvent::Cursor(_))).await;
        let convs: Vec<_> =
            evs.iter().filter_map(|e| if let ChannelEvent::Conversations(cs) = e { Some(cs.clone()) } else { None }).flatten().collect();
        let anna = convs.iter().find(|c| c.id.0 == fake::ROOM_ANNA).unwrap();
        assert_eq!(anna.kind, ConversationKind::Direct);
        let team = convs.iter().find(|c| c.id.0 == fake::ROOM_TEAM).unwrap();
        assert_eq!((team.kind, team.my_role, team.title.as_str()), (ConversationKind::Group, postman_core::channel::Role::Admin, "Команда"));
        assert!(team.folders.iter().any(|f| f.0 == fake::SPACE), "пространство — папка: {:?}", team.folders);
        let news = convs.iter().find(|c| c.id.0 == fake::ROOM_NEWS).unwrap();
        assert!(!news.my_role.can_write(), "в «Новости» писать нельзя");
        assert!(!convs.iter().any(|c| c.id.0 == fake::SPACE), "пространство — не беседа");
        let folders = evs.iter().find_map(|e| if let ChannelEvent::Folders(f) = e { Some(f.clone()) } else { None }).unwrap();
        assert!(folders.iter().any(|f| f.name == "Работа" && f.conversations.len() == 2));
        let msgs: Vec<_> = evs
            .iter()
            .filter_map(|e| if let ChannelEvent::Messages { messages, .. } = e { Some(messages.clone()) } else { None })
            .flatten()
            .collect();
        let mention = msgs.iter().find(|m| m.body.plain().contains("посмотри отчёт")).unwrap();
        assert!(mention.mentions.iter().any(|p| p.0 == fake::ME), "упоминание меня");
        assert!(matches!(&mention.body, postman_core::channel::Body::Html { html, .. } if html.contains("<b>отчёт</b>")));
        let reply = msgs.iter().find(|m| m.body.plain() == "Буду").expect("запасная цитата ответа отрезана");
        assert!(reply.reply_to.is_some());
        assert!(evs.iter().any(|e| matches!(e, ChannelEvent::People(p) if p.iter().any(|x| x.name == "Анна Петрова"))));

        server.anna_writes("Ты тут?");
        let evs = until(&mut c.rx, |e| matches!(e, ChannelEvent::Messages { live: true, .. })).await;
        let Some(ChannelEvent::Messages { messages, .. }) = evs.last() else { panic!() };
        assert_eq!(messages[0].body.plain(), "Ты тут?");
        let keys = server.device_keys(fake::ME, fake::DEVICE).expect("ключи устройства выгружены");
        drop(c);
        // Дать задаче записи дописать в базу.
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Переподключение с той же базой: учётка Olm та же — ключи устройства не меняются.
        let mut again = connect(&plugin, &server, &cache).await;
        again.session.catch_up(None).await.unwrap();
        until(&mut again.rx, |e| matches!(e, ChannelEvent::Cursor(_))).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(server.device_keys(fake::ME, fake::DEVICE).unwrap()["keys"], keys["keys"], "ключи устройства — из своего хранилища");
    });
}

/// Потоки после входа без настроенной кросс-подписи и копии ключей: проверка устройства — понятный отказ; ключ
/// восстановления — вопрос, затем «не подошёл», шаг тот же.
#[test]
fn verify_and_recovery_refuse_clearly() {
    use postman_backend_matrix::{BAD_RECOVERY_KEY, NO_CROSS_SIGNING};
    use postman_core::channel::FlowKind;
    let server = FakeMatrix::new();
    let plugin = MatrixChannel::new();
    let cache = Cache::open_in_memory().unwrap();
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt(), async {
        let mut c = connect(&plugin, &server, &cache).await;
        c.session.catch_up(None).await.unwrap();
        until(&mut c.rx, |e| matches!(e, ChannelEvent::Cursor(_))).await;
        let mut verify = c.session.flow(FlowKind::Verify).await.unwrap();
        let r = verify.step(None).await;
        assert!(matches!(r, Err(ChannelError::Denied(ref code)) if code == NO_CROSS_SIGNING), "{r:?}");
        drop(verify);
        let mut recovery = c.session.flow(FlowKind::Recovery).await.unwrap();
        assert_eq!(recovery.step(None).await.unwrap(), Prompt::RecoveryKey);
        let r = recovery.step(Some(Answer::Secret(Secret::new(b"EsTc not a key".to_vec())))).await;
        assert!(matches!(r, Err(ChannelError::Denied(ref code)) if code == BAD_RECOVERY_KEY), "{r:?}");
    });
}
