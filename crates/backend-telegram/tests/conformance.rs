//! Канал Telegram проходит проверки договора каналов (ADR 0012, `postman_core::channel::conformance`) на подставном
//! сервере (`postman_backend_telegram::fake`): те же байты TL, без сети. Плюс то, чего общий набор не видит: вход по
//! QR, неверный код и пароль, догон от курсора после пропуска, просроченная ссылка на файл, папки и архив; альбомы
//! (отправка `sendMultiMedia`, приём `grouped_id`, поздняя часть, правка, удаление), кнопки бота и опрос в `extra`,
//! папки по правилам «без заглушённых / прочитанного / архива» на лету; нажатие кнопок бота и голос в опросе
//! (`Action::Extra`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // интеграционный тест: «не падать» — для кода приложения (S2-03)

use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use postman_backend_telegram::fake::{self, FakeTelegram};
use postman_backend_telegram::{BAD_CODE, BAD_PASSWORD, BAD_PHONE, TelegramChannel};
use postman_cache::Cache;
use postman_core::channel::conformance::{Harness, Script, run_all};
use postman_core::channel::{
    AccountId, Answer, ChannelError, ChannelEvent, ChannelId, ChannelPlugin, ChannelSession, ConnectCtx, ConversationId, FlowCtx, LocalStore, Prompt,
    Secret,
};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

fn harness(server: FakeTelegram) -> Harness {
    let script: Rc<dyn Script> = Rc::new(server.clone());
    Harness {
        plugin: Arc::new(TelegramChannel::with_dial(Arc::new(server))),
        account: AccountId::new(fake::ACCOUNT),
        secret: fake::session(),
        sign_in: Some(vec![
            Answer::Text(fake::PHONE.into()),
            Answer::Text(fake::CODE.into()),
            Answer::Secret(Secret::new(fake::PASSWORD.as_bytes().to_vec())),
        ]),
        script: Some(script),
        wait: Duration::from_secs(5),
    }
}

#[test]
fn telegram_passes_conformance() {
    let report = run_all(&harness(FakeTelegram::new()));
    assert!(report.ok(), "{report}");
    // Telegram умеет всё, что проверяется отказом: пропуск — только «честность возможностей».
    assert_eq!(report.skipped, ["features_are_honest"], "{report}");
    assert!(report.passed.len() >= 24, "{report}");
    assert!(report.passed.contains(&"extra"), "кнопки бота и голос — через Action::Extra: {report}");
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
}

fn store(cache: &Cache) -> Rc<dyn postman_core::channel::PluginStore> {
    Rc::new(LocalStore::new(cache.clone(), &ChannelId::new("telegram"), &AccountId::new(fake::ACCOUNT)))
}

/// Вход: неверный телефон, код, пароль — отказ с кодом, шаг тот же; затем вход; QR — с другого устройства.
#[test]
fn sign_in_errors_and_qr() {
    let server = FakeTelegram::new();
    let plugin = TelegramChannel::with_dial(Arc::new(server.clone()));
    let cache = Cache::open_in_memory().unwrap();
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt(), async {
        let mut flow = plugin.sign_in(FlowCtx { store: store(&cache), sso: None }).await.unwrap();
        assert_eq!(flow.step(None).await.unwrap(), Prompt::Phone);
        assert!(matches!(flow.step(Some(Answer::Text("+999 0".into()))).await, Err(ChannelError::Denied(c)) if c == BAD_PHONE));
        assert!(matches!(flow.step(Some(Answer::Text(fake::PHONE.into()))).await.unwrap(), Prompt::Code { length: Some(5), .. }));
        assert!(matches!(flow.step(Some(Answer::Text("00000".into()))).await, Err(ChannelError::Denied(c)) if c == BAD_CODE));
        assert!(matches!(flow.step(Some(Answer::Resend)).await.unwrap(), Prompt::Code { .. }));
        let p = flow.step(Some(Answer::Text(fake::CODE.into()))).await.unwrap();
        assert_eq!(p, Prompt::Password { hint: Some("подсказка".into()) });
        assert!(matches!(flow.step(Some(Answer::Text("не тот".into()))).await, Err(ChannelError::Denied(c)) if c == BAD_PASSWORD));
        let Prompt::Done { account, secret, name } = flow.step(Some(Answer::Text(fake::PASSWORD.into()))).await.unwrap() else { panic!() };
        assert_eq!((account.as_str(), name.as_str()), (fake::ACCOUNT, "Иван Тестов"));
        assert!(!format!("{secret:?}").contains("171"), "секрет без значения в журнале");
        drop(flow);

        // QR: ссылка tg://login, срок; не отсканирован — тот же QR; отсканирован — вход.
        let mut flow = plugin.sign_in(FlowCtx { store: store(&cache), sso: None }).await.unwrap();
        flow.step(None).await.unwrap();
        let Prompt::Qr { url, expires } = flow.step(Some(Answer::Confirm(true))).await.unwrap() else { panic!() };
        assert!(url.starts_with("tg://login?token=") && expires > 0, "{url}");
        server.scan_qr();
        let done = flow.step(Some(Answer::Confirm(true))).await.unwrap();
        assert!(matches!(done, Prompt::Done { ref account, .. } if account.as_str() == fake::ACCOUNT), "{done:?}");
    });
}

