# PLG-04: Matrix — отдельным процессом

lane: x/plg-matrix         # crates/backend-matrix, crates/plugins, crates/plugins-matrix, core (договор обмена), app-gpui (корень), deps
status: done
deps: [PLG-03]
class: A
files: [crates/backend-matrix/*, crates/plugins/*, crates/plugins-matrix/*, app-gpui/Cargo.toml, app-gpui/src/plugins.rs, app-gpui/src/account_matrix.rs, docs/tasks/CH-05.md, crates/core/tests/layers.rs, Cargo.toml, Cargo.lock, docs/SIZE.md]
read: [docs/adr/0013-plugins-on-demand.md, docs/decisions/2026-11-14-plugins-on-demand.md, docs/adr/0012-channels.md]
checks: [nextest, bench]
manual: yes                 # docs/manual-tests/PLG-04.md

## Задача
- `backend-matrix` — в наборе плагинов, из `postman.exe` уходит: признаки сборки `matrix` и `matrix-release` убираются,
  канал включают только настройка и политика `AllowChannelMatrix`.
- Хранилища SDK (`StateStore`, `CryptoStore` поверх `PluginStore`) — через услугу хранилища по обмену; запись
  очередью, как сейчас.
- Вход через браузер: возврат на `127.0.0.1` слушает ядро и передаёт ответ плагину (песочница входящих не принимает).
- Коды отказов (`BAD_*`) — в `model`/`ChannelError`, интерфейс от крейта плагина не зависит.
- Замер: `postman.exe` и файл с Matrix — строкой в `docs/SIZE.md` (№11 и №12 закрываются: бюджет `postman.exe` не задет).

## Готово
Вход, комнаты, шифрованная переписка, проверка устройства и восстановление ключей — через процесс (ручная проверка);
`tests/conformance.rs` и `tests/e2e.rs` зелёные; `matrix-sdk` нет в дереве зависимостей `postman-gpui`; первый кадр не
хуже прежнего.

## Сделано
- **Свой файл набора** (решение владельца 2026-11-16: общий `postman-plugins` — почта и каталог вместе, Matrix и
  Telegram — каждый своим файлом; правка цитаты — PLG-05): крейт `crates/plugins-matrix`, исполняемый файл
  `postman-plugins-matrix --plugin=matrix`, свой вход в
  реестре набора (`postman_plugins_matrix::{PLUGINS, find}`). Запуск, песочница, сторож и связь — общий
  `set_main` в `postman_core::backend::remote` (общий `main` файла набора); журнал строками с уровнем — в каждом
  файле набора, предел кучи macOS — там же.
- **`backend-matrix`**: точка входа `serve(link)` по договору PLG-02 (`serve_channel`); `postman-gpui` от крейта не
  зависит, признаков `matrix`/`matrix-release` больше нет — канал включают настройка и политика `AllowChannelMatrix`
  (`account_matrix::BUILT = true`); файл набора — компонент установки (PLG-03; в выпуск 0.2.0 не входит): нет файла —
  «проблема → причина» у учётки. В реестре — заместитель по строке `Row` (`app-gpui/src/plugins.rs`).
- **SSO по договору** (ADR 0013 «Договор»): возврат браузера на `127.0.0.1` слушает ядро и передаёт токен плагину —
  листенер переехал из `backend-matrix::signin` в `postman_core::backend::remote::sso` (хост), плагин просит порт и
  ждёт токен услугой по обмену (`FromPlugin::Sso`, `SsoOp::{Listen, Wait}`; `FlowCtx::sso` — трейт `channel::Sso`,
  в процессе ядра его нет). Ожидание (до 5 минут) — в потоке пула, рантайм плагина не стоит.
- **Коды отказов** `BAD_*` — в `postman_core::channel::matrix` (слова интерфейса — Fluent, как были); `backend-matrix`
  их реэкспортирует, интерфейс берёт из ядра.
- **Хранилища SDK** — как были: свои `StateStore`/`CryptoStore` поверх `PluginStore`; по обмену это услуга
  хранилища ядра (PLG-02), запись очередью в базе профиля — без изменений кода канала.
- **Тесты**: договор через обмен — `crates/backend-matrix/tests/remote.rs` (вход паролем и браузером с SSO-услугой,
  подключения, догон, `parse_link`/`link`/`person_url`, неверный секрет) на подставном сервере `fake` (loopback,
  внешней сети нет); сквозной через настоящий процесс в песочнице — `crates/plugins-matrix/tests/process.rs` (вход,
  подключение, хранилище в базе хоста, убитый процесс поднимается; Windows — только запуск и отказ неизвестному
  плагину: AppContainer не подключается к своей машине, изоляция loopback, как в `crates/proc/tests/network.rs`);
  листенер SSO — юнит-тесты `remote::sso`; wire-тесты «туда и обратно» дополнены SSO-сообщениями.
- **Слои и полосы**: `layers.rs` — `postman-plugins-matrix` (L3p), `postman-backend-matrix` убран из
  `LINKED_FOR_NOW` (`interface_does_not_link_plugins` — список короче на один), наборы — оба корня без интерфейса;
  `lanes.toml` — `crates/plugins-matrix/*` в полосе `backends`.
- Вне `files:` (без них не собирается и не проверяется): `crates/core/src/channel/plugin.rs`, `mod.rs`,
  `conformance.rs` (поле `FlowCtx::sso`), `crates/core/src/backend/wire.rs`, `remote.rs`, `remote/serve.rs` (услуга
  SSO и `set_main`), `crates/backend-matrix/Cargo.toml` (dev-зависимость `postman-proc`),
  `crates/plugins/tests/process.rs` и `app-gpui/src/account_telegram.rs` (те же `FlowCtx`),
  `crates/backend-telegram/*` (то же), `lanes.toml`.

## Не сделано
- Ручная проверка на настоящем сервере Matrix (вход, комнаты, шифрованная переписка, проверка устройства) —
  `docs/manual-tests/PLG-04.md` (файл набора уже в пакетах установки — PLG-03).
- Windows и macOS: сетевая часть сквозного теста процесса пропускается (изоляция loopback у AppContainer; macOS —
  Seatbelt), полный прогон — ручная проверка.
- Замер памяти живого плагина Matrix (`plugins_rss_mb`) — с ручной проверкой на настоящем сервере; в тестах процесса
  рабочий набор спрашивается и больше нуля (Linux).
- Файл набора в пакетах установки (MSI, `.app`, deb/rpm) — PLG-03: файл собран (`postman-plugins-matrix`), в выпуск
  0.2.0 не входит.
