//! Демо-канал настоящим процессом `postman-plugins --plugin=demo` в песочнице «сеть без файлов» (ADR 0013, PLG-02):
//! `Hello` со сверкой сборки, вход, подключение, догон, отправка, хранилище плагина — в базе хоста услугой ядра;
//! процесс убит — сессия «без связи», хост поднимает его снова.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // интеграционный тест: «не падать» — для кода приложения (S2-03)

use std::path::Path;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use postman_cache::Cache;
use postman_core::backend::remote::{self, Claim, Host, Launcher, Namespaces, ProcLauncher, Profile, Row, Started};
use postman_core::backend::wire::{Kind, Settings};
use postman_core::channel::{
    AccountId, Action, Answer, ChannelEvent, ChannelId, ChannelPlugin, ChannelState, ConnectCtx, ConversationId, Done, FlowCtx, LocalStore, Outgoing,
    PluginStore, Prompt, Secret, text_doc,
};
use postman_demo::channel::{ACCOUNT, CODE, DIRECT, ID, PASSWORD, SESSION};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

const ROW: Row =
    Row { id: ID, kind: Kind::Channel, claim: Claim::Channel, set: "postman-plugins", profile: Profile::Network { enterprise_auth: false } };
const WAIT: Duration = Duration::from_secs(20);

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

#[test]
fn demo_channel_runs_as_a_process() {
    let exe = Path::new(env!("CARGO_BIN_EXE_postman-plugins"));
    let last: Kill = Arc::new(Mutex::new(None));
    let launcher =
        Killable { inner: ProcLauncher { dir: exe.parent().unwrap().to_owned(), namespaces: Namespaces::Optional }, last: Arc::clone(&last) };
    let host = Host::new(Arc::new(launcher), Settings { log: "debug".into(), ..Settings::default() });
    let plugin = host.channel(ROW);
    assert_eq!(host.launches(), 0, "заместитель без запроса процесса не создаёт");
    host.first_frame();

    let cache = Cache::open_in_memory().unwrap();
    let account = AccountId::new(ACCOUNT);
    let store = || -> Rc<dyn PluginStore> { Rc::new(LocalStore::new(cache.clone(), &ChannelId::new(ID), &account)) };
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    tokio::task::LocalSet::new().block_on(&rt, async {
        // Вход — шаги в процессе плагина, секрет — ответом.
        let mut flow = plugin.sign_in(FlowCtx { store: store(), sso: None }).await.unwrap();
        assert_eq!(flow.step(None).await.unwrap(), Prompt::Phone);
        assert!(matches!(flow.step(Some(Answer::Text("+70000000000".into()))).await.unwrap(), Prompt::Code { .. }));
        assert!(matches!(flow.step(Some(Answer::Text(CODE.into()))).await.unwrap(), Prompt::Password { .. }));
        let done = flow.step(Some(Answer::Secret(Secret::new(PASSWORD.to_vec())))).await.unwrap();
        assert!(matches!(done, Prompt::Done { ref secret, .. } if secret.expose() == SESSION), "{done:?}");
        drop(flow);
        assert_eq!(host.launches(), 1);
        assert!(remote::start_ms().is_some_and(|ms| ms > 0.0));

        // Подключение: секрет — в `Open`, состояние плагина — в базе хоста (файлов плагин не пишет).
        let (tx, mut rx) = unbounded_channel();
        let ctx = ConnectCtx { account: account.clone(), secret: Secret::new(SESSION.to_vec()), incoming: tx, store: store() };
        let session = plugin.connect(ctx).await.unwrap();
        assert_eq!(session.me().emails, [ACCOUNT]);
        assert_eq!(store().get("state/connections").unwrap().as_deref(), Some(&b"1"[..]));
        let bad = ConnectCtx { account: account.clone(), secret: Secret::new(b"x".to_vec()), incoming: unbounded_channel().0, store: store() };
        assert!(matches!(plugin.connect(bad).await.map(drop), Err(postman_core::channel::ChannelError::SignedOut(_))));

        session.catch_up(None).await.unwrap();
        let seen = until(&mut rx, "catch_up", |e| matches!(e, ChannelEvent::Cursor(_))).await;
        assert!(seen.iter().any(|e| matches!(e, ChannelEvent::Conversations(c) if c.len() == 3)), "{seen:?}");
        let out = Outgoing {
            conversation: ConversationId::new(DIRECT),
            client_id: 7,
            body: text_doc("через процесс"),
            attachments: Vec::new(),
            reply_to: None,
            quotes: Vec::new(),
            mentions: Vec::new(),
            at: None,
            silent: false,
        };
        assert!(matches!(session.act(Action::Send(out)).await.unwrap(), Done::Sent { client_id: 7, .. }));
        assert!(plugin.parse_link("https://chat.example.test/g-team").is_some(), "вопросы описания — у поднятого процесса");
        // Память процесса для `--bench`: сторож хоста спрашивает её сразу после `Hello`.
        let deadline = std::time::Instant::now() + WAIT;
        while remote::plugins_rss() == 0 && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(remote::plugins_rss() > 0, "plugins_rss_mb: ОС не дала рабочий набор процесса плагина");

        // Процесс убит: сессия — «без связи», ожидающих нет; хост поднимает процесс снова.
        (last.lock().unwrap().as_mut().unwrap())();
        until(&mut rx, "offline", |e| matches!(e, ChannelEvent::State(ChannelState::Offline { .. }))).await;
        assert!(session.catch_up(None).await.is_err());
        drop(session);
        let (tx, _rx) = unbounded_channel();
        let ctx = ConnectCtx { account: account.clone(), secret: Secret::new(SESSION.to_vec()), incoming: tx, store: store() };
        let again = tokio::time::timeout(WAIT, plugin.connect(ctx)).await.unwrap().unwrap();
        assert_eq!(host.launches(), 2, "перезапуск после паузы");
        assert_eq!(store().get("state/connections").unwrap().as_deref(), Some(&b"1"[..]), "новый процесс — новый сервер демо");
        drop(again);
    });
}

/// Набор не запускает неизвестный плагин; профиль — из набора, не из аргумента.
#[test]
fn unknown_plugin_is_refused() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_postman-plugins")).arg("--plugin=nope").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown plugin"));
    let entry = postman_plugins::find(ID).unwrap();
    assert_eq!((entry.kind, entry.profile), (ROW.kind, ROW.profile));
    assert!(entry.claims("postman://demo/a/b") && !entry.claims("postman://telegram/a/b"));
}
