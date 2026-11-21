# AUD-06: у проблемы канала — «Повторить»

lane: x/aud-ui               # app-gpui (панели каналов, ядро — без правок)
status: done
deps: []
class: A                     # доступность канала: «Настройки → Аккаунты → Каналы» (CH-03, CH-05)
files: [app-gpui/src/account_telegram.rs, app-gpui/src/account_matrix.rs, app-gpui/src/channels.rs, app-gpui/src/chats.rs, app-gpui/i18n/ru/channels.ftl, app-gpui/i18n/en/channels.ftl, app-gpui/i18n/ru/chats.ftl, app-gpui/i18n/en/chats.ftl]
read: [docs/adr/0012-channels.md, docs/adr/0013-plugins-on-demand.md]
checks: [nextest]
manual: no                   # живая проверка — на стенде: падение процесса набора не воспроизвести в тесте

## Задача
- У состояния «проблема» канала (Matrix/Telegram) нет «Повторить»: состояние показывается только текстом.
  Ядро понимает `Request::Reconnect` (`channel::run`: в «проблеме» процесс плагина сам не повторяется — поток
  учётки ждёт этот запрос до суток), интерфейс его не слал — только «Отправить/Получить» почты.
- Ошибка `ChannelError::Plugin` показывалась общим текстом («не удалось…»), без своей причины.

## Сделано
- «Настройки → Аккаунты → Каналы»: у строки учётки в состоянии проблемы — ссылка «Повторить» рядом с «Выйти»
  (Telegram и Matrix): `PanelEvent::Reconnect` → `Channels::retry` → `Request::Reconnect` в поток учётки;
  потока нет (канал выключен) — строка в журнале.
- `ChannelError::Plugin` — свой текст вместо общего: `tg-err-plugin`, `mx-err-plugin`, `chats-err-plugin`
  (ru/en, Fluent `tr!`); текст подсказывает «Повторить».
- Тесты: `retry_sends_reconnect` (запрос уходит в поток учётки, без потока — не уходит); Plugin — своя строка
  в `errors_are_words` (Telegram, Matrix) и `plugin_error_has_its_own_text` (беседы каналов).

## Не сделано (адресно)
- Живая проверка на стенде: «Повторить» после реального падения процесса набора (нужна проблема процесса,
  в тестах не воспроизводится).
