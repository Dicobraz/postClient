//! Области учётной записи через обмен (ADR 0013, PLG-07): сторона плагина — настоящая точка входа набора
//! ([`postman_backend_dav::serve`]) в потоке теста, хост — заместитель ядра; говорят сообщениями `wire` по кадрам
//! `postman-proc`. Сети в тесте нет: сервер — закрытый порт `127.0.0.1`, так что вход (`AreaOpen`), вид сервера
//! (`Probe`) и публичная загрузка (`Fetch`) отвечают отказом тем же кодом, что и в приложении. Полный путь
//! областей с сервером — стенд (`backend-imap/tests/stand_standards.rs`, реестр ядра) и ручная проверка
//! (`docs/manual-tests/PLG-07.md`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // интеграционный тест: «не падать» — для кода приложения (S2-03)

use std::io::pipe;
use std::sync::Arc;

use postman_backend_dav::{ID, serve};
use postman_core::backend::AreaPlugin;
use postman_core::backend::remote::{self, Claim, Host, Launcher, Profile, Row, Started};
use postman_core::backend::wire::{AreaLogin, Kind, Settings};
use postman_core::channel::Secret;
use postman_proc::link::Link;

const ROW: Row =
    Row { id: ID, kind: Kind::Area, claim: Claim::Area("dav"), set: "postman-plugins", profile: Profile::Network { enterprise_auth: false } };

/// Сторона плагина — поток теста с точкой входа набора.
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

fn login() -> AreaLogin {
    AreaLogin { user: "me@example.test".into(), password: Secret::new(b"pw".to_vec()), email: "me@example.test".into() }
}

fn closed_port() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    format!("https://127.0.0.1:{}", addr.port())
}

/// Заместитель от точки входа: вход к закрытому серверу — отказ договора по обмену; события команды процесс не
/// шлёт. Процесс поднимается по первому подключению, сессия отказавшей области не держит его.
#[test]
fn connect_refusal_crosses_the_exchange() {
    let host = Host::new(Arc::new(InThread), Settings::default());
    host.first_frame();
    assert_eq!(host.launches(), 0, "заместитель без запроса процесс не создаёт");
    let area = host.area(ROW);
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let login = login();
    let got = rt.block_on(area.connect(
        &closed_port(),
        &postman_core::backend::Login { user: login.user.clone(), password: "pw".into(), email: login.email.clone() },
        None,
    ));
    assert!(got.is_err(), "сервера нет — вход не удаётся: {:?}", got.as_ref().err());
    assert_eq!(host.launches(), 1);
}

/// Публичная загрузка ([`Host::fetch`]) по негодному адресу — отказ текстом по обмену; процесс стартует по первой
/// загрузке ([`Claim::Area`] — адрес области в адресе учётки не нужен).
#[test]
fn fetch_of_a_bad_address_refuses() {
    let host = Host::new(Arc::new(InThread), Settings::default());
    host.first_frame();
    let got = host.fetch(ROW, closed_port().replace("https", "http"), 1024, None);
    assert!(got.is_err(), "не https — адрес не годится: {got:?}");
    assert_eq!(host.launches(), 1, "процесс поднялся по первой загрузке");
}

/// Вид сервера по адресу ([`AreaPlugin::probe`]): по закрытому порту CalDAV не отвечает — `None` по обмену.
#[test]
fn probe_of_a_closed_port_is_none() {
    let host = Host::new(Arc::new(InThread), Settings::default());
    host.first_frame();
    let area = host.area(ROW);
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let host_part = closed_port().trim_start_matches("https://").to_owned();
    assert_eq!(rt.block_on(area.probe(&host_part)), None);
}
