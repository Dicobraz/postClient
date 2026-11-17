//! Набор `postman-plugins-telegram` (ADR 0013, PLG-05): канал Telegram — отдельным файлом набора (решение
//! владельца 2026-11-16: общий набор — почта и каталог вместе, Telegram и Matrix — каждый своим файлом, компонент
//! установки). Каждый плагин набора запускается своим процессом `postman-plugins-telegram --plugin=<id>`; здесь —
//! список плагинов ([`SetEntry`]), как в общем наборе `crates/plugins`. Строку плагина в композиционном корне
//! (`app-gpui/src/plugins.rs`) сверяет с набором тест корня.

use postman_core::backend::remote::{Claim, Profile, SetEntry};
use postman_core::backend::wire::Kind;

/// Имя файла набора без расширения.
pub const SET: &str = "postman-plugins-telegram";

/// Плагины набора.
pub const PLUGINS: &[SetEntry] = &[SetEntry {
    id: postman_backend_telegram::ID,
    kind: Kind::Channel,
    profile: Profile::Network { enterprise_auth: false },
    claim: Claim::Channel,
    // Ключ приложения (`api_id`/`api_hash`) — в сборке плагина; есть ли он — плагин сообщает в `Hello` (PLG-05).
    serve: postman_backend_telegram::serve,
}];

/// Плагин набора по id.
pub fn find(id: &str) -> Option<&'static SetEntry> {
    PLUGINS.iter().find(|e| e.id == id)
}
