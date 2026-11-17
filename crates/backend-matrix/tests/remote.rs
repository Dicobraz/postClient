//! Канал Matrix через обмен (ADR 0013, PLG-04): заместитель хоста и сторона плагина (точка входа набора
//! `postman_backend_matrix::serve`) говорят сообщениями `wire` по кадрам `postman-proc`, плагин — в потоке этого
//! теста (настоящий процесс — `crates/plugins-matrix/tests`). Сервер — подставной (`fake`) на loopback, внешней сети
//! нет. Проверки: вход паролем и через браузер (SSO: loopback слушает ядро и передаёт токен плагину по обмену),
//! подключения, ссылки `parse_link`/`link`/`person_url`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // интеграционный тест: «не падать» — для кода приложения (S2-03)

use std::io::pipe;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use postman_backend_matrix::fake::{self, FakeMatrix};
use postman_backend_matrix::{MatrixChannel, serve};
use postman_cache::Cache;
use postman_core::backend::remote::{self, Claim, Host, Launcher, Profile, Row, Started};
use postman_core::backend::wire::{Kind, Settings};
use postman_core::channel::conformance::{Harness, Script, run_all};
use postman_core::channel::{
    AccountId, Answer, ChannelError, ChannelEvent, ChannelId, ChannelPlugin, ConnectCtx, FlowCtx, LocalStore, Participant, ParticipantId,
    PluginStore, Prompt, Secret,
};
use postman_proc::link::Link;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::sync::mpsc::unbounded_channel;

const ROW: Row = Row {
    id: postman_backend_matrix::ID,
    kind: Kind::Channel,
    claim: Claim::Channel,
    set: "postman-plugins-matrix",
    profile: Profile::Network { enterprise_auth: false },
};

/// Сторона плагина — поток теста с той же точкой входа, что у файла набора.
struct InThread;

impl Launcher for InThread {
    fn launch(&self, _row: &Row) -> std::io::Result<Started> {
        let (from_plugin, plugin_out) = pipe()?;
        let (plugin_in, to_plugin) = pipe()?;
        std::thread::spawn(move || {
            let link = Link::new(plugin_in, plugin_out, remote::MAX_BIN).unwrap();
            serve(link)
        });
        Ok(Started {
            link: Link::new(from_plugin, to_plugin, remote::MAX_BIN)?,
            kill: Box::new(|| {}),
            exit_code: Box::new(|| None),
            background: Box::new(|_| Ok(())),
        })
    }
}

fn store(cache: &Cache) -> Rc<dyn PluginStore> {
    Rc::new(LocalStore::new(cache.clone(), &ChannelId::new(postman_backend_matrix::ID), &AccountId::new(fake::ME)))
}

