//! Тесты на записанных ответах LDAP (`docs/SMIME.md` Ф5): подменный сервер (`fake`) на 127.0.0.1 отвечает
//! сообщениями BER по RFC 4511 — как AD (rootDSE, записи с `userCertificate`, ссылки на другие разделы, предел
//! записей).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use ldap3::asn1::{PL, StructureTag, TagClass};
use postman_core::smime::directory::{Directory, DirectoryError};

use super::*;
use crate::fake::{BASE, closed_port, entry, octet, server, tlv};

fn anonymous(server: &str, base_dn: Option<&str>) -> LdapDirectory {
    LdapDirectory::new(Settings { server: Some(server.into()), base_dn: base_dn.map(Into::into), login: Login::Anonymous, ..Settings::default() })
}

/// Как AD: база — из rootDSE, сертификаты из нескольких записей и обоих имён атрибута; ссылки на другие разделы и
/// лишние атрибуты пропускаются; предел записей — не ошибка.
#[test]
fn finds_certificates() {
    let (addr, seen) = server(|base| match base {
        "" => (vec![entry("", &[("defaultNamingContext", &[BASE.as_bytes()])])], 0),
        _ => (
            vec![
                entry(
                    "CN=Anna,OU=Staff,DC=corp,DC=example",
                    &[("mail", &[b"anna@corp.example"]), ("userCertificate", &[b"\x30\x82C1", b"\x30\x82C2"])],
                ),
                // Ссылка на другой раздел (`SearchResultReference`).
                tlv(0x73, &octet(b"ldap://ForestDnsZones.corp.example/DC=ForestDnsZones,DC=corp,DC=example")),
                entry("CN=Boris,OU=Staff,DC=corp,DC=example", &[("usercertificate;binary", &[b"\x30\x82C3"])]),
            ],
            4,
        ),
    });
    let got = anonymous(&addr, None).certificates(&["anna@corp.example".into(), "boris@corp.example".into()]).unwrap();
    assert_eq!(got, [b"\x30\x82C1".to_vec(), b"\x30\x82C2".to_vec(), b"\x30\x82C3".to_vec()]);
    assert_eq!(seen.lock().unwrap().bases, ["", BASE]);
}

/// База из настроек — rootDSE не спрашивают; входа без настроек нет (подменный сервер — без входа).
#[test]
fn base_from_settings() {
    let (addr, seen) = server(|_| (vec![entry("CN=A", &[("userCertificate", &[b"\x30\x82A"])])], 0));
    let got = anonymous(&format!("ldap://{addr}"), Some(BASE)).certificates(&["a@corp.example".into()]).unwrap();
    assert_eq!(got, [b"\x30\x82A".to_vec()]);
    let seen = seen.lock().unwrap();
    assert_eq!(seen.bases, [BASE]);
    assert!(!seen.ops.contains(&0x60), "bind: {:?}", seen.ops);
}

/// Отказ поиска (нет такой базы) — «каталог недоступен».
#[test]
fn search_error_is_unavailable() {
    let (addr, _) = server(|_| (vec![], 32));
    let got = anonymous(&addr, Some("DC=gone")).certificates(&["a@corp.example".into()]);
    assert!(matches!(got, Err(DirectoryError::Unavailable(ref e)) if e.contains("rc=32")), "{got:?}");
}

/// Нет связи — «каталог недоступен», без ожидания тайм-аута.
#[test]
fn refused_is_unavailable() {
    let t = std::time::Instant::now();
    let got = anonymous(&closed_port(), Some(BASE)).certificates(&["a@corp.example".into()]);
    assert!(matches!(got, Err(DirectoryError::Unavailable(_))), "{got:?}");
    assert!(t.elapsed() < TIMEOUT);
}

/// Простой вход по открытому каналу не начинается: пароль ушёл бы как есть.
#[test]
fn simple_login_needs_tls() {
    let login = Login::Simple { user: "CN=A".into(), password: Zeroizing::new("secret".into()) };
    let dir = LdapDirectory::new(Settings { server: Some(closed_port()), base_dn: Some(BASE.into()), login, ..Settings::default() });
    let got = dir.certificates(&["a@corp.example".into()]);
    assert!(matches!(got, Err(DirectoryError::Unavailable(ref e)) if e.contains("ldaps")), "{got:?}");
}

