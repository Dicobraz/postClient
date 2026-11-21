# AUD-03: окно «Отменить» у писем-шагов поручения (Delegate) исчезло

lane: x/aud-core
status: done
deps: []
class: A                   # регресс S6-11: шаг поручения у IMAP уходил сразу, «Отменить» не было
files: [crates/core/src/backend/mail.rs]
read: [docs/specs/S6.md, docs/decisions/2026-11-14-messenger-comforts.md]
checks: [nextest]
manual: no                 # тесты рядом с существующими очереди (hold, withhold, release)

## Задача
- В очереди ядра `MailQueue` (PLG-08) есть ветка только для `Send`: `Command::Delegate` уходил пути сразу
  (у IMAP шаг шёл по SMTP без окна), и интерфейсу нечего было показать в окне «Отменить» шага (S6-11:
  «Забрать», «Удалить поручение»).
- Добавить ветку по образцу `Send`: письма шага держит очередь ядра с окном, release по тику; ответ —
  `Event::Delegated` со списком отложенных (по нему интерфейс шлёт `Withhold`).

## Сделано
- `command()`: `Delegate` с `Release::After/At` — письма шага в очереди ядра (`MailQueue::hold`, у каждого
  свой ключ `send.queue<аккаунт>/hold.N`), интерфейсу сразу `Event::Delegated { error: None, scheduled }`;
  с `Release::Now` и у путей с `Feature::ServerSchedule` (AUD-02, сервер держит сам по FUTURERELEASE) —
  в путь как есть.
- Release по тику не менялся: срок вышел — письма уходят путям обычной отправкой (`Send` с внутренними
  билетами), подтверждение размечает очередь; «Отменить» шага (`Withhold`) убирает письма из очереди и из
  базы до срока, опоздание — fallback в путь (`too_late`).
- Тесты: `delegate_steps_wait_in_the_undo_window` (hold, путь не получает, `Withhold` убирает и очередь, и
  базу) и `delegate_steps_leave_when_the_window_is_over` (по сроку — обычная отправка, подтверждение
  размечает очередь).

## Не сделано
- Ничего; правки задач перед письмами шага и остаются там, где были: у IMAP+DAV их делает ядро
  (`backend::areas::delegate`), у JMAP — плагин (путь с фичей получает `Delegate` как есть).