async fn until(rx: &mut tokio::sync::mpsc::UnboundedReceiver<ChannelEvent>, mut f: impl FnMut(&ChannelEvent) -> bool) -> Vec<ChannelEvent> {
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

/// Проверки договора каналов, вход через браузер (SSO — по обмену) и вопросы описания — у поднятого процесса.
#[test]
fn matrix_channel_over_the_wire() {
    let server = FakeMatrix::new();
    let host = Host::new(Arc::new(InThread), Settings::default());
    host.first_frame();
    let plugin: Arc<dyn ChannelPlugin> = Arc::new(host.channel(ROW));
    let script: Rc<dyn Script> = Rc::new(server.clone());
    let h = Harness {
        plugin: plugin.clone(),
        account: AccountId::new(fake::ME),
        secret: fake::session(&server),
        sign_in: Some(vec![
            Answer::Text(server.url()),
            Answer::Text(fake::LOGIN.into()),
            Answer::Secret(Secret::new(fake::PASSWORD.as_bytes().to_vec())),
        ]),
        script: Some(script),
        wait: Duration::from_secs(10),
    };
    let report = run_all(&h);
    assert!(report.ok(), "{report}");
    assert_eq!(report.skipped, ["script_delete_without_conversation"], "{report}");
    assert_eq!(host.launches(), 1, "один процесс на вход, догон и действия");

    let cache = Cache::open_in_memory().unwrap();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    tokio::task::LocalSet::new().block_on(&rt, async {
        // Новый сервер: проверки заканчиваются выходом (logout отзывает токен прошлого сервера).
        let server = FakeMatrix::new();

        // Вход через браузер: порт возврата и токен — услугой ядра по обмену (ADR 0013: песочница входящих не принимает).
        let mut flow = plugin.sign_in(FlowCtx { store: store(&cache), sso: None }).await.unwrap();
        assert_eq!(flow.step(None).await.unwrap(), Prompt::Server);
        flow.step(Some(Answer::Text(server.url()))).await.unwrap();
        let Prompt::Browser { url } = flow.step(Some(Answer::Confirm(true))).await.unwrap() else { panic!() };
        assert!(url.contains("/login/sso/redirect") && url.contains("redirectUrl=http%3A%2F%2F127.0.0.1"), "{url}");
        let (done, ()) = tokio::join!(flow.step(Some(Answer::Confirm(true))), browser_returns(&url, fake::LOGIN_TOKEN));
        assert!(matches!(done, Ok(Prompt::Done { ref account, .. }) if account.as_str() == fake::ME), "{done:?}");
        drop(flow);

        // Подключение: секрет — в `Open`, состояние и ключи шифрования — услугой хранилища в базе ядра.
        let (tx, mut rx) = unbounded_channel();
        let ctx = ConnectCtx { account: AccountId::new(fake::ME), secret: fake::session(&server), incoming: tx, store: store(&cache) };
        let session = plugin.connect(ctx).await.unwrap();
        session.catch_up(None).await.unwrap();
        let evs = until(&mut rx, |e| matches!(e, ChannelEvent::Cursor(_))).await;
        assert!(evs.iter().any(|e| matches!(e, ChannelEvent::Conversations(c) if c.iter().any(|c| c.id.0 == fake::ROOM_TEAM))), "{evs:?}");
        drop(session);

        // Вопросы описания — у поднятого процесса: адреса по ссылкам канала, ссылка наружу, человек для контакта.
        let at = plugin.parse_link("https://matrix.to/#/!team:example.test/$ev1?via=example.test").unwrap();
        assert_eq!(at.to_string(), format!("postman://matrix/{}/!team:example.test/$ev1", fake::ME));
        assert_eq!(plugin.link(&at).as_deref(), Some("https://matrix.to/#/!team:example.test/$ev1"));
        let anna = Participant { id: ParticipantId::new("@anna:example.test"), ..Participant::default() };
        assert_eq!(plugin.person_url(&anna).as_deref(), Some("matrix:u/anna:example.test"));

        // Память процесса для `--bench`: сторож хоста спрашивает её после `Hello`.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while remote::plugins_rss() == 0 && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(remote::plugins_rss() > 0, "plugins_rss_mb: ОС не дала рабочий набор процесса плагина");
    });
}

/// Неверный секрет — отказ подключения тем же кодом, что и без обмена.
#[test]
fn bad_secret_is_signed_out_over_the_wire() {
    let host = Host::new(Arc::new(InThread), Settings::default());
    host.first_frame();
    let plugin: Arc<dyn ChannelPlugin> = Arc::new(host.channel(ROW));
    let cache = Cache::open_in_memory().unwrap();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    tokio::task::LocalSet::new().block_on(&rt, async {
        let ctx = ConnectCtx {
            account: AccountId::new(fake::ME),
            secret: Secret::new(b"not a session".to_vec()),
            incoming: unbounded_channel().0,
            store: store(&cache),
        };
        let err = plugin.connect(ctx).await.err().unwrap();
        assert!(matches!(err, ChannelError::SignedOut(_)), "{err:?}");
        // `MatrixChannel::new()` без обмена — то же мнение о секрете.
        let local = MatrixChannel::new();
        let ctx = ConnectCtx {
            account: AccountId::new(fake::ME),
            secret: Secret::new(b"not a session".to_vec()),
            incoming: unbounded_channel().0,
            store: store(&cache),
        };
        assert!(matches!(local.connect(ctx).await, Err(ChannelError::SignedOut(_))));
    });
}