struct Conn {
    session: Box<dyn ChannelSession>,
    rx: UnboundedReceiver<ChannelEvent>,
}

async fn connect(plugin: &TelegramChannel, cache: &Cache) -> Conn {
    let (tx, rx) = unbounded_channel();
    let ctx = ConnectCtx { account: AccountId::new(fake::ACCOUNT), secret: fake::session(), incoming: tx, store: store(cache) };
    Conn { session: plugin.connect(ctx).await.unwrap(), rx }
}

async fn until(rx: &mut UnboundedReceiver<ChannelEvent>, mut f: impl FnMut(&ChannelEvent) -> bool) -> Vec<ChannelEvent> {
    let mut out = Vec::new();
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.expect("event in time").expect("events open");
        let stop = f(&ev);
        out.push(ev);
        if stop {
            return out;
        }
    }
}

/// Догон: беседы с ролями, счётчиками, закреплением; папка «Работа» и архив; упоминание меня и ответ с цитатой; от
/// курсора — пропущенное пришло; живое сообщение — `live`, счётчик растёт.
#[test]
fn catch_up_folders_and_live() {
    let server = FakeTelegram::new();
    let plugin = TelegramChannel::with_dial(Arc::new(server.clone()));
    let cache = Cache::open_in_memory().unwrap();
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt(), async {
        let mut c = connect(&plugin, &cache).await;
        c.session.catch_up(None).await.unwrap();
        let evs = until(&mut c.rx, |e| matches!(e, ChannelEvent::Cursor(_))).await;
        let convs: Vec<_> =
            evs.iter().filter_map(|e| if let ChannelEvent::Conversations(cs) = e { Some(cs.clone()) } else { None }).flatten().collect();
        let team = convs.iter().find(|c| c.id.0 == format!("c{}", fake::TEAM)).unwrap();
        assert!(team.pinned && team.unread.unwrap().mentions == 1);
        assert!(team.folders.iter().any(|f| f.0 == "f2"), "папка «Работа»: {:?}", team.folders);
        let news = convs.iter().find(|c| c.id.0 == format!("c{}", fake::NEWS)).unwrap();
        assert_eq!(news.kind, postman_core::channel::ConversationKind::Broadcast);
        assert!(!news.my_role.can_write());
        assert!(news.folders.iter().any(|f| f.0 == "archive"));
        let anna = convs.iter().find(|c| c.id.0 == format!("u{}", fake::ANNA)).unwrap();
        assert!(anna.folders.iter().any(|f| f.0 == "f2"), "контакт — в папке по правилу");
        let folders = evs.iter().find_map(|e| if let ChannelEvent::Folders(f) = e { Some(f.clone()) } else { None }).unwrap();
        assert!(folders.iter().any(|f| f.name == "Работа") && folders.iter().any(|f| f.id.0 == "archive"));
        let msgs: Vec<_> = evs
            .iter()
            .filter_map(|e| {
                if let ChannelEvent::Messages { conversation, messages, .. } = e { Some((conversation.clone(), messages.clone())) } else { None }
            })
            .collect();
        let team_msgs = &msgs.iter().find(|(c, _)| *c == team.id).unwrap().1;
        assert!(team_msgs.iter().any(|m| m.mentions.iter().any(|p| p.0 == fake::ACCOUNT)), "упоминание меня");
        assert!(team_msgs.iter().any(|m| m.reply_to.is_some() && m.quotes.first().is_some_and(|q| q.text == "посмотри задачу")));
        let Some(ChannelEvent::Cursor(cursor)) = evs.last().cloned() else { panic!() };

        // Живое сообщение: `live`, непрочитанное +1.
        server.anna_writes("Ты тут?");
        let evs = until(&mut c.rx, |e| matches!(e, ChannelEvent::Conversations(_))).await;
        assert!(evs.iter().any(|e| matches!(e, ChannelEvent::Messages { live: true, messages, .. } if messages[0].body.plain() == "Ты тут?")));
        let ChannelEvent::Conversations(cs) = evs.last().unwrap() else { panic!() };
        assert_eq!(cs[0].unread.unwrap().unread, anna.unread.unwrap().unread + 1);
        drop(c);

        // Без связи написали ещё — новое подключение от курсора догоняет пропущенное.
        server.anna_writes("Пока тебя не было");
        let mut again = connect(&plugin, &cache).await;
        again.session.catch_up(Some(cursor)).await.unwrap();
        let evs = until(
            &mut again.rx,
            |e| matches!(e, ChannelEvent::Messages { live: true, messages, .. } if messages[0].body.plain() == "Пока тебя не было"),
        )
        .await;
        assert!(!evs.is_empty());
    });
}

