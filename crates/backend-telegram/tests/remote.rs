//! Канал Telegram через обмен (ADR 0013, PLG-05): заместитель хоста и сторона плагина (точка входа набора
//! `postman_backend_telegram::serve`) говорят сообщениями `wire` по кадрам `postman-proc`, плагин — в потоке этого
//! теста (настоящий процесс — `crates/plugins/tests/telegram.rs`). Сервер — подставной (`fake`) в памяти, сети нет
//! вовсе. Проверки: вход по коду и паролю двухэтапной проверки и по QR (`Prompt`/`Answer` по обмену), договор каналов
//! (`conformance`), вопросы описания у поднятого «процесса», отказ без ключа приложения (`Hello` сообщает его
//! отсутствие).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // интеграционный тест: «не падать» — для кода приложения (S2-03)

use std::io::pipe;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use postman_backend_telegram::fake::{self, FakeTelegram};
use postman_backend_telegram::wire::Dial;
use postman_backend_telegram::{NO_API_KEY, TelegramChannel, serve};
use postman_cache::Cache;
use postman_core::backend::remote::{self, Claim, Host, Launcher, Profile, Row, Started};
use postman_core::backend::wire::{Kind, Settings};
use postman_core::channel::conformance::{Harness, Script, run_all};
use postman_core::channel::{
    AccountId, Answer, ChannelError, ChannelEvent, ChannelId, ChannelPlugin, ConnectCtx, FlowCtx, LocalStore, Participant, ParticipantId,
    PluginStore, Prompt, Secret,
};
use postman_proc::link::Link;
use tokio::sync::mpsc::unbounded_channel;

const ROW: Row = Row {
    id: postman_backend_telegram::ID,
    kind: Kind::Channel,
    claim: Claim::Channel,
    set: "postman-plugins",
    profile: Profile::Network { enterprise_auth: false },
};

/// Сторона плагина — поток теста с точкой входа набора и подставным сервером (`serve` настоящей сборки без ключа —
/// в тесте процесса набора).
struct InThread {
    dial: Arc<dyn Dial>,
}

