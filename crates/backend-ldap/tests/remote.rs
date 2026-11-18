//! Каталог через обмен (ADR 0013, PLG-06): заместитель хоста и сторона плагина — настоящая точка входа набора
//! ([`postman_backend_ldap::serve`]) — говорят сообщениями `wire` по кадрам `postman-proc`, плагин — в потоке
//! теста (настоящий процесс набора — `crates/plugins/tests/ldap.rs`). Сервер — подставной (`fake`) на 127.0.0.1,
//! сети вне теста нет. Проверки: поиск сертификатов туда и обратно, «каталог не настроен» без сервера (контроллер
//! домена резолвит хост, не плагин), простой вход без TLS не начинается.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // интеграционный тест: «не падать» — для кода приложения (S2-03)

use std::io::pipe;
use std::sync::Arc;

use postman_backend_ldap::fake::{BASE, closed_port, entry, server};
use postman_backend_ldap::{ID, serve};
use postman_core::backend::remote::{self, Claim, Host, Launcher, Profile, Row, Started};
use postman_core::backend::wire::{DirectoryLogin, DirectoryQuery, Kind, Settings};
use postman_core::smime::directory::{Directory, DirectoryError};
use postman_proc::link::Link;

const ROW: Row =
    Row { id: ID, kind: Kind::Directory, claim: Claim::None, set: "postman-plugins", profile: Profile::Network { enterprise_auth: true } };

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

fn query(server: Option<String>) -> DirectoryQuery {
    DirectoryQuery { server, base_dn: None, starttls: false, login: DirectoryLogin::Anonymous, addresses: Vec::new() }
}

/// Поиск сертификатов по обмену: запрос с настройками, ответ — DER записей; база — из rootDSE подменного AD.
#[test]
fn certificates_cross_the_exchange() {
    let (addr, seen) = server(|base| match base {
        "" => (vec![entry("", &[("defaultNamingContext", &[BASE.as_bytes()])])], 0),
        _ => (
            vec![
                entry("CN=Anna,OU=Staff,DC=corp,DC=example", &[("mail", &[b"anna@corp.example"]), ("userCertificate", &[b"\x30\x82C1"])]),
                entry("CN=Boris,OU=Staff,DC=corp,DC=example", &[("usercertificate;binary", &[b"\x30\x82C2"])]),
            ],
            0,
        ),
    });
    let host = Host::new(Arc::new(InThread), Settings::default());
    let dir = host.directory(ROW, query(Some(addr)));
    assert_eq!(host.launches(), 0, "заместитель без запроса процесс не создаёт");
    host.first_frame();
    let got = dir.certificates(&["anna@corp.example".into(), "boris@corp.example".into()]).unwrap();
    assert_eq!(got, [b"\x30\x82C1".to_vec(), b"\x30\x82C2".to_vec()]);
    assert_eq!(seen.lock().unwrap().bases, ["", BASE]);
    assert_eq!(host.launches(), 1);
}

/// Сервера в запросе нет (хост не нашёл контроллер домена) — «каталог не настроен» тем же кодом, что и без обмена.
#[test]
fn no_server_is_not_configured() {
    let host = Host::new(Arc::new(InThread), Settings::default());
    host.first_frame();
    let dir = host.directory(ROW, query(None));
    assert_eq!(dir.certificates(&["a@corp.example".into()]), Err(DirectoryError::NotConfigured));
}

/// Простой вход по открытому каналу не начинается (пароль ушёл бы как есть) — отказ приходит по обмену.
#[test]
fn simple_login_needs_tls() {
    let host = Host::new(Arc::new(InThread), Settings::default());
    host.first_frame();
    let query = DirectoryQuery {
        server: Some(closed_port()),
        base_dn: Some(BASE.into()),
        starttls: false,
        login: DirectoryLogin::Password { user: "CN=svc".into(), password: postman_core::channel::Secret::new(b"pw".to_vec()) },
        addresses: Vec::new(),
    };
    let dir = host.directory(ROW, query);
    let got = dir.certificates(&["a@corp.example".into()]);
    assert!(matches!(got, Err(DirectoryError::Unavailable(ref e)) if e.contains("ldaps")));
}