struct Sink(Vec<u8>);

impl postman_core::channel::ByteSink for Sink {
    fn write(&mut self, chunk: &[u8]) -> Result<(), ChannelError> {
        self.0.extend_from_slice(chunk);
        Ok(())
    }
}

/// Ссылка на файл устарела — канал перечитывает сообщение и качает по свежей.
#[test]
fn expired_file_reference_is_refreshed() {
    let server = FakeTelegram::new();
    let plugin = TelegramChannel::with_dial(Arc::new(server.clone()));
    let cache = Cache::open_in_memory().unwrap();
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt(), async {
        let mut c = connect(&plugin, &cache).await;
        c.session.catch_up(None).await.unwrap();
        let evs = until(&mut c.rx, |e| matches!(e, ChannelEvent::Cursor(_))).await;
        let att = evs
            .iter()
            .find_map(|e| match e {
                ChannelEvent::Messages { messages, .. } => {
                    messages.iter().flat_map(|m| m.attachments.iter()).find(|a| a.name == "отчёт.pdf").cloned()
                }
                _ => None,
            })
            .unwrap();
        assert_eq!(att.name, "отчёт.pdf");
        server.expire_file_refs();
        let mut sink = Sink(Vec::new());
        c.session.download(&att.source, &mut sink).await.unwrap();
        assert_eq!(sink.0.len() as u64, att.size.unwrap());
        // Неизвестная беседа — не найдена, без запроса к серверу.
        let r = c.session.history(&ConversationId::new("u42"), None, 5).await;
        assert!(matches!(r, Err(ChannelError::NotFound)), "{r:?}");
    });
}

// --- Хвосты CH-03: альбомы, кнопки и опросы, папки по правилам ---

use postman_backend_telegram::extra::{ButtonAction, TgExtra};
use postman_backend_telegram::ids::Peer;
use postman_core::channel::{Action, Done, FileMeta, Message, MessageId, Outgoing, Uploaded, text_doc};

fn messages(evs: &[ChannelEvent]) -> Vec<(ConversationId, Message)> {
    evs.iter()
        .filter_map(
            |e| if let ChannelEvent::Messages { conversation, messages, .. } = e { Some((conversation.clone(), messages.clone())) } else { None },
        )
        .flat_map(|(c, ms)| ms.into_iter().map(move |m| (c.clone(), m)))
        .collect()
}

fn conv(p: Peer) -> ConversationId {
    p.conv()
}

