//! Канал Matrix (ADR 0012, CH-05): плагин `ChannelPlugin` ядра поверх `matrix-sdk` (Apache-2.0). Канал — только
//! протокол: вход паролем или через браузер (SSO, возврат по loopback — слушает ядро и передаёт токен плагину,
//! ADR 0013; [`signin`]), сессия (секрет ядра — [`secret`]), sync с курсором и поток событий, история, поиск на
//! сервере, действия, файлы (`session`), перевод событий в общую модель и обратно ([`convert`]), проверка устройства
//! и резервная копия ключей ([`verify`]).
//!
//! Отдельным процессом (PLG-04, ADR 0013): файл набора `postman-plugins-matrix`, точка входа — [`serve`]; из
//! `postman.exe` канал ушёл, в реестре — заместитель. Хранилища и услуги — по обмену с ядром.
//!
//! - **Сквозное шифрование** — в клиенте (`vodozemac`): не расшифровалось — `MessageKind::Undecryptable`, пришёл
//!   ключ (другое устройство, резервная копия) — то же сообщение заново, тот же `MessageId`.
//! - **Хранилища SDK** — свои, в базе профиля через `PluginStore` ([`store`]): состояние комнат и ключи шифрования,
//!   зашифрованные ключом из секрета сессии; ни SQLite, ни файлов (решение «всё в базе»).
//! - **Сеть** — только для учёток, которые добавил пользователь, и по его действию (вход); ни телеметрии, ни
//!   запросов при старте без учётки. Мосты к другим сетям — не в клиенте (ADR 0012 «Что не входит»).
//! - **Тесты без сети**: подставной сервер `fake` (признак `testkit`) отвечает как Client-Server API на loopback;
//!   канал проходит `postman_core::channel::conformance` (`tests/conformance.rs`) и те же проверки через обмен
//!   (`tests/remote.rs`).

use std::sync::Arc;

use postman_core::backend::BoxFuture;
use postman_core::backend::remote::{PluginLink, serve_channel};
use postman_core::channel::{
    AccountId, Address, ChannelError, ChannelFeatures, ChannelId, ChannelPlugin, ChannelSession, ConnectCtx, ConversationId, E2e, Flow, FlowCtx,
    Limit, MessageId, Participant, Reactions,
};

mod client;
pub mod convert;
#[cfg(feature = "testkit")]
pub mod fake;
pub mod links;
pub mod secret;
mod session;
pub mod signin;
pub mod store;
pub mod verify;

/// Id канала: адрес `postman://matrix/…`, ключ политики `AllowChannelMatrix`.
pub const ID: &str = "matrix";

/// Коды отказов входа и проверок в `ChannelError::Denied` — слова интерфейса (Fluent) в ядре
/// (`postman_core::channel::matrix`), интерфейс от крейта плагина не зависит (PLG-04).
pub use postman_core::channel::matrix::{
    BAD_LOGIN, BAD_PASSWORD, BAD_RECOVERY_KEY, BAD_SERVER, BROWSER_TIMEOUT, NO_CROSS_SIGNING, NO_LOGIN_FLOW, VERIFY_CANCELLED,
};

/// Точка входа процесса набора `postman-plugins-matrix --plugin=matrix` (ADR 0013, PLG-04): канал отвечает хосту за
/// себя; клиент и рантайм — при первом настоящем вопросе.
pub fn serve(link: PluginLink) -> i32 {
    serve_channel(link, ID, || Arc::new(MatrixChannel::new()))
}

/// Длина текста: событие Matrix — не больше 64 КиБ JSON; с запасом на разметку и шифр.
pub const MAX_TEXT: u32 = 16_000;