/// OpenLDAP: в rootDSE только `namingContexts` — база — первый раздел данных, `cn=config` пропускается; сертификат —
/// `userCertificate;binary` (RFC 4523).
#[test]
fn openldap_base_from_naming_contexts() {
    let (addr, seen) = server(|base| match base {
        "" => (vec![entry("", &[("namingContexts", &[b"cn=config", b"dc=example,dc=org"])])], 0),
        _ => (vec![entry("uid=anna,ou=people,dc=example,dc=org", &[("userCertificate;binary", &[b"\x30\x82O"])])], 0),
    });
    let got = anonymous(&addr, None).certificates(&["anna@example.org".into()]).unwrap();
    assert_eq!(got, [b"\x30\x82O".to_vec()]);
    assert_eq!(seen.lock().unwrap().bases, ["", "dc=example,dc=org"]);
}

/// Выбор базы из rootDSE: `defaultNamingContext` (AD, 389 DS — ALD Pro, FreeIPA) важнее списка; служебные разделы
/// 389 DS пропускаются; нет подходящего — нет базы.
#[test]
fn base_choice() {
    let root = |attrs: &[(&str, &[&[u8]])]| ldap3::asn1::parse_tag(&entry("", attrs)).map(|(_, t)| t).unwrap();
    let r389 =
        root(&[("namingContexts", &[b"o=netscaperoot", b"cn=changelog", b"dc=ald,dc=example"]), ("defaultnamingcontext", &[b"dc=ald,dc=example"])]);
    assert_eq!(choose_base(r389).as_deref(), Some("dc=ald,dc=example"));
    let r389_list = root(&[("namingContexts", &[b"o=NetscapeRoot", b"cn=changelog", b"dc=ald,dc=example"])]);
    assert_eq!(choose_base(r389_list).as_deref(), Some("dc=ald,dc=example"));
    assert_eq!(choose_base(root(&[("namingContexts", &[b"cn=config"])])), None);
    assert_eq!(choose_base(root(&[])), None);
}

#[test]
fn targets() {
    let t = |s: &str| Target::parse(s).map(|t| (t.url, t.host, t.tls));
    assert_eq!(t("dc1.corp.example"), Ok(("ldap://dc1.corp.example".into(), "dc1.corp.example".into(), false)));
    assert_eq!(t(" LDAPS://dc1.corp.example:636/ "), Ok(("ldaps://dc1.corp.example:636".into(), "dc1.corp.example".into(), true)));
    assert_eq!(t("[::1]:389"), Ok(("ldap://[::1]:389".into(), "::1".into(), false)));
    for bad in ["", "http://dc1", "ldap://", "ldap://user@dc1", "dc 1"] {
        assert!(Target::parse(bad).is_err(), "{bad}");
    }
}

/// Спецсимволы адреса экранируются (RFC 4515): фильтр не меняет смысл.
#[test]
fn filter_escapes() {
    assert_eq!(
        filter(&["a*(b)@x.test".into(), "c@x.test".into()]),
        r"(|(mail=a\2a\28b\29@x.test)(proxyAddresses=smtp:a\2a\28b\29@x.test)(mail=c@x.test)(proxyAddresses=smtp:c@x.test))"
    );
}

/// Чужая запись странной формы — пусто, без паники.
#[test]
fn malformed_entries_are_skipped() {
    let tag = |id, payload| StructureTag { class: TagClass::Application, id, payload };
    let prim = |b: &[u8]| StructureTag { class: TagClass::Universal, id: 4, payload: PL::P(b.to_vec()) };
    assert!(values(tag(4, PL::P(vec![1])), &CERT_ATTRS).is_empty());
    assert!(values(tag(5, PL::C(vec![])), &CERT_ATTRS).is_empty());
    assert!(values(tag(4, PL::C(vec![prim(b"CN=A")])), &CERT_ATTRS).is_empty());
    let odd_attr = StructureTag { class: TagClass::Universal, id: 16, payload: PL::C(vec![prim(b"userCertificate")]) };
    let attrs = StructureTag { class: TagClass::Universal, id: 16, payload: PL::C(vec![prim(b"x"), odd_attr]) };
    assert!(values(tag(4, PL::C(vec![prim(b"CN=A"), attrs])), &CERT_ATTRS).is_empty());
}
