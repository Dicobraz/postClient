//! Пути к серверам (ADR 0004, S7-02б): композиционный корень. Здесь — какие плагины протоколов знает Postman и в
//! каком порядке: почтовые пути — JMAP (путь по умолчанию: адрес без своей схемы), затем EWS Exchange (S7-06г:
//! адрес — URL EWS; узел без схемы — EWS узла или `autodiscover.<узел>`), затем IMAP + SMTP; пути областей — CalDAV и
//! CardDAV, EWS (у IMAP-учётки на Exchange); каналы бесед — Telegram и Matrix. Новый протокол — плагин-крейт
//! и строка здесь; `postman-core` и экраны не меняются.
//!
//! Плагины отдельными процессами (ADR 0013): строка плагина ([`ROWS`]) — идентификатор, вид, как узнать свой адрес,
//! файл набора и профиль песочницы, без запуска процесса; процессы ведёт хост ([`host`]) — после первого кадра и только
//! по запросу. Так работают демо-канал (PLG-02, общий набор `postman-plugins`), Matrix (PLG-04, свой файл
//! `postman-plugins-matrix`), Telegram (PLG-05, свой файл `postman-plugins-telegram`), каталог сертификатов
//! (PLG-06, общий набор), CalDAV/CardDAV (PLG-07, области учётки и публичные загрузки), EWS (PLG-08),
//! IMAP + SMTP (PLG-09) и JMAP (PLG-10) — общим набором: в `postman.exe` сетевого стека протоколов больше нет.

use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use postman_core::backend::Registry;
use postman_core::backend::remote::{Claim, Host, Launcher, ProcLauncher, Profile, Row, Started};
use postman_core::backend::wire::{DirectoryQuery, Kind, Settings};
use postman_core::smime::directory::Directory;

/// Демо-канал — набором `postman-plugins` (PLG-02): без сети, для тестов и замера.
const DEMO: Row = Row {
    id: postman_demo::channel::ID,
    kind: Kind::Channel,
    claim: Claim::Channel,
    set: "postman-plugins",
    profile: Profile::Network { enterprise_auth: false },
};

/// Matrix (CH-05) — набором `postman-plugins-matrix` (PLG-04, ADR 0013): тяжёлый канал отдельным файлом, решение
/// владельца 2026-11-16. Включает настройка и политика `AllowChannelMatrix`; файл — компонент установки (PLG-03).
const MATRIX: Row = Row {
    id: crate::account_matrix::MATRIX,
    kind: Kind::Channel,
    claim: Claim::Channel,
    set: "postman-plugins-matrix",
    profile: Profile::Network { enterprise_auth: false },
};

/// Telegram (CH-03) — набором `postman-plugins-telegram` (PLG-05, ADR 0013): отдельным файлом набора, решение
/// владельца 2026-11-16 (общий файл — почта и каталог). Включает настройка и политика `AllowChannelTelegram`;
/// ключ приложения — у плагина.
const TELEGRAM: Row = Row {
    id: crate::account_telegram::TELEGRAM,
    kind: Kind::Channel,
    claim: Claim::Channel,
    set: "postman-plugins-telegram",
    profile: Profile::Network { enterprise_auth: false },
};

/// Каталог сертификатов адресатов (S5-12 Ф5, PLG-06) — общим набором `postman-plugins` (решение владельца
/// 2026-11-16: общий файл — почта и каталог). Идентификатор — мнение набора сверяет тест строк. Своего адреса нет
/// ([`Claim::None`]): сервер приносит запрос — контроллер домена компьютера ([`crate::sys::domain_controller`],
/// функция ОС, решение №12) или настройки «Безопасность». Профиль с корпоративной проверкой подлинности: вход
/// учётной записью Windows (Kerberos, SSPI).
const LDAP: Row =
    Row { id: "ldap", kind: Kind::Directory, claim: Claim::None, set: "postman-plugins", profile: Profile::Network { enterprise_auth: true } };

/// CalDAV и CardDAV (PLG-07) — общим набором `postman-plugins` (решение владельца 2026-11-16): области учётки
/// «стандарты» (почта — IMAP) и публичные загрузки (картинки письма, подписки на календари, занятость по ссылке).
/// Адрес области — в адресе учётки ([`Claim::Area`]: `?dav=https://…`); процесс стартует по первому запросу
/// (вход учётки или загрузка), простой 5 минут завершает его до следующего ([`IDLE_EXIT`]).
const DAV: Row =
    Row { id: "dav", kind: Kind::Area, claim: Claim::Area("dav"), set: "postman-plugins", profile: Profile::Network { enterprise_auth: false } };