impl Launcher for InThread {
    fn launch(&self, _row: &Row) -> std::io::Result<Started> {
        let (from_plugin, plugin_out) = pipe()?;
        let (plugin_in, to_plugin) = pipe()?;
        let dial = Arc::clone(&self.dial);
        std::thread::spawn(move || {
            let link = Link::new(plugin_in, plugin_out, remote::MAX_BIN).unwrap();
            let make = move || Arc::new(TelegramChannel::with_dial(Arc::clone(&dial))) as Arc<dyn ChannelPlugin>;
            postman_core::backend::remote::serve_channel(link, postman_backend_telegram::ID, true, make)
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
    Rc::new(LocalStore::new(cache.clone(), &ChannelId::new(postman_backend_telegram::ID), &AccountId::new(fake::ACCOUNT)))
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

/// Проверки договора каналов и вход по коду и паролю — через обмен; поднятый «процесс» отвечает и на вопросы
/// описания.
#[test]
fn telegram_channel_over_the_wire() {
    let server = FakeTelegram::new();
    let host = Host::new(Arc::new(InThread { dial: Arc::new(server.clone()) }), Settings::default());
    host.first_frame();
    let plugin: Arc<dyn ChannelPlugin> = Arc::new(host.channel(ROW));
    assert!(host.channel(ROW).has_key(), "Hello сообщил ключ (заместитель настоящей сборки сказал бы факт)");
    let script: Rc<dyn Script> = Rc::new(server.clone());
    let h = Harness {
        plugin: plugin.clone(),
        account: AccountId::new(fake::ACCOUNT),
        secret: fake::session(),
        sign_in: Some(vec![
            Answer::Text(fake::PHONE.into()),
            Answer::Text(fake::CODE.into()),
            Answer::Secret(Secret::new(fake::PASSWORD.as_bytes().to_vec())),
        ]),
        script: Some(script),
        wait: Duration::from_secs(10),
    };
    let report = run_all(&h);
    assert!(report.ok(), "{report}");
    assert_eq!(report.skipped, ["features_are_honest"], "{report}");
    assert_eq!(host.launches(), 1, "один процесс на вход, догон и действия");
}

/// Вход по QR с другого устройства — вопросы `Prompt::Qr`/`Answer::Confirm` по обмену; отсканирован — `Done`.
#[test]
fn qr_sign_in_over_the_wire() {
    let server = FakeTelegram::new();
    let host = Host::new(Arc::new(InThread { dial: Arc::new(server.clone()) }), Settings::default());
    host.first_frame();
    let plugin: Arc<dyn ChannelPlugin> = Arc::new(host.channel(ROW));
    let cache = Cache::open_in_memory().unwrap();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    tokio::task::LocalSet::new().block_on(&rt, async {
        let mut flow = plugin.sign_in(FlowCtx { store: store(&cache), sso: None }).await.unwrap();
        assert_eq!(flow.step(None).await.unwrap(), Prompt::Phone);
        let Prompt::Qr { url, expires } = flow.step(Some(Answer::Confirm(true))).await.unwrap() else { panic!() };
        assert!(url.starts_with("tg://login?token=") && expires > 0, "{url}");
        server.scan_qr();
        let done = flow.step(Some(Answer::Confirm(true))).await.unwrap();
        assert!(matches!(done, Prompt::Done { ref account, .. } if account.as_str() == fake::ACCOUNT), "{done:?}");
        drop(flow);

        // Подключение: секрет — в `Open`, состояние плагина — услугой хранилища в базе ядра.
        let (tx, mut rx) = unbounded_channel();
        let ctx = ConnectCtx { account: AccountId::new(fake::ACCOUNT), secret: fake::session(), incoming: tx, store: store(&cache) };
        let session = plugin.connect(ctx).await.unwrap();
        session.catch_up(None).await.unwrap();
        let evs = until(&mut rx, |e| matches!(e, ChannelEvent::Cursor(_))).await;
        assert!(evs.iter().any(|e| matches!(e, ChannelEvent::Conversations(c) if c.iter().any(|c| c.id.0 == format!("c{}", fake::TEAM)))), "{evs:?}");
        drop(session);

        // Вопросы описания — у поднятого процесса: адреса по ссылкам канала, ссылка наружу, человек для контакта.
        let at = plugin.parse_link("https://t.me/c/123/45").unwrap();
        assert_eq!(at.to_string(), format!("postman://telegram/{}/c123/45", fake::ACCOUNT));
        assert_eq!(plugin.link(&at).as_deref(), Some("https://t.me/c/123/45"));
        let anna = Participant { id: ParticipantId::new("u1"), handle: Some("anna_example".into()), ..Participant::default() };
        assert_eq!(plugin.person_url(&anna).as_deref(), Some("https://t.me/anna_example"));

        // Память процесса для `--bench`: сторож хоста спрашивает её после `Hello`.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while remote::plugins_rss() == 0 && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(remote::plugins_rss() > 0, "plugins_rss_mb: ОС не дала рабочий набор процесса плагина");
    });
}

/// Точка входа настоящей сборки набора без ключа приложения: `Hello` сообщает отсутствие, вход и подключение —
/// отказ «нет ключа» тем же кодом, что и без обмена. Сборке дан ключ (`POSTMAN_TG_API_*`) — проверка пропускается:
/// настоящий набор вышел бы в сеть.
#[test]
fn without_a_key_the_set_says_so_over_the_wire() {
    if postman_backend_telegram::api_key().is_some() {
        eprintln!("сборка с ключом приложения: отказ «нет ключа» не проверить без сети");
        return;
    }
    let host = Host::new(Arc::new(NoKey), Settings::default());
    host.first_frame();
    let plugin = host.channel(ROW);
    let cache = Cache::open_in_memory().unwrap();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    tokio::task::LocalSet::new().block_on(&rt, async move {
        // Вход — отказ: запуск только отвечает `Hello` (без ключа) и отказывает.
        let err = plugin.sign_in(FlowCtx { store: store(&cache), sso: None }).await.err().unwrap();
        assert!(matches!(err, ChannelError::Denied(ref c) if c == NO_API_KEY), "{err:?}");
        assert!(!plugin.has_key(), "Hello сказал: ключа нет");
        // Подключение — тот же отказ.
        let ctx =
            ConnectCtx { account: AccountId::new(fake::ACCOUNT), secret: fake::session(), incoming: unbounded_channel().0, store: store(&cache) };
        let err = plugin.connect(ctx).await.err().unwrap();
        assert!(matches!(err, ChannelError::Denied(ref c) if c == NO_API_KEY), "{err:?}");
        // `TelegramChannel::new()` без обмена — то же мнение о ключе.
        assert!(!TelegramChannel::new().has_key());
    });
}

/// Сторона плагина — точка входа набора как есть (канал этой сборки).
struct NoKey;

impl Launcher for NoKey {
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