struct Bytes(Vec<u8>, usize);

impl postman_core::channel::ByteSource for Bytes {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, ChannelError> {
        let rest = &self.0[self.1.min(self.0.len())..];
        let n = rest.len().min(buf.len());
        buf[..n].copy_from_slice(&rest[..n]);
        self.1 += n;
        Ok(n)
    }

    fn len(&self) -> u64 {
        self.0.len() as u64
    }
}

async fn upload(c: &Conn, name: &str, mime: &str, n: usize) -> Uploaded {
    let bytes = vec![0x5a; n];
    let meta = FileMeta { name: name.into(), mime: mime.into(), size: bytes.len() as u64 };
    c.session.upload(&meta, &mut Bytes(bytes, 0)).await.unwrap()
}

fn outgoing(to: &ConversationId, client_id: u64, text: &str, attachments: Vec<Uploaded>) -> Outgoing {
    Outgoing {
        conversation: to.clone(),
        client_id,
        body: text_doc(text),
        attachments,
        reply_to: None,
        quotes: Vec::new(),
        mentions: Vec::new(),
        at: None,
        silent: false,
    }
}

fn album_parts(m: &Message) -> Vec<i32> {
    TgExtra::of(&m.extra).album.map(|a| a.parts).unwrap_or_default()
}

/// Догон: альбом «Команды» — одним сообщением с двумя фото и подписью; кнопки бота и опрос — структурой в `extra`
/// (опрос — ещё и строками тела); голос другого — итоги опроса заново.
#[test]
fn albums_buttons_and_poll_in_catch_up() {
    let server = FakeTelegram::new();
    let plugin = TelegramChannel::with_dial(Arc::new(server.clone()));
    let cache = Cache::open_in_memory().unwrap();
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt(), async {
        let mut c = connect(&plugin, &cache).await;
        c.session.catch_up(None).await.unwrap();
        let evs = until(&mut c.rx, |e| matches!(e, ChannelEvent::Cursor(_))).await;
        let all = messages(&evs);
        let team = conv(Peer::Channel(fake::TEAM));
        let albums: Vec<&Message> = all.iter().filter(|(c, m)| *c == team && !album_parts(m).is_empty()).map(|(_, m)| m).collect();
        assert_eq!(albums.len(), 1, "альбом — одно сообщение");
        let a = albums[0];
        assert_eq!(a.attachments.len(), 2);
        assert_eq!(a.body.plain(), "Фото со встречи");
        let parts = album_parts(a);
        assert_eq!(parts.len(), 2);
        assert_eq!(a.id.0, parts[0].to_string(), "номер — первой части");
        assert!(!all.iter().any(|(c, m)| *c == team && m.id.0 == parts[1].to_string()), "вторая часть — не отдельным сообщением");

        let bot = all.iter().find(|(c, m)| *c == conv(Peer::User(fake::BOT)) && m.body.plain() == "Выберите действие").unwrap();
        let kb = TgExtra::of(&bot.1.extra).keyboard.unwrap();
        assert!(kb.inline);
        assert_eq!(kb.rows.len(), 2);
        assert_eq!(kb.rows[0][0].text, "Да");
        assert_eq!(kb.rows[0][0].action, ButtonAction::Callback { data: b"yes".to_vec(), password: false });
        assert_eq!(kb.rows[1][0].action, ButtonAction::Url { url: "https://example.test/help".into() });

        let group = conv(Peer::Chat(fake::GROUP));
        let (_, pm) = all.iter().find(|(c, m)| *c == group && TgExtra::of(&m.extra).poll.is_some()).unwrap();
        let poll = TgExtra::of(&pm.extra).poll.unwrap();
        assert_eq!((poll.id, poll.question.as_str(), poll.total), (fake::POLL, "Куда поедем?", Some(3)));
        assert_eq!(
            poll.answers.iter().map(|a| (a.text.as_str(), a.voters, a.chosen)).collect::<Vec<_>>(),
            [("Лес", Some(1), false), ("Море", Some(2), true)]
        );
        let body = pm.body.plain();
        assert!(body.contains("📊 Куда поедем?") && body.contains("◦ Море"), "{body}");

        // Голос другого: итоги — то же сообщение заново, мой голос на месте.
        server.vote_by_other(0);
        let evs = until(&mut c.rx, |e| matches!(e, ChannelEvent::Messages { .. })).await;
        let (cv, m) = messages(&evs).pop().unwrap();
        assert_eq!((cv, m.id.clone()), (group, pm.id.clone()));
        let poll = TgExtra::of(&m.extra).poll.unwrap();
        assert_eq!((poll.answers[0].voters, poll.total, poll.answers[1].chosen), (Some(2), Some(4), true));
        assert_eq!(m.edited, pm.edited, "итоги — не правка");
    });
}

