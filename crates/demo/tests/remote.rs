//! Демо-канал через обмен (ADR 0013, PLG-02): заместитель хоста и сторона плагина говорят сообщениями `wire` по
//! кадрам `postman-proc`, плагин — в потоке этого теста (настоящий процесс — `crates/plugins/tests`). Проверки договора
//! каналов проходят так же, как без обмена: сериализация, услуга хранилища, вход, сессия, файлы.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // интеграционный тест: «не падать» — для кода приложения (S2-03)

use std::io::pipe;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use postman_core::backend::remote::{Claim, Host, Launcher, Profile, Row, Started};
use postman_core::backend::wire::{Kind, Settings};
use postman_core::channel::conformance::{Harness, Script, run_all};
use postman_core::channel::{AccountId, Answer, Secret};
use postman_demo::channel::{ACCOUNT, CODE, DemoChannel, DemoServer, ID, PASSWORD, SESSION};
use postman_proc::link::Link;

const ROW: Row =
    Row { id: ID, kind: Kind::Channel, claim: Claim::Channel, set: "postman-plugins", profile: Profile::Network { enterprise_auth: false } };

/// Сторона плагина — поток теста с общим сервером: сценарий «событий от других» играет тот же сервер.
struct InThread(DemoServer);

impl Launcher for InThread {
    fn launch(&self, _row: &Row) -> std::io::Result<Started> {
        let (from_plugin, plugin_out) = pipe()?;
        let (plugin_in, to_plugin) = pipe()?;
        let server = self.0.clone();
        std::thread::spawn(move || {
            let link = Link::new(plugin_in, plugin_out, postman_core::backend::remote::MAX_BIN).unwrap();
            postman_core::backend::remote::serve_channel(link, ID, move || Arc::new(DemoChannel::new(server)))
        });
        Ok(Started {
            link: Link::new(from_plugin, to_plugin, postman_core::backend::remote::MAX_BIN)?,
            kill: Box::new(|| {}),
            exit_code: Box::new(|| None),
            background: Box::new(|_| Ok(())),
        })
    }
}

#[test]
fn demo_channel_passes_conformance_over_the_wire() {
    let server = DemoServer::new();
    let host = Host::new(Arc::new(InThread(server.clone())), Settings::default());
    host.first_frame();
    let script: Rc<dyn Script> = Rc::new(server);
    let h = Harness {
        plugin: Arc::new(host.channel(ROW)),
        account: AccountId::new(ACCOUNT),
        secret: Secret::new(SESSION.to_vec()),
        sign_in: Some(vec![Answer::Text("+70000000000".into()), Answer::Text(CODE.into()), Answer::Secret(Secret::new(PASSWORD.to_vec()))]),
        script: Some(script),
        wait: Duration::from_secs(5),
    };
    let report = run_all(&h);
    assert!(report.ok(), "{report}");
    assert_eq!(report.skipped, ["features_are_honest"], "{report}");
    assert!(report.passed.contains(&"extra") && report.passed.contains(&"upload_and_send") && report.passed.contains(&"run_loop"), "{report}");
    assert_eq!(host.launches(), 1, "один процесс на все учётки и входы");
    // Вопросы описания — у поднятого процесса.
    assert_eq!(h.plugin.features(), postman_demo::channel::all_features());
    assert!(h.plugin.parse_link("https://chat.example.test/g-team/m7").is_some());
}
