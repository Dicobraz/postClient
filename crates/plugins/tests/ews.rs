//! Путь EWS настоящим процессом `postman-plugins --plugin=ews` в песочнице «сеть без файлов» (ADR 0013, PLG-08):
//! заместитель ([`MailPlugin`]) поднимает набор, вход к закрытому серверу — отказ по обмену тем же родом ошибки,
//! что и без песочницы; синхронизация учётки целиком с сервером — подменный сервер EWS (`backend-ews/tests/mock.rs`)
//! и боевой Exchange — стенд (`backend-ews/tests/stand_exchange.rs`) и ручная проверка (`docs/manual-tests/PLG-08.md`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // интеграционный тест: «не падать» — для кода приложения (S2-03)

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use postman_cache::Cache;
use postman_core::backend::remote::{Claim, Host, Launcher, Namespaces, ProcLauncher, Profile, Row, Started};
use postman_core::backend::wire::{Kind, Settings};
use postman_core::backend::{MailPlugin, Spawn};
use postman_core::model::{Auth, ErrorKind};
use postman_core::sync::{Account, Event};

const ID: &str = "ews";
const ROW: Row = Row {
    id: ID,
    kind: Kind::Mail,
    claim: Claim::Path("/ews/exchange.asmx"),
    set: "postman-plugins",
    profile: Profile::Network { enterprise_auth: true },
};

/// Запуск настоящего набора.
struct Real;

impl Launcher for Real {
    fn launch(&self, row: &Row) -> std::io::Result<Started> {
        let exe = Path::new(env!("CARGO_BIN_EXE_postman-plugins"));
        ProcLauncher { dir: exe.parent().unwrap().to_owned(), namespaces: Namespaces::Optional }.launch(row)
    }
}

/// Сервера нет: вход не удался — отказ по обмену тем же родом ошибки (сеть), синхронизация остановилась.
#[test]
fn sign_in_refusal_over_a_real_process() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let url = format!("https://127.0.0.1:{}/EWS/Exchange.asmx", addr.port());
    let host = Host::new(Arc::new(Real), Settings::default());
    let mail = host.mail(ROW);
    assert_eq!(host.launches(), 0, "заместитель без запроса процесс не создаёт");
    host.first_frame();
    let (events, rx) = std::sync::mpsc::channel();
    let _commands = mail.spawn(Spawn {
        account: Account { url, auth: Auth::Basic { user: "user@example.test".into(), password: "pw".into() } },
        profile: None,
        cache: Cache::open_in_memory().unwrap(),
        index: None,
        events,
        repaint: Box::new(|| {}),
    });
    let until = std::time::Instant::now() + Duration::from_secs(60);
    let error = loop {
        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Event::Error(e)) => break e,
            Ok(_) => {}
            Err(e) => panic!("нет отказа входа ({e})"),
        }
        assert!(std::time::Instant::now() < until, "отказа входа нет за 60 с");
    };
    assert_eq!(error.kind, ErrorKind::Network, "сервера нет — вход не удалётся: {error:?}");
    assert_eq!(host.launches(), 1, "процесс поднят по запросу");
}

/// Строка набора — как мнение самого плагина: вид, профиль (корпоративная проверка подлинности — вход учётной
/// записью Windows); адрес — путь `…/EWS/Exchange.asmx`, областей у учётки без EWS-пути нет.
#[test]
fn set_entry_matches_the_row() {
    let entry = postman_plugins::find(ID).unwrap();
    assert_eq!((entry.kind, entry.profile), (ROW.kind, ROW.profile));
    assert!(!entry.claims("imaps://mail.example.test") && !entry.claims("https://mail.example.test/"));
    assert!(entry.claims("https://mail.example.test/EWS/Exchange.asmx"));
    assert!(!entry.claims("https://mail.example.test/EWS/Exchange.asmx/deep/path"));
    assert!(entry.claims("https://mail.example.test/EWS/Exchange.asmx?ews=x"), "запрос в адресе учётки — не мешает");
}