/// Что умеет Matrix (ADR 0012 «Проверка договора»): правка и удаление у всех — всегда (по правам комнаты), любые
/// реакции, «прочитано», «печатает», ответы, упоминания, ветки, закрепление (тег `m.favourite`), поиск и счётчики
/// сервера, пространства — папками, шифрование по выбору комнаты. Цитаты фрагмента, пересылки сервером, отложенной
/// отправки и черновиков на сервере в протоколе нет; заглушить (правила push) — пока в базе профиля.
pub fn features() -> ChannelFeatures {
    ChannelFeatures {
        edit: Limit::Always,
        delete_for_all: Limit::Always,
        reactions: Reactions::Any,
        read_receipts: true,
        typing: true,
        reply_to: true,
        quote_fragment: false,
        mentions: true,
        forward_native: false,
        scheduled_send: false,
        server_drafts: false,
        server_mute: false,
        server_pin: true,
        server_search: true,
        server_unread: true,
        folders: true,
        threads: true,
        e2e: E2e::Optional,
        max_attachment: None,
        max_text: Some(MAX_TEXT),
        // Довесок (`Action::Extra`: опросы MSC3381) — позже.
        extra: false,
    }
}

/// Канал Matrix.
#[derive(Default)]
pub struct MatrixChannel {
    accounts: Arc<std::sync::Mutex<Vec<AccountId>>>,
}

impl MatrixChannel {
    pub fn new() -> Self {
        Self::default()
    }

    fn add_account(&self, a: &AccountId) {
        if let Ok(mut v) = self.accounts.lock()
            && !v.contains(a)
        {
            v.push(a.clone());
        }
    }

    fn account(&self) -> Option<AccountId> {
        self.accounts.lock().ok()?.first().cloned()
    }
}

impl ChannelPlugin for MatrixChannel {
    fn id(&self) -> &'static str {
        ID
    }

    fn features(&self) -> ChannelFeatures {
        features()
    }

    fn parse_link(&self, url: &str) -> Option<Address> {
        let link = links::parse(url)?;
        let account = self.account()?;
        let conv = match &link.target {
            links::Target::Room(id) => ConversationId::new(id.clone()),
            // Псевдоним комнаты и человек — через сервер (`Action::Resolve`).
            links::Target::Alias(_) | links::Target::User(_) => return None,
        };
        let channel = ChannelId::new(ID);
        Some(match &link.event {
            Some(e) => Address::message(&channel, &account, &conv, &MessageId::new(e.clone())),
            None => Address::conversation(&channel, &account, &conv),
        })
    }

    fn link(&self, at: &Address) -> Option<String> {
        Some(links::room(at.conversation.as_str(), at.message.as_ref().map(MessageId::as_str)))
    }

    fn person_url(&self, p: &Participant) -> Option<String> {
        links::person(p.id.as_str())
    }

    fn sign_in<'a>(&'a self, ctx: FlowCtx) -> BoxFuture<'a, Result<Box<dyn Flow + 'a>, ChannelError>> {
        Box::pin(async move {
            let flow: Box<dyn Flow + 'a> = Box::new(signin::SignIn::new(ctx.sso));
            Ok(flow)
        })
    }

    fn connect<'a>(&'a self, ctx: ConnectCtx) -> BoxFuture<'a, Result<Box<dyn ChannelSession>, ChannelError>> {
        Box::pin(async move {
            let s = secret::Session::from_secret(&ctx.secret).ok_or_else(|| ChannelError::SignedOut("not a matrix session".into()))?;
            if s.user_id != ctx.account.0 {
                return Err(ChannelError::SignedOut("session of another account".into()));
            }
            self.add_account(&ctx.account);
            let session = session::MxSession::open(s, ctx).await?;
            let session: Box<dyn ChannelSession> = Box::new(session);
            Ok(session)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_round_trip() {
        let p = MatrixChannel::new();
        assert_eq!(p.parse_link("https://matrix.to/#/!room:example.test/$ev1"), None, "нет учётки — нет адреса");
        p.add_account(&AccountId::new("@me:example.test"));
        let a = p.parse_link("https://matrix.to/#/!room:example.test/$ev1?via=example.test").unwrap();
        assert_eq!(a.to_string(), "postman://matrix/@me:example.test/!room:example.test/$ev1");
        assert_eq!(p.link(&a).as_deref(), Some("https://matrix.to/#/!room:example.test/$ev1"));
        assert_eq!(p.parse_link("https://matrix.to/#/#team:example.test"), None, "псевдоним — через сервер");
        let anna = Participant { id: postman_core::channel::ParticipantId::new("@anna:example.test"), ..Participant::default() };
        assert_eq!(p.person_url(&anna).as_deref(), Some("matrix:u/anna:example.test"));
    }
}
