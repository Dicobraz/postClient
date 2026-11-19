# PLG-07: сериализация команд и событий, области через ядро, DAV — отдельным процессом

lane: x/plg-areas           # core (договор), backend-dav, app-gpui
status: done
deps: [PLG-06]
class: B                    # договор sync::Command / sync::Event и путей областей (ADR 0004)
files: [crates/core/src/sync.rs, crates/core/src/model.rs, crates/core/src/backend/*, crates/core/src/schedule.rs, crates/core/src/settings.rs, crates/core/src/tasks/*, crates/backend-dav/*, crates/plugins/*, app-gpui/Cargo.toml, app-gpui/src/plugins.rs, app-gpui/src/remote_images.rs, app-gpui/src/meeting.rs, app-gpui/src/calendar/subscriptions.rs, crates/core/tests/layers.rs, Cargo.toml, Cargo.lock]
read: [docs/adr/0013-plugins-on-demand.md, docs/decisions/2026-11-14-plugins-on-demand.md, docs/adr/0004-protocol-plugins.md]
checks: [nextest, bench]
manual: yes                 # docs/manual-tests/PLG-07.md: календарь и контакты IMAP + CalDAV/CardDAV, картинки письма, подписки

## Задача
- `serde` на `sync::Command` (86 вариантов), `sync::Event` (71) и вложенных типах; тест «туда и обратно» на каждый
  вариант. Смысл договора не меняется.
- Области через ядро: команды областей учётки разводит ядро (сначала область — «не моя, назад», затем почтовый
  путь), `Features` объединяет ядро, опрос `changed` раз в 2 минуты — таймер ядра. Почтовые пути внутри
  `postman.exe` (до своих волн) работают с областью-процессом так же.
- `backend-dav` — в наборе плагинов; задачи поверх кэша (`tasks::handle`) — услугой ядра.
- Загрузка по открытой ссылке (картинки письма, подписки на календари, внешняя занятость) — командой плагина DAV;
  `fetch_public`, `fetch_changed`, `public_url_ok` из интерфейса не зовутся. Процесс стартует по первой загрузке и
  завершается после 5 минут без учёток с областью и без загрузок.

## Готово
Области (календарь, задачи, занятость, приглашения) и DAV — через процесс набора `postman-plugins --plugin=dav`;
сериализация команд/событий — с волны PLG-07-serde (PR #11). Тесты: CI трёх ОС зелёный (clippy `-D warnings`, fmt,
CodeQL), school-99 Linux с полной песочницей — **1591 тест** (`nextest --workspace`; подмножество процессных и
песочных крейтов — отдельным прицельным прогоном после замера), локально `postman-core` 395/395. `postman-dav` нет
в дереве зависимостей `postman-gpui` (`layers::interface_does_not_link_plugins`). Замер — `docs/SIZE.md` №18
(`postman` 79,38 → **79,07 МиБ**, набор 5,54 → **11,24 МиБ**). Ручная проверка — `docs/manual-tests/PLG-07.md`,
владелец.

## Сделано
- **Области через ядро**: заместитель `RemoteAreasSession` (`crates/core/src/backend/remote/area.rs`) разводит
  команды учётки — сначала области («не моя — назад», `Reply::AreaCommand(Ok(false))`), затем почтовый путь;
  `Features` областей объединяет ядро; опрос `changed` раз в 2 минуты — таймер ядра (`AreaChanged` по обмену).
- **`backend-dav` — в общий набор** (`crates/plugins`, решение владельца 2026-11-16: почта и каталог вместе):
  `SetEntry { kind: Kind::Area, claim: Claim::Area("dav"), profile: Network }`, `serve_area` с точкой входа
  `backend_dav::serve`; упаковка не тронута — набор уже в пакетах трёх ОС (PLG-03).
- **Загрузка по открытой ссылке** — `Ask::Fetch` процессу DAV (картинки письма, подписки календарей, внешняя
  занятость; условный запрос по `etag`); интерфейс зовёт заместители `plugins::fetch`/`fetch_changed`
  (`public_url_ok` — строкой описания плагина). Процесс стартует по первой загрузке и завершается по 5 минутам
  простоя (`IDLE_EXIT`, как PLG-06), пока нет учёток с областью.
- **Задачи поверх кэша — услуга ядра**: кэш у ядра, хранилище плагина — операциями `TaskOp` по обмену
  (`WireTasks`), выполнение — `tasks::handle` в ядре.
- **Договор областей по обмену** (`wire`, PLG-07): `AreaOpen`/`AreaCommand`/`AreaRespond`/`AreaChanged`/
  `AreaTask`/`Probe`/`Fetch`, события сессии — `FromPlugin::AreaEvent` (тот же `sync::Event`, что у почты);
  крупное (`SyncCommand`, `Invitation`) — в `Box`.
- **Сессия области в процессе — рабочая задача-владелец** (`serve.rs`): очередь `Job` и один `area_worker` на
  сессию, операции строго по одной — как локальный раннер ядра; `RefCell` через `await` не держится (нет
  повторного входа).
- **Тесты**: через обмен и настоящий процесс — `plugins/tests/dav.rs` (песочница «сеть без файлов», вход к
  закрытому серверу — отказ договора; строка набора сверена с мнением плагина); сторона процесса с `fake`-DAV —
  `backend-dav/tests/remote.rs` и `core backend::remote::area::tests` (вход, развод команд, ответ на приглашение,
  задачи по операциям, загрузка по ссылке, простой); wire «туда и обратно» дополнен областными вопросами.

## Не сделано
- Ручная проверка на живом стенде (календарь и контакты учётки IMAP + CalDAV/CardDAV, картинки письма, подписки,
  `first_sync_ms` учётки «стандарты» до/после) — `docs/manual-tests/PLG-07.md`, владельцу: нужны учётки стенда
  (`POSTMAN_STAND_*`); автотесты стенда без них пропускаются.
- Паника в задаче процесса (например, внутри `spawn_local` у `serve_area`) завершает задачу молча — заместитель
  дождётся сторожа, но не получит ответа; защита «паника → ответ ошибкой» (`catch_unwind` на границе задачи) —
  отдельно, до PLG-12 приёмки.