/// Отправка нескольких файлов — альбомами: два фото и видео — одним альбомом с подписью, документ — отдельно;
/// 12 фото — альбомы по 10 и 2. Приём: части в разных пачках — один альбом, правка подписи, удаление части и
/// альбома целиком.
#[test]
fn albums_send_and_live() {
    let server = FakeTelegram::new();
    let plugin = TelegramChannel::with_dial(Arc::new(server.clone()));
    let cache = Cache::open_in_memory().unwrap();
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt(), async {
        let mut c = connect(&plugin, &cache).await;
        c.session.catch_up(None).await.unwrap();
        until(&mut c.rx, |e| matches!(e, ChannelEvent::Cursor(_))).await;
        let anna = conv(Peer::User(fake::ANNA));

        let files = vec![
            upload(&c, "a.jpg", "image/jpeg", 700).await,
            upload(&c, "смета.pdf", "application/pdf", 900).await,
            upload(&c, "b.png", "image/png", 800).await,
            upload(&c, "clip.mp4", "video/mp4", 1000).await,
        ];
        let Done::Sent { id, .. } = c.session.act(Action::Send(outgoing(&anna, 9100, "Три файла и смета", files))).await.unwrap()
        else {
            panic!()
        };
        let evs = until(
            &mut c.rx,
            |e| matches!(e, ChannelEvent::Messages { messages, .. } if messages.iter().any(|m| m.attachments.iter().any(|a| a.name == "смета.pdf"))),
        )
        .await;
        let ms = messages(&evs);
        let album = ms.iter().rev().find(|(_, m)| m.id == id).map(|(_, m)| m).unwrap();
        assert_eq!(album.attachments.len(), 3, "фото, фото, видео — одним альбомом");
        assert_eq!(album.body.plain(), "Три файла и смета", "подпись — у альбома");
        assert_eq!(album_parts(album).len(), 3);
        let first = id.0.parse().unwrap();
        assert!(server.grouped(Peer::User(fake::ANNA), first).unwrap().is_some(), "на сервере — альбом");
        let doc = ms.iter().find(|(_, m)| m.attachments.iter().any(|a| a.name == "смета.pdf")).map(|(_, m)| m).unwrap();
        assert_eq!(doc.attachments.len(), 1);
        assert!(doc.body.plain().is_empty(), "документ — без подписи");

        // 12 фото — 10 и 2.
        let mut many = Vec::new();
        for i in 0..12 {
            many.push(upload(&c, &format!("p{i}.jpg"), "image/jpeg", 100 + i).await);
        }
        c.session.act(Action::Send(outgoing(&anna, 9200, "", many))).await.unwrap();
        let evs =
            until(&mut c.rx, |e| matches!(e, ChannelEvent::Messages { messages, .. } if messages.iter().any(|m| m.attachments.len() == 2))).await;
        let sizes: Vec<usize> = messages(&evs).iter().map(|(_, m)| m.attachments.len()).filter(|n| *n > 1).collect();
        assert!(sizes.contains(&10) && sizes.contains(&2), "{sizes:?}");

        // Альбом от Анны: две части одной пачкой, третья — следующей.
        let ids = server.anna_sends_album(3, "Отпуск", 2);
        let evs = until(&mut c.rx, |e| matches!(e, ChannelEvent::Messages { messages, .. } if messages[0].attachments.len() == 3)).await;
        let got = messages(&evs);
        let heads: Vec<&MessageId> = got.iter().filter(|(_, m)| !album_parts(m).is_empty()).map(|(_, m)| &m.id).collect();
        assert!(heads.iter().all(|h| h.0 == ids[0].to_string()), "один номер — первой части: {heads:?}");
        assert!(evs.iter().any(|e| matches!(e, ChannelEvent::Messages { live: true, .. })));

        // Правка подписи и удаление части — альбом заново.
        server.edit_by_other(Peer::User(fake::ANNA), ids[0], "Отпуск, день второй");
        let evs = until(&mut c.rx, |e| matches!(e, ChannelEvent::Messages { .. })).await;
        let (_, m) = messages(&evs).pop().unwrap();
        assert_eq!((m.id.0.as_str(), m.attachments.len(), m.body.plain().as_str()), (ids[0].to_string().as_str(), 3, "Отпуск, день второй"));
        assert!(m.edited.is_some());
        server.delete_by_other(Peer::User(fake::ANNA), &[ids[2]]);
        let evs = until(&mut c.rx, |e| matches!(e, ChannelEvent::Messages { .. } | ChannelEvent::Deleted { .. })).await;
        let (_, m) = messages(&evs).pop().unwrap();
        assert_eq!((m.attachments.len(), album_parts(&m).len()), (2, 2));

        // Удалить альбом — на сервере уходят все части.
        c.session.act(Action::Delete { conversation: anna.clone(), ids: vec![MessageId::new(ids[0].to_string())], for_all: true }).await.unwrap();
        let left = server.ids(Peer::User(fake::ANNA));
        assert!(!left.contains(&ids[0]) && !left.contains(&ids[1]), "{left:?}");
    });
}

