//! Набор плагинов `postman-plugins` (ADR 0013): протоколы, слинкованные в один файл; каждый запускается своим
//! процессом `postman-plugins --plugin=<id>`. Здесь — список плагинов набора ([`SetEntry`]: точка входа `serve`,
//! вид, как узнать свой адрес, профиль песочницы — вшит в набор, аргументом его не понизить). Строки тех же плагинов
//! в композиционном корне (`app-gpui/src/plugins.rs`) сверяет с набором тест корня.
//!
//! Тяжёлый канал Matrix — отдельным файлом набора `postman-plugins-matrix` (решение владельца 2026-11-16, PLG-04);
//! Telegram и остальные лёгкие каналы — этим файлом.

use postman_core::backend::remote::{Claim, Profile, SetEntry};
use postman_core::backend::wire::Kind;

/// Имя файла набора без расширения.
pub const SET: &str = "postman-plugins";

/// Плагины набора.
pub const PLUGINS: &[SetEntry] = &[SetEntry {
    id: postman_demo::channel::ID,
    kind: Kind::Channel,
    // Демо сети не нужно, но профиль у плагинов один (рантайм tokio и потоки связи): «сеть без файлов».
    profile: Profile::Network { enterprise_auth: false },
    claim: Claim::Channel,
    serve: postman_demo::channel::serve,
}];

/// Плагин набора по id.
pub fn find(id: &str) -> Option<&'static SetEntry> {
    PLUGINS.iter().find(|e| e.id == id)
}
