//! Путь EWS — плагин ядра (ADR 0004, S7-06г): учётная запись Exchange on-prem целиком по EWS (решение владельца
//! 2026-10-23 вечер — почта Exchange по EWS, не через IMAP/SMTP; `docs/EXCHANGE.md`). Тот же договор команд и событий,
//! что у JMAP и IMAP (`postman_core::sync::Command`, `Event`), на том же кэше; интерфейс не знает, что сервер — Exchange.
//!
//! - **Почтовый путь** ([`EwsPlugin`]): адрес учётной записи — URL EWS (`https://…/EWS/Exchange.asmx`). Вид сервера
//!   по адресу — EWS узла (или `autodiscover.<узел>`) просит Negotiate/NTLM без входа; при входе — Autodiscover POX
//!   с логином и паролем учётной записи (логин — адрес почты), он даёт точный URL EWS. Синхронизация почты — [`sync`].
//! - **Путь областей** ([`EwsAreaPlugin`], ключ `ews`): календарь, ответы на встречи, занятость, справочник (GAL) —
//!   [`areas`]. У учётной записи EWS области подключает сам почтовый путь; у IMAP-учётки на Exchange —
//!   `imaps://…?ews=https://…/EWS/Exchange.asmx`.
//! - Вход — пароль учётной записи из диспетчера учётных данных Windows (`model::Auth::Basic`) через SSPI
//!   (Negotiate/NTLM); пустой пароль — вход учётной записью Windows без пароля (учётные данные текущего входа:
//!   Kerberos, вне домена — NTLM, T1-02а). Адрес ящика — у сервера (`ResolveNames` своей учётки), не из логина.
//!   TLS — SChannel (`postman-ews`).

pub mod areas;
pub mod sync;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use postman_core::backend::mail::{MailServer, MailServices};
use postman_core::backend::remote::{AreaServer, AreaSession, FetchedBody, PluginLink, serve_mail_area};
use postman_core::backend::wire::MailLogin;
use postman_core::backend::{AreaPlugin, Areas, BoxFuture, Login, MailPlugin, Spawn};
use postman_core::model::{Auth, Error as CoreError, ErrorKind};
use postman_core::sync::{Account, Command, Event, Stage, SyncError, UnboundedSender};
use tokio::sync::mpsc::UnboundedReceiver;

pub use areas::EwsAreas;

/// Интервал между запросами у клиентов, которые путь создаёт дальше (наносекунды); `u64::MAX` — по умолчанию,
/// [`postman_ews::MIN_INTERVAL`] (предел боевого Exchange).
static REQUEST_INTERVAL: AtomicU64 = AtomicU64::new(u64::MAX);

/// Тесты с подменным сервером: паузы между запросами (200 мс) — без них. На боевом сервере не вызывается: предел
/// частоты запросов — [`postman_ews::MIN_INTERVAL`].
#[doc(hidden)]
pub fn set_request_interval(d: Duration) {
    REQUEST_INTERVAL.store(u64::try_from(d.as_nanos()).unwrap_or(u64::MAX - 1), Ordering::Relaxed);
}

/// Клиент EWS пути: [`postman_ews::Client::new`] с интервалом запросов из [`set_request_interval`].
pub(crate) fn client(url: &str, auth: postman_ews::Auth) -> Result<postman_ews::Client, postman_ews::Error> {
    let mut client = postman_ews::Client::new(url, auth)?;
    let ns = REQUEST_INTERVAL.load(Ordering::Relaxed);
    if ns != u64::MAX {
        client.set_min_interval(Duration::from_nanos(ns));
    }
    Ok(client)
}

/// Путь EWS в адресе: `…/EWS/Exchange.asmx`.
const EWS_PATH: &str = "/ews/exchange.asmx";

/// Адрес учётной записи EWS: `https://узел[:порт]/EWS/Exchange.asmx`.
pub fn is_ews_url(url: &str) -> bool {
    let u = url.trim().to_ascii_lowercase();
    u.starts_with("https://") && u.split('?').next().unwrap_or_default().trim_end_matches('/').ends_with(EWS_PATH)
}