/// Правка части альбома, которого сеанс не видел, — альбом целиком с сервера; страница истории не рвёт альбом.
#[test]
fn album_edit_refetch_and_history_pages() {
    let server = FakeTelegram::new();
    let plugin = TelegramChannel::with_dial(Arc::new(server.clone()));
    let cache = Cache::open_in_memory().unwrap();
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt(), async {
        let mut c = connect(&plugin, &cache).await;
        c.session.catch_up(None).await.unwrap();
        until(&mut c.rx, |e| matches!(e, ChannelEvent::Cursor(_))).await;
        let ids = server.anna_sends_album(4, "Горы", 4);
        until(&mut c.rx, |e| matches!(e, ChannelEvent::Cursor(_))).await;
        server.anna_writes("Как тебе?");
        let evs = until(&mut c.rx, |e| matches!(e, ChannelEvent::Cursor(_))).await;
        let Some(ChannelEvent::Cursor(cursor)) = evs.last().cloned() else { panic!() };
        drop(c);

        let mut c = connect(&plugin, &cache).await;
        c.session.catch_up(Some(cursor)).await.unwrap();
        until(&mut c.rx, |e| matches!(e, ChannelEvent::Cursor(_))).await;
        server.edit_by_other(Peer::User(fake::ANNA), ids[0], "Горы, вид сверху");
        let evs = until(&mut c.rx, |e| matches!(e, ChannelEvent::Messages { messages, .. } if !album_parts(&messages[0]).is_empty())).await;
        let (_, m) = messages(&evs).pop().unwrap();
        assert_eq!((m.id.0.clone(), m.attachments.len(), m.body.plain()), (ids[0].to_string(), 4, "Горы, вид сверху".to_owned()));

        // Страница из трёх: «Как тебе?» и две части альбома — альбом не рвётся, приходит целиком следующей.
        let anna = conv(Peer::User(fake::ANNA));
        let page = c.session.history(&anna, None, 3).await.unwrap();
        assert!(page.more);
        assert!(page.messages.iter().all(|(_, m)| album_parts(m).is_empty()), "оборванный альбом — не на этой странице");
        let oldest = page.messages.last().unwrap().1.id.clone();
        let next = c.session.history(&anna, Some(&oldest), 10).await.unwrap();
        let a = next.messages.iter().find(|(_, m)| !album_parts(m).is_empty()).unwrap();
        assert_eq!(a.1.attachments.len(), 4);
    });
}

