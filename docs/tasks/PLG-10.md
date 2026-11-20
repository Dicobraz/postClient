# PLG-10: JMAP — отдельным процессом

lane: x/plg-jmap         # crates/backend-jmap, crates/plugins, app-gpui (корень)
status: done
deps: [PLG-09]
class: A
files: [crates/backend-jmap/*, crates/plugins/*, crates/core/src/backend/*, app-gpui/Cargo.toml, app-gpui/src/plugins.rs, app-gpui/src/signin.rs, crates/core/tests/layers.rs, Cargo.toml, Cargo.lock, docs/SIZE.md, docs/MEMORY.md]
read: [docs/adr/0013-plugins-on-demand.md, docs/decisions/2026-11-14-plugins-on-demand.md]
checks: [nextest, bench]
manual: yes                 # docs/manual-tests/PLG-10.md

## Задача
- `backend-jmap` — в наборе плагинов, на услугах ядра из PLG-08; из `postman.exe` уходит.
- Основной путь переносится последним. Push (EventSource), выгрузка и перенос между учётками (`Import`,
  `ServerCopy`) — в процессе плагина.
- `settings::ONE_TEXT_ONLY` — в `model::ErrorKind`; интерфейс от крейта плагина не зависит.
- После волны в `postman.exe` нет сетевого стека протоколов (reqwest, rustls) — проверить деревом зависимостей
  и замером.
- Сторож скорости: `first_frame_ms`, `first_sync_ms`, `list_ms` до и после — первый кадр не хуже, первая
  синхронизация не хуже чем на 10 %.

## Готово
JMAP — процессом общего набора (`postman-plugins --plugin=jmap`): почта, календарь, контакты, задачи, правила,
автоответ, «Отправить позже» сервером, push (EventSource), выгрузка (`Import`) и перенос сервером
(`Email/copy`) — события и команды той же сессии; `postman-backend-jmap` и `postman-jmap` нет в дереве
`postman-gpui` (`layers::interface_does_not_link_plugins`, `LINKED_FOR_NOW` пуст) — в `postman.exe`
сетевого стека протоколов больше нет. Услуги ядра по обмену: хранилище учётки [`MailCache`] — +18 операций
JMAP (patch_emails, missing_bodies, oldest, set_keywords, set_recipients, remove_state; задачи — tasks,
dirty_tasks, task, task_uid, put_tasks, remove_tasks, replace_tasks; связи бесед — xref_set, xref_drop,
xref_sources, xref_targets, keyword_prefix; + upsert_folder из PLG-09), воркер и S/MIME — как были;
**поисковый индекс — услугой** ([`MailIndex`], [`FromPlugin::Index`]): строки писем — ядру без ответа, текст и
tantivy — у ядра ([`LocalIndex`] в потоке, [`NopIndex`] путям без поиска, [`RemoteIndex`] в процессе);
`tasks::handle` и связи бесед — обобщены ([`postman_cache::TaskCache`]: путь в процессе подставляет
[`MailTasks`] поверх обмена, ядро и пути в потоке — [`Cache`]). Вход — логин с паролем **или токен**
([`MailLogin::bearer`], `Auth::Bearer`); `mail_login` больше не отвергает токены. Политика TLS 1.3 — процесс
читает настройки ядра на входе ([`remote::settings`]); смена политики доезжает до живых процессов настройками
хоста ([`Host::set_settings`] — приложение пушит их при смене, раньше настройки получал только запуск).
`settings::ONE_TEXT_ONLY` — константой в `postman_core::settings` (интерфейс сверяет с ней, крейта плагина не
знает; род ошибки в договор не заводили — ответ настроек текстовый, как `smime-sign:` коды). Заместитель —
строка `JMAP` в корне ([`Claim::None`]: своего адреса нет, вид сервера — `Probe` по сети
`/.well-known/jmap`), первым в реестре на пробу. WIRE 6 (`MailLogin.bearer`, `FromPlugin::Index`,
операции хранилища JMAP). Тесты: путь настоящим процессом в песочнице (`plugins/tests/jmap.rs`: отказ входа
к мёртвому серверу по обмену, строка набора), wire — новые операции туда и обратно, стенд `stand*` —
прежним входом (`spawn` с локальными услугами) — зелёные.

## Не сделано
- Ручная проверка на живом стенде The Post (почта, календарь, контакты, задачи, правила, автоответ, «Отправить
  позже», push, общие ящики) — `docs/manual-tests/PLG-10.md`, владельцу: нужны учётки стенда (`POSTMAN_STAND_*`);
  автотесты стенда без них пропускаются.
- Замер №21 (`docs/SIZE.md`, release j2) и финальный nextest с полной песочницей — school-99, отдельно после
  зелёного CI (догоняющим коммитом, как в PLG-05…09).
- Сторож скорости (`first_frame_ms`, `first_sync_ms`, `list_ms` до/после) — не снят: нужны учётки стенда.
- Смена политики TLS 1.3 доезжает до живого процесса только на его следующий вход сессии (процесс читает
  настройки ядра на входе; слинкованные пути получали её сразу) — до перезапуска процесса старые подключения
  живут по старой политике.