/// Узел адреса без схемы и пути (`mail.example.test`); с путём или портом — `None` (это не «просто узел»).
fn bare_host(url: &str) -> Option<&str> {
    let rest = url.trim().split_once("://").map_or(url.trim(), |(_, r)| r);
    let host = rest.trim_end_matches('/');
    (!host.is_empty() && !host.contains(['/', '?', ':', ' '])).then_some(host)
}

/// Где искать EWS по введённому узлу: сам узел, затем `autodiscover.<узел>` (адрес — домен почты).
fn candidates(host: &str) -> [String; 2] {
    [postman_ews::autodiscover::fallback_url(host), postman_ews::autodiscover::fallback_url(&format!("autodiscover.{host}"))]
}

/// Вход EWS из учётной записи договора: только логин и пароль (SSPI), токенов у EWS нет.
pub fn ews_auth(a: &Account) -> Option<(String, String, postman_ews::Auth)> {
    match &a.auth {
        Auth::Basic { user, password } => Some((user.clone(), password.clone(), auth(user, password))),
        Auth::Bearer(_) => None,
    }
}

/// Пароль пустой — вход учётной записью Windows (Kerberos), иначе явный.
pub fn auth(user: &str, password: &str) -> postman_ews::Auth {
    if password.is_empty() { postman_ews::Auth::Windows } else { postman_ews::Auth::password(user, password) }
}

/// Ошибка входа (PLG-08): отказ входа учётной записью Windows — род `WindowsSignInUnavailable` (интерфейс предложит
/// войти с паролем), отклонённые логин и пароль — `CredentialsRejected` (подсказка про вид логина); прочее — как есть.
pub fn sign_in_error(stage: Stage, a: &postman_ews::Auth, e: &postman_ews::Error) -> SyncError {
    let mut err = sync_error(stage, e);
    if matches!(a, postman_ews::Auth::Windows) && err.kind == ErrorKind::Auth {
        err.kind = ErrorKind::WindowsSignInUnavailable;
    } else if err.kind == ErrorKind::Auth {
        err.kind = ErrorKind::CredentialsRejected;
    }
    err
}

/// Ошибка EWS → род ошибки договора.
pub fn kind(e: &postman_ews::Error) -> ErrorKind {
    use postman_ews::Error as E;
    match e {
        E::Auth(_) | E::AuthScheme | E::Sspi { .. } => ErrorKind::Auth,
        E::Io(_) | E::Tls(_) | E::Http(_) | E::Closed | E::Timeout => ErrorKind::Network,
        E::Status(s) if *s >= 500 => ErrorKind::Network,
        E::Ews { .. } | E::ServerBusy { .. } | E::Status(_) => ErrorKind::Server,
        E::Url(_) | E::Xml(_) | E::TooLarge | E::Autodiscover(_) => ErrorKind::Protocol,
    }
}

/// Ошибка EWS → ошибка договора областей (`model::Error`): отказ сервера — с кодом EWS.
pub fn core_error(e: postman_ews::Error) -> postman_core::model::Error {
    match &e {
        postman_ews::Error::Ews { code, .. } => {
            postman_core::model::Error::Method { method: "EWS".into(), kind: code.clone(), description: String::new() }
        }
        _ => postman_core::model::Error::other(kind(&e), &e),
    }
}

/// Ошибка EWS → ошибка синхронизации.
pub fn sync_error(stage: Stage, e: &postman_ews::Error) -> SyncError {
    SyncError { kind: kind(e), stage, detail: e.to_string() }
}

/// Плагин почтового пути EWS для реестра (`postman_core::backend::Registry`).
#[derive(Debug, Default, Clone, Copy)]
pub struct EwsPlugin;

impl MailPlugin for EwsPlugin {
    fn name(&self) -> &'static str {
        "ews"
    }

    fn claims(&self, url: &str) -> bool {
        is_ews_url(url)
    }

    /// Exchange по введённому узлу (адрес без схемы и пути): EWS узла или `autodiscover.<узел>` без входа просит
    /// Negotiate/NTLM → адрес учётной записи — этот URL EWS (точный — Autodiscover при входе).
    fn probe<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Option<String>> {
        Box::pin(async move {
            let host = bare_host(url)?;
            for c in candidates(host) {
                if postman_ews::probe(&c).await {
                    return Some(c);
                }
            }
            None
        })
    }

    fn set_tls13_only(&self, on: bool) {
        postman_ews::set_tls13_only(on);
    }

    fn spawn(&self, s: Spawn) -> UnboundedSender<Command> {
        match ews_auth(&s.account) {
            Some((user, password, _)) => sync::spawn(s, user, password),
            None => {
                let detail = "EWS signs in with a login and password only".to_owned();
                let _ = s.events.send(Event::Error(SyncError { kind: ErrorKind::Auth, stage: Stage::Start, detail }));
                (s.repaint)();
                tokio::sync::mpsc::unbounded_channel().0
            }
        }
    }
}