/// Папка «Непрочитанное» (все типы, без заглушённых, прочитанного, архива, кроме бота) — на лету: прочитал —
/// беседа ушла, новое сообщение — вернулась, заглушил — ушла; папку удалили на другом устройстве — её нет.
#[test]
fn folders_follow_state() {
    let server = FakeTelegram::new();
    let plugin = TelegramChannel::with_dial(Arc::new(server.clone()));
    let cache = Cache::open_in_memory().unwrap();
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt(), async {
        let mut c = connect(&plugin, &cache).await;
        c.session.catch_up(None).await.unwrap();
        let evs = until(&mut c.rx, |e| matches!(e, ChannelEvent::Cursor(_))).await;
        let unread = format!("f{}", fake::UNREAD);
        let folder = |evs: &[ChannelEvent]| {
            evs.iter()
                .rev()
                .find_map(|e| if let ChannelEvent::Folders(f) = e { Some(f.clone()) } else { None })
                .map(|fs| fs.into_iter().find(|f| f.id.0 == unread).map(|f| f.conversations))
        };
        let (anna, group, team, bot, news) = (
            conv(Peer::User(fake::ANNA)),
            conv(Peer::Chat(fake::GROUP)),
            conv(Peer::Channel(fake::TEAM)),
            conv(Peer::User(fake::BOT)),
            conv(Peer::Channel(fake::NEWS)),
        );
        let f = folder(&evs).unwrap().unwrap();
        assert!(f.contains(&anna) && f.contains(&group) && f.contains(&team), "{f:?}");
        assert!(!f.contains(&bot), "исключена явно");
        assert!(!f.contains(&news), "архив — без архива");

        // Прочитал Анну — ушла из папки (и у беседы папки нет).
        let last = MessageId::new(server.ids(Peer::User(fake::ANNA)).into_iter().max().unwrap().to_string());
        c.session.act(Action::MarkRead { conversation: anna.clone(), up_to: last }).await.unwrap();
        let evs = until(&mut c.rx, |e| matches!(e, ChannelEvent::Folders(_))).await;
        assert!(!folder(&evs).unwrap().unwrap().contains(&anna));
        let changed: Vec<_> =
            evs.iter().filter_map(|e| if let ChannelEvent::Conversations(cs) = e { Some(cs.clone()) } else { None }).flatten().collect();
        assert!(changed.iter().any(|c| c.id == anna && !c.folders.iter().any(|f| f.0 == unread)));

        // Новое от Анны — вернулась.
        server.anna_writes("Ещё вопрос");
        let evs = until(&mut c.rx, |e| matches!(e, ChannelEvent::Folders(_))).await;
        assert!(folder(&evs).unwrap().unwrap().contains(&anna));

        // Заглушил группу — ушла.
        c.session.act(Action::Mute { conversation: group.clone(), until: Some(i64::MAX) }).await.unwrap();
        let evs = until(&mut c.rx, |e| matches!(e, ChannelEvent::Folders(_))).await;
        assert!(!folder(&evs).unwrap().unwrap().contains(&group));

        // Папку удалили на другом устройстве.
        server.remove_filter(fake::UNREAD);
        let evs = until(&mut c.rx, |e| matches!(e, ChannelEvent::Folders(_))).await;
        assert_eq!(folder(&evs), Some(None), "папки «Непрочитанное» нет");
    });
}