/// EWS Exchange (PLG-08) — общим набором `postman-plugins`: почта и свои области (у IMAP-учётки на Exchange,
/// `?ews=…`) — один процесс. Адрес — путь в адресе учётки ([`Claim::Path`]: `…/EWS/Exchange.asmx`, «строкой
/// описания плагина»); хранилище учётки, воркер и S/MIME — услуги ядра по обмену; окно «Отменить» и очередь
/// отправки — у ядра. Профиль с корпоративной проверкой подлинности: вход учётной записью Windows (Kerberos, SSPI).
const EWS: Row = Row {
    id: "ews",
    kind: Kind::Mail,
    claim: Claim::Path("/ews/exchange.asmx"),
    set: "postman-plugins",
    profile: Profile::Network { enterprise_auth: true },
};

/// IMAP + SMTP (PLG-09) — общим набором `postman-plugins` (решение владельца 2026-11-16): почта учётки
/// «стандарты», IDLE и отправка SMTP — одним сетевым стеком с каталогом, областями и EWS. Адрес — своя схема
/// ([`Claim::Scheme`]: префикс `imap` — обе, `imaps://` и `imap://` STARTTLS); области учётки (`?dav=…`) — процесс
/// DAV ([`DAV`]); хранилище учётки, воркер и S/MIME — услуги ядра по обмену, окно «Отменить» и очередь — у ядра.
const IMAP: Row =
    Row { id: "imap", kind: Kind::Mail, claim: Claim::Scheme("imap"), set: "postman-plugins", profile: Profile::Network { enterprise_auth: false } };

/// JMAP (PLG-10) — общим набором `postman-plugins` (решение владельца 2026-11-16): основной путь (The Post) —
/// последним из `postman.exe`; календарь, контакты, задачи, правила и автоответ — события той же сессии.
/// Своего адреса у пути нет ([`Claim::None`]): узел отвечает `/.well-known/jmap` — вид сервера по сети
/// ([`Registry::detect`], процесс поднимается по первому вопросу). Хранилище учётки (с задачами и связями
/// бесед), поисковый индекс, воркер и S/MIME — услуги ядра по обмену; вход — логин с паролем или токен.
const JMAP: Row =
    Row { id: "jmap", kind: Kind::Mail, claim: Claim::None, set: "postman-plugins", profile: Profile::Network { enterprise_auth: false } };

/// Плагины отдельными процессами.
pub(crate) const ROWS: &[Row] = &[DEMO, MATRIX, TELEGRAM, LDAP, DAV, EWS, IMAP, JMAP];

static HOST: OnceLock<Host> = OnceLock::new();

/// Хост плагинов процесса. Создание — без ввода-вывода и без процессов: каталог набора и политика namespaces читаются
/// при запуске процесса, в фоновом потоке плагина.
pub(crate) fn host() -> &'static Host {
    HOST.get_or_init(|| {
        let log = std::env::var("POSTMAN_LOG").unwrap_or_default();
        Host::new(Arc::new(Beside), Settings { log, ..Settings::default() })
    })
}

/// Заместитель канала `id` по его строке ([`ROWS`]) — первым в очереди старта (активная учётка); строки нет — `None`.
/// Процесса нет, пока заместителя не позовут.
pub(crate) fn remote_channel(id: &str) -> Option<Arc<dyn postman_core::channel::ChannelPlugin>> {
    let row = *ROWS.iter().find(|r| r.id == id)?;
    let host = host();
    let remote = host.channel(row);
    host.prefer(row.id);
    Some(Arc::new(remote))
}

/// Ключ приложения Telegram есть — из `Hello` плагина набора (PLG-05: ключ — у плагина, интерфейс у `api_key` не
/// зовёт). Процесс ещё не поднимался — не знаем, показываем вход: отказ «нет ключа» вернёт сам плагин.
pub(crate) fn telegram_has_key() -> bool {
    host().channel(TELEGRAM).has_key()
}

