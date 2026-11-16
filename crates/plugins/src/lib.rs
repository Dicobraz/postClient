//! Набор плагинов `postman-plugins` (ADR 0013): протоколы, слинкованные в один файл; каждый запускается своим
//! процессом `postman-plugins --plugin=<id>`. Здесь — список плагинов набора: точка входа `serve(link)`, вид и профиль
//! песочницы (вшит в набор: аргументом его не понизить). Строки тех же плагинов в композиционном корне
//! (`app-gpui/src/plugins.rs`) сверяет с набором тест корня.
//!
//! В этой волне (PLG-02) в наборе — демо-канал; остальные плагины пока внутри `postman.exe`.

use std::sync::Arc;

use postman_core::backend::remote::{PluginLink, Profile};
use postman_core::backend::wire::Kind;
use postman_core::channel::{ChannelPlugin, SCHEME};

/// Имя файла набора без расширения.
pub const SET: &str = "postman-plugins";

/// Сам плагин, без процесса: для сверки строки корня с плагином.
#[derive(Clone, Copy)]
pub enum Plugin {
    Channel(fn() -> Arc<dyn ChannelPlugin>),
}

/// Плагин набора.
pub struct Entry {
    pub id: &'static str,
    pub kind: Kind,
    pub profile: Profile,
    /// Точка входа процесса: связь с хостом уже открыта, песочница заперта. Ответ — код выхода.
    pub serve: fn(PluginLink) -> i32,
    pub plugin: Plugin,
}

impl Entry {
    /// Адрес — этого плагина по мнению самого плагина: у канала — `postman://<его id>/…`.
    pub fn claims(&self, url: &str) -> bool {
        match self.plugin {
            Plugin::Channel(make) => url.to_ascii_lowercase().starts_with(&format!("{SCHEME}{}/", make().id())),
        }
    }
}

fn demo() -> Arc<dyn ChannelPlugin> {
    Arc::new(postman_demo::channel::DemoChannel::default())
}

/// Плагины набора.
pub const PLUGINS: &[Entry] = &[Entry {
    id: postman_demo::channel::ID,
    kind: Kind::Channel,
    // Демо сети не нужно, но профиль у плагинов один (рантайм tokio и потоки связи): «сеть без файлов».
    profile: Profile::Network { enterprise_auth: false },
    serve: postman_demo::channel::serve,
    plugin: Plugin::Channel(demo),
}];

/// Плагин набора по id.
pub fn find(id: &str) -> Option<&'static Entry> {
    PLUGINS.iter().find(|e| e.id == id)
}