/// Плагин областей EWS (календарь, ответы на встречи, занятость, справочник): ключ адреса — `ews`.
#[derive(Debug, Default, Clone, Copy)]
pub struct EwsAreaPlugin;

impl AreaPlugin for EwsAreaPlugin {
    fn key(&self) -> &'static str {
        "ews"
    }

    /// EWS на узле учётной записи: `https://<узел>/EWS/Exchange.asmx` просит Negotiate/NTLM.
    fn probe<'a>(&'a self, host: &'a str) -> BoxFuture<'a, Option<String>> {
        Box::pin(async move {
            let url = postman_ews::autodiscover::fallback_url(host);
            postman_ews::probe(&url).await.then_some(url)
        })
    }

    fn set_tls13_only(&self, on: bool) {
        postman_ews::set_tls13_only(on);
    }

    fn connect<'a>(
        &'a self,
        url: &'a str,
        login: &'a Login,
        _profile: Option<&'a str>,
    ) -> BoxFuture<'a, postman_core::model::Result<Box<dyn Areas>>> {
        Box::pin(async move {
            match EwsAreas::connect(url, &login.user, &login.password, &login.email).await {
                Ok(a) => Ok(Box::new(a) as Box<dyn Areas>),
                Err(e) => Err(core_error(e)),
            }
        })
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Путь EWS отдельным процессом (PLG-08, ADR 0013): `postman-plugins --plugin=ews`. Почта и свои области — один
// процесс и один сетевой стек; хранилище учётки, разбор письма и S/MIME — услуги ядра по обмену
// ([`postman_core::backend::mail`]).

/// Идентификатор плагина пути EWS в наборе (`postman-plugins`, PLG-08); тот же в строке композиционного корня
/// (`app-gpui/src/plugins.rs`) — совпадение сверяет тест корня.
pub const ID: &str = "ews";

/// Путь EWS на стороне процесса: вид сервера и запуск синхронизации учётки с услугами ядра ([`MailServices`]).
#[derive(Debug, Default, Clone, Copy)]
pub struct EwsServer;

impl MailServer for EwsServer {
    /// Exchange по введённому узлу: EWS узла или `autodiscover.<узел>` ([`EwsPlugin::probe`]).
    fn probe<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Option<String>> {
        EwsPlugin.probe(url)
    }

    /// Запустить синхронизацию учётной записи: свой поток с рантаймом, как у пути в ядре; события — хосту
    /// (`FromPlugin::MailEvent`), команды — из канала. `Err` — поток не завёлся.
    fn open(
        &self,
        url: String,
        profile: Option<String>,
        login: MailLogin,
        services: MailServices,
        commands: UnboundedReceiver<Command>,
        notify: Arc<dyn Fn(Event) + Send + Sync>,
    ) -> BoxFuture<'_, Result<(), String>> {
        let user = login.user;
        let password = String::from_utf8_lossy(login.password.expose()).into_owned();
        let spawned = std::thread::Builder::new().name(format!("sync-{ID}")).spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(e) => return notify(Event::Error(SyncError::internal(format!("tokio runtime: {e}")))),
            };
            rt.block_on(async move {
                sync::run(url, user, password, profile, services, commands, &*notify).await;
            });
        });
        Box::pin(async move {
            spawned.map_err(|e| format!("sync thread: {e}"))?;
            Ok(())
        })
    }
}

/// Путь областей на стороне процесса (PLG-08): календарь и контакты учётки IMAP на Exchange (`?ews=…`) — тот же
/// процесс, что и почта Exchange. Публичных загрузок по ссылке у EWS нет (занятость и календарь — учёткой).
#[derive(Debug, Default, Clone, Copy)]
pub struct EwsAreaServer;

