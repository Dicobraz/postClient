//! Набор плагинов `postman-plugins` (ADR 0013): протоколы, слинкованные в один файл; каждый запускается своим
//! процессом `postman-plugins --plugin=<id>`. Здесь — список плагинов набора ([`SetEntry`]: точка входа `serve`,
//! вид, как узнать свой адрес, профиль песочницы — вшит в набор, аргументом его не понизить). Строки тех же плагинов
//! в композиционном корне (`app-gpui/src/plugins.rs`) сверяет с набором тест корня.
//!
//! Общий файл — почта и каталог вместе (решение владельца 2026-11-16: одна копия сетевого стека на все протоколы);
//! каналы Matrix (PLG-04) и Telegram (PLG-05) — отдельными файлами наборов `postman-plugins-matrix` и
//! `postman-plugins-telegram`, компоненты установки. Почтовые протоколы наполняют этот файл (PLG-06…PLG-10).

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
