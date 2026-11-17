# PLG-05: Telegram — отдельным процессом

lane: x/plg-telegram         # crates/backend-telegram, crates/plugins, app-gpui (корень, вход), deps
status: done
deps: [PLG-04]
class: A
files: [crates/backend-telegram/*, crates/plugins/*, app-gpui/Cargo.toml, app-gpui/src/plugins.rs, app-gpui/src/account_telegram.rs, crates/core/tests/layers.rs, Cargo.toml, Cargo.lock, docs/SIZE.md]
read: [docs/adr/0013-plugins-on-demand.md, docs/decisions/2026-11-14-plugins-on-demand.md, docs/adr/0012-channels.md]
checks: [nextest, bench]
manual: yes                 # docs/manual-tests/PLG-05.md

## Задача
- `backend-telegram` — в наборе плагинов, из `postman.exe` уходит; признак сборки `telegram` убирается.
- Ключ приложения (`api_id`/`api_hash`) остаётся в плагине; есть ли он — плагин сообщает в `Hello`, `api_key` из
  интерфейса не зовётся.
- Вход по QR, коду и паролю двухэтапной проверки — потоком `Prompt`/`Answer` по обмену.
- Замер `postman.exe` до и после — в `docs/SIZE.md`.

## Готово
Вход, диалоги, история, отправка, правки, файлы — через процесс (ручная проверка — `docs/manual-tests/PLG-05.md`,
владелец); тесты договора зелёные (`conformance` + через обмен `tests/remote.rs` + процесс набора
`crates/plugins/tests/telegram.rs`, 1416 тестов двух пакетов `nextest --workspace`, clippy `-D warnings` и
`fmt --check` чисты, school-99 Linux); `grammers-*` нет в дереве зависимостей `postman-gpui`
(`interface_does_not_link_plugins`: `backend-telegram` вне `LINKED_FOR_NOW`); пакеты трёх ОС работают с каналом
(файл набора `postman-plugins` — компонент установки с PLG-03; MSI/пакеты соберёт CI).

## Сделано
- **Общий набор** (решение владельца 2026-11-16: почта+каталог+Telegram — один `postman-plugins`, Matrix — свой):
  `backend-telegram::serve` в `PLUGINS` набора `postman-plugins --plugin=telegram`; `postman-gpui` от крейта не
  зависит, признака `telegram` нет — канал включают настройка и политика `AllowChannelTelegram`
  (`account_telegram::BUILT = true`); в реестре — заместитель по строке `Row`.
- **Ключ приложения — у плагина** (карточка, ADR 0013 «Договор»): `PluginHello` сообщает `key: bool` (версия
  договора `WIRE` 2), хост помнит ответ процесса, заместитель отдаёт панели канала (`plugins::telegram_has_key`);
  интерфейс у `api_key()` не зовёт. До первого подъёма процесса ответ неизвестен — вход показываем, отказ
  «нет ключа» возвращает сам плагин (`ChannelError::Denied(no-api-key)`, тем же кодом, что и без обмена).
- **Коды отказов и довесок — в ядре**: `postman_core::channel::telegram` — слова `NO_API_KEY`/`BAD_*`/`SIGN_UP` и
  формат довеска сообщения (кнопки ботов, опросы, альбомы; JSON, версия 1) — как `BAD_*` у Matrix (PLG-04):
  интерфейс (`account_telegram`, `chat_extra`) от крейта плагина не зависит; перевод TL ↔ довесок — в плагине
  (`backend-telegram::extra`).
- **Вход без изменений кода канала**: телефон → код → пароль (SRP) и QR (`tg://login`) идут потоком
  `Prompt`/`Answer` по обмену (`Ask::SignIn`/`Ask::Step`); QR рисует интерфейс (`qrcodegen` остался у app-gpui —
  это картинка из `Prompt::Qr`, не протокол).
- **Тесты**: через обмен — `crates/backend-telegram/tests/remote.rs` (конформанс `run_all`, вход кодом/паролем и
  QR, подключение и догон, `parse_link`/`link`/`person_url`, отказ без ключа у настоящей точки входа `serve`);
  настоящий процесс — `crates/plugins/tests/telegram.rs` (`Hello` без ключа и отказ входа через обмен — сети не
  нужно, работает и на Windows; строка набора); wire-тесты «туда и обратно» дополнены `PluginHello.key`.
- **Слои и замер**: `layers.rs` — `postman-backend-telegram` убран из `LINKED_FOR_NOW`
  (`interface_does_not_link_plugins` короче на один); `docs/SIZE.md` — строка №16 (Linux x86_64, school-99):
  до/после `postman` и рост файла набора.
- Вне `files:` (без них не собирается и не проверяется): `crates/core/src/channel/mod.rs`, `plugin.rs` (модуль
  `channel::telegram`), `crates/core/src/backend/wire.rs`, `remote.rs` (`PluginHello.key`, `RemoteChannel::has_key`),
  `remote/serve.rs` (`serve_channel` с ключом в `Hello`), `crates/backend-matrix/src/lib.rs` и
  `crates/demo/src/channel.rs` (+`tests/remote.rs`) — тот же параметр, `app-gpui/src/chat_extra.rs` (довесок из
  ядра), `docs/manual-tests/PLG-05.md`.

## Не сделано
- Ручная проверка на настоящем Telegram (вход телефоном и QR, диалоги, история, отправка, правки, файлы, перезапуск
  процесса) — `docs/manual-tests/PLG-05.md`, владелец: нужен аккаунт и сборка с ключом приложения
  (`POSTMAN_TG_API_*` при сборке набора).
- Полный вход настоящим процессом в автотестах не проверяется: подставной сервер `fake` живёт в памяти теста, а
  процесс набирает сеть сам; вход по обмену проверяет `tests/remote.rs`, отказ без ключа настоящим процессом —
  `plugins/tests/telegram.rs`.
- Замер памяти живого плагина (`plugins_rss_mb`) — с ручной проверкой; в тестах процесса рабочий набор спрашивается
  и больше нуля (Linux).
- dev-сборка без файла набора рядом (`cargo run -p postman-gpui`): канал включён настройкой, но вход упрётся в
  «проблема → причина» до первой сборки набора (`cargo build -p postman-plugins`) — то же, что у Matrix (PLG-04).