impl AreaServer for EwsAreaServer {
    fn probe<'a>(&'a self, host: &'a str) -> BoxFuture<'a, Option<String>> {
        EwsAreaPlugin.probe(host)
    }

    fn connect<'a>(&'a self, url: &'a str, login: &'a Login, _profile: Option<&'a str>) -> BoxFuture<'a, Result<Box<dyn AreaSession>, CoreError>> {
        Box::pin(async move {
            // Политику TLS процесс берёт из настроек ядра (`Hello` и смены), как у областей в наборе (PLG-07).
            EwsAreaPlugin.set_tls13_only(postman_core::backend::remote::settings().tls13_only);
            match EwsAreas::connect(url, &login.user, &login.password, &login.email).await {
                Ok(a) => Ok(Box::new(a) as Box<dyn AreaSession>),
                Err(e) => Err(core_error(e)),
            }
        })
    }

    fn fetch<'a>(&'a self, _url: &'a str, _max: u32, _etag: Option<&'a str>) -> BoxFuture<'a, Result<Option<FetchedBody>, String>> {
        Box::pin(async { Err("EWS does not serve public resources".to_owned()) })
    }
}

/// Точка входа процесса: `postman-plugins --plugin=ews` (PLG-08). Ответ — код выхода.
pub fn serve(link: PluginLink) -> i32 {
    serve_mail_area(link, ID, Some(Arc::new(EwsServer)), || Arc::new(EwsAreaServer))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ews_urls() {
        assert!(is_ews_url("https://mail.example.test/EWS/Exchange.asmx"));
        assert!(is_ews_url("https://mail.example.test:8443/ews/exchange.asmx/"));
        assert!(!is_ews_url("http://mail.example.test/EWS/Exchange.asmx"), "только https");
        assert!(!is_ews_url("imaps://mail.example.test") && !is_ews_url("https://mail.example.test"));
        assert_eq!(bare_host("example.test"), Some("example.test"));
        assert_eq!(bare_host("https://example.test/"), Some("example.test"));
        assert_eq!(bare_host("example.test:443"), None);
        assert_eq!(bare_host("example.test/jmap"), None);
        assert_eq!(
            candidates("example.test"),
            ["https://example.test/EWS/Exchange.asmx".to_owned(), "https://autodiscover.example.test/EWS/Exchange.asmx".to_owned()]
        );
    }

    #[test]
    fn errors_keep_their_kind() {
        assert_eq!(kind(&postman_ews::Error::Auth("x".into())), ErrorKind::Auth);
        assert_eq!(kind(&postman_ews::Error::Timeout), ErrorKind::Network);
        assert_eq!(kind(&postman_ews::Error::Status(503)), ErrorKind::Network);
        let e = core_error(postman_ews::Error::Ews { code: "ErrorItemNotFound".into(), message: "addr@example.test".into() });
        assert!(matches!(&e, postman_core::model::Error::Method { kind, .. } if kind == "ErrorItemNotFound"));
        assert!(!e.to_string().contains("addr@"), "текст сервера (бывают адреса) — не в ошибку");
        assert!(matches!(auth("u", ""), postman_ews::Auth::Windows));
    }

    /// Отказ входа — своим родом: учётной записью Windows — «войти с паролем», отклонённые логин и пароль —
    /// подсказка про вид логина; сеть — не отказ входа.
    #[test]
    fn windows_sign_in_refusal_has_a_reason() {
        let e = postman_ews::Error::Sspi { call: "AcquireCredentialsHandleW", code: 0x8009_030e };
        let w = sign_in_error(Stage::Connect, &postman_ews::Auth::Windows, &e);
        assert_eq!(w.kind, ErrorKind::WindowsSignInUnavailable);
        assert!(w.kind.is_auth());
        let p = sign_in_error(Stage::Connect, &auth("u", "p"), &postman_ews::Error::Auth("credentials rejected".into()));
        assert_eq!(p.kind, ErrorKind::CredentialsRejected);
        let net = sign_in_error(Stage::Connect, &postman_ews::Auth::Windows, &postman_ews::Error::Timeout);
        assert_eq!(net.kind, ErrorKind::Network, "сеть — не отказ входа");
        assert!(!net.kind.is_auth());
    }
}
