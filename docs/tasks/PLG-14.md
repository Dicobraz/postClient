# PLG-14: каталог компонентов и «компонент не установлен»

lane: x/plg-14-components  # core (договор каналов), app-gpui (корень, экраны каналов)
status: todo
deps: [PLG-13]
class: B                   # поведение хоста и договор ошибки канала
files: [crates/core/src/channel/mod.rs, crates/core/src/backend/remote.rs, crates/core/src/backend/wire.rs, app-gpui/src/plugins.rs, app-gpui/src/account_telegram.rs, app-gpui/src/account_matrix.rs, app-gpui/src/chats.rs, app-gpui/i18n/ru/channels.ftl, app-gpui/i18n/en/channels.ftl]
read: [docs/adr/0013-plugins-on-demand.md, docs/decisions/2026-11-19-components.md, docs/decisions/2026-11-16-plugin-files.md]
checks: [nextest]
manual: yes                # docs/manual-tests/PLG-14.md: MSI с CHANNELS=none — состояние канала

## Задача
- Хост ищет файл набора в двух местах (решение владельца 2026-11-19 «components»): рядом с `postman.exe`
  (файл установки важнее) и в каталоге компонентов `<data>\Postman\Components` — туда PLG-15 положит докачанное.
  Нет нигде — учётка получает честное состояние «компонент не установлен» вместо вечной «проблемы» с текстом
  «plugin set … not found».
- Новая ошибка `ChannelError::Missing(файл)` (вариант в конце перечисления: `ChannelState::Problem` хранится в
  базе postcard, порядок прежних вариантов не менять); текст — Fluent `comp-missing`, оба языка.

## Сделано
- `crates/core/src/backend/remote.rs`: `Failed::Missing` из отказа запуска «нет файла»; `problem()` принимает
  `ChannelError`; «компонент не установлен» — как «сборки не совпали»: без перезапусков, до «Повторить»
  (плагин появится — «Повторить» поднимет); тест `missing_set_is_a_problem_until_retry` по образцу теста сборки.
- `crates/core/src/backend/wire.rs`: пример `Missing` в тесте круговорота вариантов по проводу.
- `app-gpui/src/plugins.rs`: `Beside` → `Sets` — каталог из `choose_dir` (установка → компоненты → установка),
  тест выбора (кроссплатформенный суффикс exe).
- Тексты: ветки `Missing` в трёх разборах (`account_telegram`, `account_matrix`, `chats`), ключ `comp-missing`
  в `channels.ftl` (ru/en).

## Не сделано (адресно)
- Кнопка установки и докачка из выпуска — PLG-15; офлайн «из файла» — PLG-16; пакеты без файлов каналов и
  компонентные ассеты — PLG-17 (до них состояние видно только при установке MSI с `CHANNELS=none`, PLG-13).
