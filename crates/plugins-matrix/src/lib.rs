//! Набор `postman-plugins-matrix` (ADR 0013, PLG-04): тяжёлый канал Matrix — отдельным файлом набора (решение
//! владельца 2026-11-16: общий набор — почта, календарь, каталог и Telegram; Matrix ~16 МиБ — свой файл, компонент
//! установки). Каждый плагин набора запускается своим процессом `postman-plugins-matrix --plugin=<id>`; здесь —
//! список плагинов ([`SetEntry`]), как в общем наборе `crates/plugins`. Строку плагина в композиционном корне
//! (`app-gpui/src/plugins.rs`) сверяет с набором тест корня.

use postman_core::backend::remote::{Claim, Profile, SetEntry};
use postman_core::backend::wire::Kind;

/// Имя файла набора без расширения.
pub const SET: &str = "postman-plugins-matrix";

/// Плагины набора.
pub const PLUGINS: &[SetEntry] = &[SetEntry {
    id: postman_backend_matrix::ID,
    kind: Kind::Channel,
    profile: Profile::Network { enterprise_auth: false },
    claim: Claim::Channel,
    serve: postman_backend_matrix::serve,
}];

/// Плагин набора по id.
pub fn find(id: &str) -> Option<&'static SetEntry> {
    PLUGINS.iter().find(|e| e.id == id)
}
