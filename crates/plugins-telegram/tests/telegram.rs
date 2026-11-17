//! Канал Telegram настоящим процессом `postman-plugins-telegram --plugin=telegram` в песочнице «сеть без файлов»
//! (ADR 0013, PLG-05): `Hello` со сверкой сборки и сообщением о ключе приложения; сборка без ключа
//! (`POSTMAN_TG_API_*` не задан, как в тестах CI) — вход отказывает «нет ключа приложения» тем же кодом, что и без
//! процесса. Полный вход (телефон, код, пароль, QR) без сети не проверить настоящим процессом: подставной сервер
//! `fake` живёт в памяти теста — тот же поток обменом проверяет `crates/backend-telegram/tests/remote.rs`; настоящая
//! проверка входа — ручная (`docs/manual-tests/PLG-05.md`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // интеграционный тест: «не падать» — для кода приложения (S2-03)

use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;

use postman_backend_telegram::ID;
use postman_cache::Cache;
use postman_core::backend::remote::{Claim, Host, Launcher, Namespaces, ProcLauncher, Profile, Row, Started};
use postman_core::backend::wire::{Kind, Settings};
use postman_core::channel::telegram as tg;
use postman_core::channel::{AccountId, ChannelError, ChannelId, ChannelPlugin, FlowCtx, LocalStore, PluginStore};

const ROW: Row =
    Row { id: ID, kind: Kind::Channel, claim: Claim::Channel, set: "postman-plugins-telegram", profile: Profile::Network { enterprise_auth: false } };

/// Запуск настоящего набора.
struct Real;

impl Launcher for Real {
    fn launch(&self, row: &Row) -> std::io::Result<Started> {
        let exe = Path::new(env!("CARGO_BIN_EXE_postman-plugins-telegram"));
        ProcLauncher { dir: exe.parent().unwrap().to_owned(), namespaces: Namespaces::Optional }.launch(row)
    }
}

fn store(cache: &Cache) -> Rc<dyn PluginStore> {
    Rc::new(LocalStore::new(cache.clone(), &ChannelId::new(ID), &AccountId::new("9990001")))
}

/// Настоящий процесс без ключа приложения: `Hello` сообщает отсутствие ключа, вход — отказ «нет ключа» через обмен.
/// Сборке дан ключ — проверка пропускается: настоящий набор вышел бы в сеть (вход — ручная проверка).
#[test]
fn telegram_without_a_key_refuses_over_a_real_process() {
    if postman_backend_telegram::api_key().is_some() {
        eprintln!("сборка с ключом приложения: отказ «нет ключа» не проверить без сети");
        return;
    }
    let host = Host::new(Arc::new(Real), Settings::default());
    let plugin = host.channel(ROW);
    assert_eq!(host.launches(), 0, "заместитель без запроса процесс не создаёт");
    host.first_frame();
    let cache = Cache::open_in_memory().unwrap();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    tokio::task::LocalSet::new().block_on(&rt, async move {
        assert!(plugin.has_key(), "процесса ещё не было — не знаем, показываем вход");
        let err = plugin.sign_in(FlowCtx { store: store(&cache), sso: None }).await.err().unwrap();
        assert!(matches!(err, ChannelError::Denied(ref c) if c == tg::NO_API_KEY), "{err:?}");
        assert_eq!(host.launches(), 1);
        assert!(!plugin.has_key(), "Hello процесса: ключа нет");
    });
}

/// Строка набора — как мнение самого плагина: вид, профиль, адреса.
#[test]
fn set_entry_matches_the_row() {
    let entry = postman_plugins_telegram::find(ID).unwrap();
    assert_eq!((entry.kind, entry.profile), (ROW.kind, ROW.profile));
    assert!(entry.claims("postman://telegram/9990001/c123") && !entry.claims("postman://demo/a/b"));
}