/// Заместитель каталога (PLG-06): настройки запроса — из «Настройки → Безопасность» и хранилища учётных данных
/// (пароль простого входа читает ядро, [`DirectoryQuery`]); процесс набора — по первому поиску сертификатов,
/// после простоя ([`IDLE_EXIT`](postman_core::backend::remote::IDLE_EXIT)) завершается до следующего (ADR 0013).
pub(crate) fn remote_directory(query: DirectoryQuery) -> Arc<dyn Directory> {
    host().directory(LDAP, query)
}

/// Заместитель пути областей (PLG-07): CalDAV и CardDAV — процессом общего набора; реестр вставляет его вместо
/// плагина, для ядра разницы нет ([`postman_core::backend::areas`]).
pub(crate) fn remote_area() -> postman_core::backend::remote::RemoteAreas {
    host().area(DAV)
}

/// Адрес — пути EWS ([`EWS`], строкой описания плагина, PLG-08): «Exchange (EWS)» на строке входа, поле логина и
/// вход учётной записью Windows. Интерфейс плагин пути не линкует — мнение набора о адресе здесь.
pub(crate) fn is_ews_url(url: &str) -> bool {
    ROWS.iter().any(|r| r.id == "ews" && r.claims(url))
}

/// Публичный ресурс без входа — процессом DAV ([`Host::fetch`](postman_core::backend::remote::Host::fetch),
/// PLG-07): картинка письма, занятость по ссылке. Блокирует поток — звать из фона. Ошибка — текстом, как её
/// показал бы плагин.
pub(crate) fn fetch_public(url: &str, max: usize) -> Result<Vec<u8>, String> {
    match host().fetch(DAV, url.to_owned(), max, None)? {
        (postman_core::backend::wire::Fetched::Body { .. }, body) => Ok(body),
        (postman_core::backend::wire::Fetched::NotModified, _) => Err("HTTP: 304 Not Modified".into()),
    }
}

/// Тело публичного ресурса с его ETag.
pub(crate) type FetchedBody = (Vec<u8>, Option<String>);

/// Публичный ресурс, если изменился (S8-44: подписки), — процессом DAV: `None` — не менялся с `etag`.
pub(crate) fn fetch_changed(url: &str, max: usize, etag: Option<&str>) -> Result<Option<FetchedBody>, String> {
    match host().fetch(DAV, url.to_owned(), max, etag.map(str::to_owned))? {
        (postman_core::backend::wire::Fetched::Body { etag }, body) => Ok(Some((body, etag))),
        (postman_core::backend::wire::Fetched::NotModified, _) => Ok(None),
    }
}

/// Набор — рядом с `postman.exe` (как воркер); namespaces Linux — по политике `SandboxNamespaces`.
struct Beside;

impl Launcher for Beside {
    fn launch(&self, row: &Row) -> std::io::Result<Started> {
        let dir = std::env::current_exe()?.parent().map(std::path::Path::to_path_buf).unwrap_or_default();
        #[cfg(target_os = "linux")]
        let namespaces = crate::security::sandbox_namespaces(crate::sys::shell::policy("SandboxNamespaces"));
        #[cfg(not(target_os = "linux"))]
        let namespaces = postman_core::backend::remote::Namespaces::Optional;
        ProcLauncher { dir, namespaces }.launch(row)
    }
}

/// Демо-канал — отдельным процессом: в dev-сборке и в замере с `POSTMAN_BENCH_PLUGINS`; снимки и release без набора в
/// поставке (PLG-03) — в процессе, как прежде. У Matrix и Telegram внутрипроцессных плагинов больше нет (PLG-04, PLG-05).
pub(crate) fn demo_remote(screens: bool) -> bool {
    !screens && (cfg!(debug_assertions) || std::env::var_os(postman_core::bench::PLUGINS_VAR).is_some())
}