/// Кнопки бота и голос (`Action::Extra`): кнопки берутся из довеска сообщения; ответ бота — `Done::Reply`, ссылка —
/// без запроса; опрос с несколькими вариантами — два голоса, итоги заново; отзыв; закрытый опрос — отказ сервера.
#[test]
fn bot_buttons_and_votes() {
    use postman_backend_telegram::extra::TgAct;
    use postman_core::channel::ExtraReply;

    let server = FakeTelegram::new();
    server.set_poll(true, false);
    let plugin = TelegramChannel::with_dial(Arc::new(server.clone()));
    let cache = Cache::open_in_memory().unwrap();
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt(), async {
        let mut c = connect(&plugin, &cache).await;
        c.session.catch_up(None).await.unwrap();
        let all = messages(&until(&mut c.rx, |e| matches!(e, ChannelEvent::Cursor(_))).await);
        let (bot, group) = (conv(Peer::User(fake::BOT)), conv(Peer::Chat(fake::GROUP)));
        let (_, bm) = all.iter().find(|(c, m)| *c == bot && TgExtra::of(&m.extra).keyboard.is_some()).unwrap();
        let kb = TgExtra::of(&bm.extra).keyboard.unwrap();
        let act = |conversation: &ConversationId, message: &MessageId, payload| Action::Extra {
            conversation: conversation.clone(),
            message: message.clone(),
            payload,
        };

        let yes = c.session.act(act(&bot, &bm.id, TgAct::press(&kb.rows[0][0]))).await.unwrap();
        assert_eq!(yes, Done::Reply(ExtraReply { text: Some(fake::BOT_YES.into()), alert: false, url: None }));
        let no = c.session.act(act(&bot, &bm.id, TgAct::press(&kb.rows[0][1]))).await.unwrap();
        assert_eq!(no, Done::Reply(ExtraReply { text: Some(fake::BOT_NO.into()), alert: true, url: None }), "предупреждением");
        let help = c.session.act(act(&bot, &bm.id, TgAct::press(&kb.rows[1][0]))).await.unwrap();
        assert_eq!(help, Done::Reply(ExtraReply { url: Some(fake::HELP.into()), ..ExtraReply::default() }), "ссылка — открыть по нажатию");
        let hide = c.session.act(act(&bot, &bm.id, TgAct::press(&kb.rows[1][1]))).await.unwrap();
        assert_eq!(hide, Done::Reply(ExtraReply::default()), "принято молча");
        let r = c.session.act(act(&ConversationId::new("u42"), &bm.id, TgAct::press(&kb.rows[0][0]))).await;
        assert!(matches!(r, Err(ChannelError::NotFound)), "{r:?}");
        let r = c.session.act(act(&bot, &bm.id, postman_core::channel::Extra(b"junk".to_vec()))).await;
        assert!(matches!(r, Err(ChannelError::Protocol(_))), "{r:?}");

        // Опрос с несколькими вариантами: два голоса — итоги тем же сообщением заново.
        let (_, pm) = all.iter().find(|(c, m)| *c == group && TgExtra::of(&m.extra).poll.is_some()).unwrap();
        let poll = TgExtra::of(&pm.extra).poll.unwrap();
        assert!(poll.multiple);
        let both: Vec<_> = poll.answers.iter().collect();
        assert_eq!(c.session.act(act(&group, &pm.id, TgAct::vote(&both))).await.unwrap(), Done::Ok);
        assert_eq!(server.poll_votes(), (vec![0, 1], vec![2, 2]));
        let evs = until(&mut c.rx, |e| matches!(e, ChannelEvent::Messages { messages, .. } if messages.iter().any(|m| m.id == pm.id))).await;
        let (_, m) = messages(&evs).pop().unwrap();
        let p = TgExtra::of(&m.extra).poll.unwrap();
        assert!(p.answers.iter().all(|a| a.chosen), "{p:?}");
        assert_eq!(p.total, Some(4));

        // Отзыв голоса.
        assert_eq!(c.session.act(act(&group, &pm.id, TgAct::vote(&[]))).await.unwrap(), Done::Ok);
        assert_eq!(server.poll_votes(), (vec![], vec![1, 1]));
        let evs = until(&mut c.rx, |e| matches!(e, ChannelEvent::Messages { messages, .. } if messages.iter().any(|m| m.id == pm.id))).await;
        let (_, m) = messages(&evs).pop().unwrap();
        assert!(!TgExtra::of(&m.extra).poll.unwrap().voted());

        // Опрос закрыли на сервере — отказ сервера, голоса прежние.
        server.set_poll(true, true);
        let r = c.session.act(act(&group, &pm.id, TgAct::vote(&both[..1]))).await;
        assert!(matches!(&r, Err(ChannelError::Denied(e)) if e == "MESSAGE_POLL_CLOSED"), "{r:?}");
        assert_eq!(server.poll_votes(), (vec![], vec![1, 1]));
    });
}
