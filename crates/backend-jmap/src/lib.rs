//! Путь JMAP (Stalwart) — плагин ядра (ADR 0004, S7-02б): синхронизация учётной записи по JMAP тем же договором
//! команд и событий (`postman_core::sync`), что у остальных путей. Почта, календарь, задачи, занятость, контакты,
//! справочник и то, что есть только у JMAP: правила (Sieve), автоответ, квота, таблица категорий (FileNode), общие
//! ящики, «Отправить позже» сервером. Что умеет сервер — по capabilities сессии (`Event::Features`).
//!
//! Перевод типов `postman_jmap` ↔ модель — [`convert`]; общий код (кэш, задачи, правила, автоответ) — `postman_core`.

pub mod categories;
pub mod convert;
pub mod error;
pub mod invite;
pub mod schedule;
pub mod settings;
pub mod sync;
pub mod tasks;

use std::sync::Arc;

use postman_core::backend::mail::{MailServer, MailServices};
use postman_core::backend::remote::{AreaServer, FetchedBody, PluginLink, serve_mail_area};
use postman_core::backend::wire::MailLogin;
use postman_core::backend::{BoxFuture, Login, MailPlugin, Spawn};
use postman_core::model::{Auth, Error as ModelError};
use postman_core::sync::{Account, Command, UnboundedSender};
use tokio::sync::mpsc::UnboundedReceiver;

pub use sync::{spawn, spawn_as};

/// Идентификатор пути в наборе (`postman-plugins --plugin=jmap`, PLG-10).
pub const ID: &str = "jmap";

/// Плагин пути JMAP для реестра (`postman_core::backend::Registry`): путь по умолчанию — адрес без своей схемы.
#[derive(Debug, Default, Clone, Copy)]
pub struct JmapPlugin;

impl MailPlugin for JmapPlugin {
    fn name(&self) -> &'static str {
        "jmap"
    }

    /// Есть ли на узле JMAP (`/.well-known/jmap`, S7-04в).
    fn probe<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Option<String>> {
        Box::pin(async move { postman_jmap::has_jmap(url).await.then(|| url.to_owned()) })
    }

    fn set_tls13_only(&self, on: bool) {
        postman_jmap::set_tls13_only(on);
    }

    fn spawn(&self, s: Spawn) -> UnboundedSender<Command> {
        sync::spawn_as(s.account, s.profile, s.cache, s.index, s.events, s.repaint)
    }
}

/// Путь JMAP на стороне процесса (PLG-10): вид сервера и запуск синхронизации учётки с услугами ядра
/// ([`MailServices`]): хранилище учётки — по обмену (вместе с задачами и связями бесед), поисковый индекс — у
/// ядра, разбор письма — воркером ядра. Календарь, контакты, правила и автоответ — событиями той же сессии.
#[derive(Debug, Default, Clone, Copy)]
pub struct JmapServer;

impl MailServer for JmapServer {
    /// Есть ли на узле JMAP ([`JmapPlugin::probe`]).
    fn probe<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Option<String>> {
        JmapPlugin.probe(url)
    }

    /// Запустить синхронизацию учётной записи: свой поток с рантаймом и общим циклом (`supervise`), как у пути в
    /// ядре; события — хосту (`FromPlugin::MailEvent`), команды — из канала. Вход — логин с паролем или токен
    /// ([`MailLogin::bearer`]). `Err` — поток не завёлся.
    fn open(
        &self,
        url: String,
        profile: Option<String>,
        login: MailLogin,
        services: MailServices,
        commands: UnboundedReceiver<Command>,
        notify: Arc<dyn Fn(postman_core::sync::Event) + Send + Sync>,
    ) -> BoxFuture<'_, Result<(), String>> {
        let auth = match login.bearer {
            Some(t) => Auth::Bearer(String::from_utf8_lossy(t.expose()).into_owned()),
            None => Auth::Basic { user: login.user, password: String::from_utf8_lossy(login.password.expose()).into_owned() },
        };
        let MailServices { store, worker, seal, index } = services;
        let spawned = std::thread::Builder::new().name("sync-jmap".to_owned()).spawn(move || {
            // Политику TLS процесс берёт из настроек ядра (`Hello` и смены), как у областей в наборе (PLG-07).
            postman_jmap::set_tls13_only(postman_core::backend::remote::settings().tls13_only);
            let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(e) => return notify(postman_core::sync::Event::Error(postman_core::sync::SyncError::internal(format!("tokio runtime: {e}")))),
            };
            rt.block_on(async move {
                let mut session = sync::Jmap { account: Account { url, auth }, profile, store, index, worker, seal };
                postman_core::sync::supervise(&mut session, commands, &*notify).await;
            });
        });
        Box::pin(async move {
            spawned.map_err(|e| format!("sync thread: {e}"))?;
            Ok(())
        })
    }
}

/// Областей у JMAP нет: календарь, контакты и задачи учётки ведёт сама сессия пути (события `run`).
#[derive(Debug, Default, Clone, Copy)]
pub struct JmapAreaServer;

impl AreaServer for JmapAreaServer {
    fn probe<'a>(&'a self, _host: &'a str) -> BoxFuture<'a, Option<String>> {
        Box::pin(async { None })
    }

    fn connect<'a>(
        &'a self,
        _url: &'a str,
        _login: &'a Login,
        _profile: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Box<dyn postman_core::backend::remote::AreaSession>, ModelError>> {
        Box::pin(async { Err(ModelError::Protocol("JMAP does not serve areas".into())) })
    }

    fn fetch<'a>(&'a self, _url: &'a str, _max: u32, _etag: Option<&'a str>) -> BoxFuture<'a, Result<Option<FetchedBody>, String>> {
        Box::pin(async { Err("JMAP does not serve public resources".to_owned()) })
    }
}

/// Точка входа процесса: `postman-plugins --plugin=jmap` (PLG-10). Ответ — код выхода.
pub fn serve(link: PluginLink) -> i32 {
    serve_mail_area(link, ID, Some(Arc::new(JmapServer)), || Arc::new(JmapAreaServer))
}
