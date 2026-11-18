//! CalDAV и CardDAV настоящим процессом `postman-plugins --plugin=dav` в песочнице «сеть без файлов» (ADR 0013,
//! PLG-07): `Hello` со сверкой сборки, вход к закрытому серверу — отказ договора по обмену тем же кодом, что и
//! без песочницы. Полный путь областей с сервером — стенд (`backend-imap/tests/stand_standards.rs`, реестр ядра)
//! и ручная проверка (`docs/manual-tests/PLG-07.md`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // интеграционный тест: «не падать» — для кода приложения (S2-03)

use std::path::Path;
use std::sync::Arc;

use postman_core::backend::remote::{Claim, Host, Launcher, Namespaces, ProcLauncher, Profile, Row, Started};
use postman_core::backend::wire::{Kind, Settings};

const ID: &str = "dav";
const ROW: Row =
    Row { id: ID, kind: Kind::Area, claim: Claim::Area(ID), set: "postman-plugins", profile: Profile::Network { enterprise_auth: false } };

/// Запуск настоящего набора.
struct Real;

impl Launcher for Real {
    fn launch(&self, row: &Row) -> std::io::Result<Started> {
        let exe = Path::new(env!("CARGO_BIN_EXE_postman-plugins"));
        ProcLauncher { dir: exe.parent().unwrap().to_owned(), namespaces: Namespaces::Optional }.launch(row)
    }
}

fn closed_port() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    format!("https://127.0.0.1:{}", addr.port())
}

/// Настоящий процесс в песочнице: вход к закрытому серверу — отказ по обмену (`CalendarFailed` интерфейсу делает
/// ядро); повторное подключение поднимает процесс снова (сессия отказавшей области процесс не держит).
#[test]
fn connect_refusal_over_a_real_process() {
    let host = Host::new(Arc::new(Real), Settings::default());
    let area = host.area(ROW);
    assert_eq!(host.launches(), 0, "заместитель без запроса процесс не создаёт");
    host.first_frame();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let login = postman_core::backend::Login { user: "me@example.test".into(), password: "pw".into(), email: "me@example.test".into() };
    let got = rt.block_on(area.connect(&closed_port(), &login, None));
    assert!(got.is_err(), "сервера нет — вход не удаётся: {got:?}");
    assert_eq!(host.launches(), 1);
    let got = rt.block_on(area.connect(&closed_port(), &login, None));
    assert!(got.is_err());
    assert_eq!(host.launches(), 1, "процесс жив — повторный вход не создаёт новый");
}

/// Строка набора — как мнение самого плагина: вид, профиль; адрес области — `?dav=…`.
#[test]
fn set_entry_matches_the_row() {
    let entry = postman_plugins::find(ID).unwrap();
    assert_eq!((entry.kind, entry.profile), (ROW.kind, ROW.profile));
    assert!(!entry.claims("imaps://mail.example.test") && !entry.claims("postman://dav/a/b"));
    assert!(entry.claims("imaps://mail.example.test?dav=https://dav.example.test"));
}
