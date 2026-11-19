//! Набор плагинов `postman-plugins` (ADR 0013): протоколы, слинкованные в один файл; каждый запускается своим
//! процессом `postman-plugins --plugin=<id>`. Здесь — список плагинов набора ([`SetEntry`]: точка входа `serve`,
//! вид, как узнать свой адрес, профиль песочницы — вшит в набор, аргументом его не понизить). Строки тех же плагинов
//! в композиционном корне (`app-gpui/src/plugins.rs`) сверяет с набором тест корня.
//!
//! Общий файл — почта и каталог вместе (решение владельца 2026-11-16: одна копия сетевого стека на все протоколы);
//! каналы Matrix (PLG-04) и Telegram (PLG-05) — отдельными файлами наборов `postman-plugins-matrix` и
//! `postman-plugins-telegram`, компоненты установки. Каталог сертификатов — PLG-06, почтовые протоколы — PLG-07…10.

use postman_core::backend::remote::{Claim, Profile, SetEntry};
use postman_core::backend::wire::Kind;

/// Имя файла набора без расширения.
pub const SET: &str = "postman-plugins";

/// Плагины набора.
pub const PLUGINS: &[SetEntry] = &[
    SetEntry {
        id: postman_demo::channel::ID,
        kind: Kind::Channel,
        // Демо сети не нужно, но профиль у плагинов один (рантайм tokio и потоки связи): «сеть без файлов».
        profile: Profile::Network { enterprise_auth: false },
        claim: Claim::Channel,
        serve: postman_demo::channel::serve,
    },
    // Каталог сертификатов адресатов (S5-12 Ф5, PLG-06): своего адреса нет — сервер приносит запрос (контроллер
    // домена резолвит хост, функция ОС в интерфейсе); вход учётной записью Windows (Kerberos, SSPI) — право
    // корпоративной проверки подлинности. Процесс каталога завершается после 5 минут простоя (ADR 0013).
    SetEntry {
        id: postman_backend_ldap::ID,
        kind: Kind::Directory,
        profile: Profile::Network { enterprise_auth: true },
        claim: Claim::None,
        serve: postman_backend_ldap::serve,
    },
    // CalDAV и CardDAV (PLG-07): области учётки «стандарты» (почта — IMAP) и публичные загрузки — один сетевой
    // стек. Процесс держат открытые сессии областей; простой 5 минут — до следующего запроса (ADR 0013).
    SetEntry {
        id: postman_backend_dav::ID,
        kind: Kind::Area,
        profile: Profile::Network { enterprise_auth: false },
        claim: Claim::Area(postman_backend_dav::ID),
        serve: postman_backend_dav::serve,
    },
    // EWS Exchange (PLG-08): почта и свои области (у IMAP-учётки на Exchange, `?ews=…`) — один процесс и один
    // сетевой стек. Хранилище учётки открыто только у ядра (услуга по обмену), как и воркер разбора писем и S/MIME;
    // окно «Отменить» и очередь отправки — у ядра. Вход учётной записью Windows (Kerberos, SSPI) — право
    // корпоративной проверки подлинности. Адрес — путь в адресе учётки `…/EWS/Exchange.asmx` ([`Claim::Path`]).
    SetEntry {
        id: postman_backend_ews::ID,
        kind: Kind::Mail,
        profile: Profile::Network { enterprise_auth: true },
        claim: Claim::Path("/ews/exchange.asmx"),
        serve: postman_backend_ews::serve,
    },
    // IMAP + SMTP (PLG-09): почта учётки «стандарты», IDLE и отправка SMTP — один сетевой стек с каталогом,
    // областями и EWS. Хранилище учётки, воркер и S/MIME — услуги ядра по обмену; окно «Отменить» и очередь — у
    // ядра. Областей у IMAP нет (календарь и контакты учётки ведёт процесс DAV, `?dav=…`). Адрес — своя схема
    // ([`Claim::Scheme`]: `imaps://` и `imap://` — STARTTLS; префикс `imap` покрывает обе).
    SetEntry {
        id: postman_backend_imap::ID,
        kind: Kind::Mail,
        profile: Profile::Network { enterprise_auth: false },
        claim: Claim::Scheme("imap"),
        serve: postman_backend_imap::serve,
    },
];

/// Плагин набора по id.
pub fn find(id: &str) -> Option<&'static SetEntry> {
    PLUGINS.iter().find(|e| e.id == id)
}
