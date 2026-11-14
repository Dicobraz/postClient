//! `postman-plugins --plugin=<id>` (ADR 0013): процесс одного плагина. До первого сообщения — только запирание
//! песочницы (профиль — из набора) и ответ на `Hello`; рантайм и сам плагин — при первом настоящем вопросе хоста.
//! Своих файлов процесс не пишет: состояние плагина — услуга ядра по обмену.

use std::sync::OnceLock;

use postman_core::backend::remote::{self, MAX_BIN, PluginLink};
use postman_proc::sandbox;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::reload;
use tracing_subscriber::util::SubscriberInitExt as _;

/// Предел кучи на macOS: `RLIMIT_AS` там не действует — считает распределитель.
#[cfg(target_os = "macos")]
#[global_allocator]
static ALLOC: postman_proc::sandbox::Limited = postman_proc::sandbox::Limited;

/// `/proc/self/status`, открытый до запирания: после него Landlock файлов не откроет, а читать открытый можно.
#[cfg(target_os = "linux")]
static STATUS: OnceLock<std::fs::File> = OnceLock::new();

/// Рабочий набор процесса (`VmRSS`) для `plugins_rss_mb`; не вышло — 0.
#[cfg(target_os = "linux")]
fn rss() -> u64 {
    use std::os::unix::fs::FileExt as _;
    let Some(f) = STATUS.get() else { return 0 };
    let mut buf = [0u8; 4096];
    let n = f.read_at(&mut buf, 0).unwrap_or(0);
    let status = String::from_utf8_lossy(&buf[..n]);
    status.lines().find_map(|l| l.strip_prefix("VmRSS:")?.trim().strip_suffix("kB")?.trim().parse::<u64>().ok()).map_or(0, |kb| kb * 1024)
}

static LEVEL: OnceLock<reload::Handle<LevelFilter, tracing_subscriber::Registry>> = OnceLock::new();

/// Наибольший уровень из фильтра `POSTMAN_LOG` (`warn`, `postman=debug,info`); пусто или не разобрать — `info`.
fn level(filter: &str) -> LevelFilter {
    filter.split(',').filter_map(|d| d.rsplit('=').next()?.trim().parse::<LevelFilter>().ok()).max().unwrap_or(LevelFilter::INFO)
}

fn set_level(filter: &str) {
    if let Some(h) = LEVEL.get()
        && let Err(e) = h.reload(level(filter))
    {
        eprintln!("WARN postman-plugins: log level not set: {e}");
    }
}

/// Журнал — строками `УРОВЕНЬ текст` в stderr (хост пишет их в журнал приложения с меткой плагина).
fn init_log() {
    let (filter, handle) = reload::Layer::new(LevelFilter::INFO);
    let fmt = tracing_subscriber::fmt::layer().with_writer(std::io::stderr).with_ansi(false).without_time().with_target(false);
    if tracing_subscriber::registry().with(filter).with(fmt).try_init().is_ok() {
        let _ = LEVEL.set(handle);
        remote::on_log(set_level);
    }
}

fn main() {
    let id = std::env::args().find_map(|a| a.strip_prefix("--plugin=").map(str::to_owned));
    let Some(entry) = id.as_deref().and_then(postman_plugins::find) else {
        eprintln!("ERROR postman-plugins: unknown plugin {id:?}");
        std::process::exit(2);
    };
    #[cfg(target_os = "linux")]
    if let Ok(f) = std::fs::File::open("/proc/self/status") {
        let _ = STATUS.set(f);
        remote::on_stats(rss);
    }
    if let Err(e) = sandbox::isolate(entry.profile).and_then(|()| sandbox::lockdown(entry.profile)) {
        eprintln!("ERROR postman-plugins: {}{e}", postman_proc::SANDBOX_REFUSAL);
        std::process::exit(postman_proc::EXIT_SANDBOX);
    }
    // Сторож смерти хоста и связь — после запирания: запреты Linux действуют на потоки, созданные после него.
    let _watchdog = postman_proc::watchdog(None);
    init_log();
    let link = match PluginLink::stdio(MAX_BIN) {
        Ok(link) => link,
        Err(e) => {
            eprintln!("ERROR postman-plugins: link: {e}");
            std::process::exit(1);
        }
    };
    std::process::exit((entry.serve)(link));
}
