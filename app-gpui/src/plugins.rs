//! Пути к серверам (ADR 0004, S7-02б): композиционный корень. Здесь — какие плагины протоколов знает Postman и в
//! каком порядке: почтовые пути — JMAP (путь по умолчанию: адрес без своей схемы), затем EWS Exchange (S7-06г:
//! адрес — URL EWS; узел без схемы — EWS узла или `autodiscover.<узел>`), затем IMAP + SMTP; пути областей — CalDAV и
//! CardDAV, EWS (у IMAP-учётки на Exchange); каналы бесед — Telegram (признак `telegram`). Новый протокол — плагин-крейт
//! и строка здесь; `postman-core` и экраны не меняются.
//!
//! Плагины отдельными процессами (ADR 0013): строка плагина ([`ROWS`]) — идентификатор, вид, как узнать свой адрес,
//! файл набора и профиль песочницы, без запуска процесса; процессы ведёт хост ([`host`]) — после первого кадра и только
//! по запросу. В этой волне так работает демо-канал; остальные плагины — внутри `postman.exe`, как были.

use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use postman_core::backend::Registry;
use postman_core::backend::remote::{Claim, Host, Launcher, ProcLauncher, Profile, Row, Started};
use postman_core::backend::wire::{Kind, Settings};

/// Демо-канал — набором `postman-plugins` (PLG-02): без сети, для тестов и замера.
const DEMO: Row = Row {
    id: postman_demo::channel::ID,
    kind: Kind::Channel,
    claim: Claim::Channel,
    set: "postman-plugins",
    profile: Profile::Network { enterprise_auth: false },
};

/// Плагины отдельными процессами.
pub(crate) const ROWS: &[Row] = &[DEMO];

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
/// поставке (PLG-03) — в процессе, как прежде.
pub(crate) fn demo_remote(screens: bool) -> bool {
    !screens && (cfg!(debug_assertions) || std::env::var_os(postman_core::bench::PLUGINS_VAR).is_some())
}

/// Реестр плагинов процесса.
pub(crate) fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        let r = Registry::default()
            .with_mail(postman_backend_jmap::JmapPlugin)
            .with_mail(postman_backend_ews::EwsPlugin)
            .with_mail(postman_backend_imap::ImapPlugin)
            .with_area(postman_backend_dav::DavPlugin)
            .with_area(postman_backend_ews::EwsAreaPlugin);
        // Каналы бесед (ADR 0012 п. 12): Telegram — признаком сборки `telegram`; включает настройка и политика
        // (`account_telegram`), без учётки сети нет.
        #[cfg(feature = "telegram")]
        let r = r.with_channel(postman_backend_telegram::TelegramChannel::new());
        // Matrix (CH-05) — признаком `matrix`: в dev-сборке всегда, в release — с `matrix-release` (`account_matrix`).
        #[cfg(all(feature = "matrix", any(debug_assertions, feature = "matrix-release")))]
        let r = r.with_channel(postman_backend_matrix::MatrixChannel::new());
        r
    })
}

/// Автоответ записан, но сервер хранит один текст всем (отказ в своём скрипте `vacation`, плагин JMAP): это не ошибка
/// записи, а ограничение, о котором надо сказать пользователю.
pub(crate) fn autoreply_limited(error: &str) -> bool {
    error == postman_backend_jmap::settings::ONE_TEXT_ONLY
}

/// Выход: письма в окне «Отменить» держат потоки IMAP и Exchange, сообщения каналов — очереди отправки потоков
/// каналов (ADR 0012 п. 11): они уходят сейчас, процесс их дожидается. Окна уже нет: не дольше [`LEAVE_LIMIT`], сервер
/// без ответа не держит процесс вечно.
pub(crate) fn finish_sends() {
    use postman_core::channel::outbox;
    if let Some(host) = HOST.get() {
        host.leave();
    }
    let left = || postman_backend_imap::holding() + postman_backend_ews::sync::holding() + outbox::holding();
    if left() == 0 {
        return;
    }
    postman_backend_imap::leave();
    postman_backend_ews::sync::leave();
    outbox::leave();
    let start = Instant::now();
    while left() > 0 && start.elapsed() < LEAVE_LIMIT {
        std::thread::sleep(Duration::from_millis(50));
    }
}

const LEAVE_LIMIT: Duration = Duration::from_secs(30);

#[cfg(test)]
mod tests {
    /// Строка корня — та же, что у плагина в наборе: вид, профиль, файл и адреса по мнению самого плагина; у каждого
    /// плагина набора есть строка.
    #[test]
    fn rows_match_the_plugin_set() {
        for row in super::ROWS {
            let entry = postman_plugins::find(row.id).unwrap_or_else(|| panic!("{}: нет в наборе", row.id));
            assert_eq!(row.set, postman_plugins::SET);
            assert_eq!((row.kind, row.profile), (entry.kind, entry.profile), "{}", row.id);
            for url in ["postman://demo/me@example.test/c", "postman://telegram/1/c", "imaps://mail.example.test", "https://mail.example.test/"] {
                assert_eq!(row.claims(url), entry.claims(url), "{} {url}", row.id);
            }
        }
        for e in postman_plugins::PLUGINS {
            assert!(super::ROWS.iter().any(|r| r.id == e.id), "{}: нет строки в корне", e.id);
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
