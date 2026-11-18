//! Каталог настоящим процессом `postman-plugins --plugin=ldap` в песочнице «сеть без файлов» (ADR 0013, PLG-06):
//! `Hello` со сверкой сборки, запрос и ответ по обмену, «каталог не настроен» без сервера (контроллер домена
//! резолвит хост — плагин его не ищет). Полный поиск сертификатов настоящим процессом с подставным сервером
//! (`fake`) — сети нужен loopback, у песочницы Windows (AppContainer) он закрыт: проверяется на Linux и через обмен
//! в потоке теста — `crates/backend-ldap/tests/remote.rs`; настоящий поиск — ручная проверка
//! (`docs/manual-tests/PLG-06.md`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // интеграционный тест: «не падать» — для кода приложения (S2-03)

use std::path::Path;
use std::sync::Arc;

use postman_backend_ldap::ID;
use postman_core::backend::remote::{Claim, Host, Launcher, Namespaces, ProcLauncher, Profile, Row, Started};
use postman_core::backend::wire::{DirectoryLogin, DirectoryQuery, Kind, Settings};
use postman_core::smime::directory::DirectoryError;

const ROW: Row =
    Row { id: ID, kind: Kind::Directory, claim: Claim::None, set: "postman-plugins", profile: Profile::Network { enterprise_auth: true } };

/// Запуск настоящего набора.
struct Real;

impl Launcher for Real {
    fn launch(&self, row: &Row) -> std::io::Result<Started> {
        let exe = Path::new(env!("CARGO_BIN_EXE_postman-plugins"));
        ProcLauncher { dir: exe.parent().unwrap().to_owned(), namespaces: Namespaces::Optional }.launch(row)
    }
}

fn query(server: Option<String>) -> DirectoryQuery {
    DirectoryQuery { server, base_dn: None, starttls: false, login: DirectoryLogin::Anonymous, addresses: Vec::new() }
}

/// Настоящий процесс в песочнице: «каталог не настроен» приходит по обмену тем же кодом, что и без него; повторный
/// запрос поднимает процесс снова (у каталога нет учёток — хост держит его до [`IDLE_EXIT`](postman_core::backend::remote::IDLE_EXIT)
/// простоя, в тесте он живёт до конца).
#[test]
fn directory_without_a_server_refuses_over_a_real_process() {
    let host = Host::new(Arc::new(Real), Settings::default());
    let dir = host.directory(ROW, query(None));
    assert_eq!(host.launches(), 0, "заместитель без запроса процесс не создаёт");
    host.first_frame();
    assert_eq!(dir.certificates(&["me@example.test".into()]), Err(DirectoryError::NotConfigured));
    assert_eq!(host.launches(), 1);
    assert_eq!(dir.certificates(&["me@example.test".into()]), Err(DirectoryError::NotConfigured));
    assert_eq!(host.launches(), 1, "процесс жив — повторный запрос не создаёт новый");
}

/// Строка набора — как мнение самого плагина: вид, профиль; своего адреса у каталога нет.
#[test]
fn set_entry_matches_the_row() {
    let entry = postman_plugins::find(ID).unwrap();
    assert_eq!((entry.kind, entry.profile), (ROW.kind, ROW.profile));
    assert!(!entry.claims("ldap://dc.corp.example") && !entry.claims("postman://ldap/a/b"));
}

/// Полный поиск настоящим процессом и подставной сервер — только где песочница пускает loopback.
#[cfg(not(target_os = "windows"))]
#[test]
fn directory_finds_certificates_over_a_real_process() {
    let (addr, seen) = postman_backend_ldap::fake::server(|base| match base {
        "" => (vec![postman_backend_ldap::fake::entry("", &[("defaultNamingContext", &[postman_backend_ldap::fake::BASE.as_bytes()])])], 0),
        _ => (
            vec![postman_backend_ldap::fake::entry(
                "CN=Anna,OU=Staff,DC=corp,DC=example",
                &[("mail", &[b"anna@corp.example"]), ("userCertificate", &[b"\x30\x82C1"])],
            )],
            0,
        ),
    });
    let host = Host::new(Arc::new(Real), Settings::default());
    host.first_frame();
    let dir = host.directory(ROW, query(Some(addr)));
    let got = dir.certificates(&["anna@corp.example".into()]).unwrap();
    assert_eq!(got, [b"\x30\x82C1".to_vec()]);
    assert_eq!(seen.lock().unwrap().bases, ["", postman_backend_ldap::fake::BASE]);
}