/// Реестр плагинов процесса.
pub(crate) fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        let r = Registry::default()
            // JMAP — тем же набором (PLG-10): основной путь последним ушёл из `postman.exe`; узел без схемы
            // проверяется по сети (`/.well-known/jmap`), потому заместитель первым на пробу.
            .with_mail(host().mail(JMAP))
            // EWS — процессом общего набора (PLG-08, ADR 0013): почта и свои области одним процессом, хранилище
            // учётки, воркер и S/MIME — услуги ядра по обмену.
            .with_mail(host().mail(EWS))
            // IMAP + SMTP — тем же набором (PLG-09): IDLE и отправка SMTP в процессе, окно «Отменить» — у ядра.
            .with_mail(host().mail(IMAP))
            // CalDAV и CardDAV — процессом общего набора (PLG-07, ADR 0013); области EWS у IMAP-учётки на
            // Exchange — тем же процессом набора ([`EWS`]).
            .with_area(remote_area())
            .with_area(host().area(EWS));
        // Каналы бесед (ADR 0012 п. 12) — заместителями своих наборов (ADR 0013): процесс — у хоста, после первого
        // кадра и по запросу; включает настройка и политика (`account_telegram`, `account_matrix`).
        r.with_channel(host().channel(MATRIX)).with_channel(host().channel(TELEGRAM))
    })
}

/// Автоответ записан, но сервер хранит один текст всем (отказ в своём скрипте `vacation`, плагин JMAP): это не ошибка
/// записи, а ограничение, о котором надо сказать пользователю.
pub(crate) fn autoreply_limited(error: &str) -> bool {
    error == postman_core::settings::ONE_TEXT_ONLY
}

/// Выход: письма в окне «Отменить» держит очередь отправки ядра (PLG-08), сообщения каналов — очереди
/// отправки потоков каналов (ADR 0012 п. 11): они уходят сейчас, выход их дожидается. Окна уже нет: не дольше
/// [`LEAVE_LIMIT`], сервер без ответа не держит программу вечно.
pub(crate) fn finish_sends() {
    use postman_core::backend::mail::{mail_holding, mail_leave};
    use postman_core::channel::outbox;
    if let Some(host) = HOST.get() {
        host.leave();
    }
    let left = || outbox::holding() + mail_holding();
    if left() == 0 {
        return;
    }
    mail_leave();
    outbox::leave();
    let start = Instant::now();
    while left() > 0 && start.elapsed() < LEAVE_LIMIT {
        std::thread::sleep(Duration::from_millis(50));
    }
}

const LEAVE_LIMIT: Duration = Duration::from_secs(30);

#[cfg(test)]
mod tests {
    /// Строка корня — та же, что у плагина в его наборе: вид, профиль, файл и адреса по мнению самого плагина; у каждого
    /// плагина набора есть строка.
    #[test]
    fn rows_match_the_plugin_sets() {
        use postman_core::backend::remote::SetEntry;
        let sets: &[(&str, &[SetEntry])] = &[
            ("postman-plugins", postman_plugins::PLUGINS),
            ("postman-plugins-matrix", postman_plugins_matrix::PLUGINS),
            ("postman-plugins-telegram", postman_plugins_telegram::PLUGINS),
        ];
        for row in super::ROWS {
            let Some((_, plugins)) = sets.iter().find(|(set, _)| *set == row.set) else {
                panic!("{}: неизвестный файл набора {}", row.id, row.set)
            };
            let entry = plugins.iter().find(|e| e.id == row.id).unwrap_or_else(|| panic!("{}: нет в наборе {}", row.id, row.set));
            assert_eq!((row.kind, row.profile), (entry.kind, entry.profile), "{}", row.id);
            for url in [
                "postman://demo/me@example.test/c",
                "postman://matrix/@me:example.test/c",
                "postman://telegram/1/c",
                "imaps://mail.example.test",
                "https://mail.example.test/",
                "https://mail.example.test/EWS/Exchange.asmx",
                "ldap://dc.corp.example",
            ] {
                assert_eq!(row.claims(url), entry.claims(url), "{} {url}", row.id);
            }
        }
        for (_, plugins) in sets {
            for e in *plugins {
                assert!(super::ROWS.iter().any(|r| r.id == e.id), "{}: нет строки в корне", e.id);
            }
        }
    }

    /// Адрес со своей схемой — путь IMAP; адрес без схемы и `https://` — ничей (проверит сеть, иначе JMAP).
    #[test]
    fn imap_scheme_is_claimed() {
        let r = super::registry();
        assert!(r.claims("imaps://mail.example.test") && r.claims("IMAP://x"));
        assert!(!r.claims("https://mail.example.test") && !r.claims("mail.example.test"));
        // Адрес EWS — путь Exchange (S7-06г).
        assert!(r.claims("https://mail.example.test/EWS/Exchange.asmx"));
    }
}
