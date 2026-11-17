//! `postman-plugins-telegram --plugin=telegram` (ADR 0013, PLG-05): процесс плагина Telegram. Запуск, песочница, сторож
//! и связь — общий [`set_main`](postman_core::backend::remote::set_main) ядра; здесь — журнал строками с уровнем в
//! stderr (хост пишет их в журнал приложения) и предел кучи macOS. Своих файлов процесс не пишет.

use std::sync::OnceLock;

use postman_core::backend::remote::{self, set_main};
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::reload;
use tracing_subscriber::util::SubscriberInitExt as _;

/// Предел кучи на macOS: `RLIMIT_AS` там не действует — считает распределитель.
#[cfg(target_os = "macos")]
#[global_allocator]
static ALLOC: postman_proc::sandbox::Limited = postman_proc::sandbox::Limited;

static LEVEL: OnceLock<reload::Handle<LevelFilter, tracing_subscriber::Registry>> = OnceLock::new();

/// Наибольший уровень из фильтра `POSTMAN_LOG` (`warn`, `postman=debug,info`); пусто или не разобрать — `info`.
fn level(filter: &str) -> LevelFilter {
    filter.split(',').filter_map(|d| d.rsplit('=').next()?.trim().parse::<LevelFilter>().ok()).max().unwrap_or(LevelFilter::INFO)
}

fn set_level(filter: &str) {
    if let Some(h) = LEVEL.get()
        && let Err(e) = h.reload(level(filter))
    {
        eprintln!("WARN {}: log level not set: {e}", postman_plugins_telegram::SET);
    }
}

/// Журнал — строками `УРОВЕНЬ текст` в stderr; уровень — из `Hello` и смены настройки (`reload`).
fn init_log() {
    let (filter, handle) = reload::Layer::new(LevelFilter::INFO);
    let fmt = tracing_subscriber::fmt::layer().with_writer(std::io::stderr).with_ansi(false).without_time().with_target(false);
    if tracing_subscriber::registry().with(filter).with(fmt).try_init().is_ok() {
        let _ = LEVEL.set(handle);
        remote::on_log(set_level);
    }
}

fn main() {
    std::process::exit(set_main(postman_plugins_telegram::SET, postman_plugins_telegram::find, init_log));
}
