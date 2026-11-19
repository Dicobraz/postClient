# PLG-09: IMAP + SMTP — отдельным процессом

lane: x/plg-imap         # crates/backend-imap, crates/plugins, app-gpui (корень)
status: done
deps: [PLG-08]
class: A
files: [crates/backend-imap/*, crates/plugins/*, crates/core/src/backend/*, app-gpui/Cargo.toml, app-gpui/src/plugins.rs, app-gpui/src/signin.rs, crates/core/tests/layers.rs, Cargo.toml, Cargo.lock, docs/SIZE.md, docs/MEMORY.md]
read: [docs/adr/0013-plugins-on-demand.md, docs/decisions/2026-11-14-plugins-on-demand.md]
checks: [nextest, bench]
manual: yes                 # docs/manual-tests/PLG-09.md

## Задача
- `backend-imap` — в наборе плагинов, на услугах ядра из PLG-08; из `postman.exe` уходит.
- Окно «Отменить» — уже у ядра (PLG-08); IDLE и отправка SMTP — в процессе плагина.
- Недостающие методы услуги хранилища — по описи PLG-08, без новых механизмов.
- Сторож скорости: `first_frame_ms`, `first_sync_ms`, `list_ms` до и после — первый кадр не хуже, первая
  синхронизация не хуже чем на 10 %.

## Готово
IMAP + SMTP — процессом общего набора (`postman-plugins --plugin=imap`): синхронизация, IDLE на «Входящих»
отдельным соединением и отправка SMTP — в процессе; `postman-backend-imap` нет в дереве `postman-gpui`
(`layers::interface_does_not_link_plugins`, `LINKED_FOR_NOW` без imap). Услуги ядра по обмену: хранилище
учётки ([`MailCache`] + недостающий из описи `upsert_folder` — RENAME перелицовывает ключи писем; `FolderId`
— ответом), воркер разбора (`mime`, `mime_part`, `mime_split` с раскладкой частей, `threads`), S/MIME
`seal_outgoing` (ключи пользователя плагину недоступны); индекса поиска у IMAP нет — поиск на сервере
(`SEARCH`), `Index` в пути не использовался (`_index`). Окно «Отменить» и очередь отправки — у ядра: путь
получает письмо в момент отправки (`Release` всегда «сейчас») и подтверждает итог (`Sent`/`ComposeFailed` с
билетом; ответы на билеты очереди ядро забирает себе, интерфейсу — только её ошибки); `HOLDING`/`LEAVING`,
`leave()`/`holding()` и `held` у пути удалены — выход дожидается очереди ядра. Области учётки (`?dav=…`)
— процесс DAV (PLG-07), областей у самого IMAP нет (`ImapAreaServer` — заглушка для единого входа процесса).
Заместитель — строка `IMAP` в корне (схема-претензия `imap`: обе, `imaps://` и `imap://` STARTTLS), реестр —
`host().mail(IMAP)`. Тесты: путь настоящим процессом в песочнице (`plugins/tests/imap.rs`: отказ входа к
мёртвому серверу по обмену тем же родом, строка набора); `wire` — `UpsertFolder`/`FolderUpserted` туда и
обратно; стенд `stand_imap`/`stand_migrate` — прежним входом (`spawn` с локальными услугами: `LocalCache`,
настоящий воркер, `LocalSeal`) — зелёные.

## Не сделано
- Ручная проверка на живом стенде (почта, папки, IDLE, отправка с «Отменить», перенос, квота) —
  `docs/manual-tests/PLG-09.md`, владельцу: нужны учётки стенда (`POSTMAN_STAND_*`); автотесты стенда без них
  пропускаются.
- Замер №20 (`docs/SIZE.md`, release j2) и финальный nextest с полной песочницей — school-99, отдельно после
  зелёного CI (догоняющим коммитом, как в PLG-05…08).
- Сторож скорости (`first_frame_ms`, `first_sync_ms`, `list_ms` до/после) — не снят: нужны учётки стенда.
- Окно «Отменить» у писем шага поручения (`Command::Delegate` с `Release::After`) — ушло вместе с `held` пути:
  очередь у ядра держит окно только у `Send`; шаги уходят сразу (`Delegated` с пустым `scheduled`). Вернуть —
  отдельной задачей поверх очереди ядра (держать `Delegate` тоже).
- Последовательностный флейк app-тестов на Windows (`invite_card::tests::day_lane_marks_overlap_with_the_invite`
  падает в полной прогонке `cargo test`, проходит один и у nextest; воспроизводится и на main до волны) —
  не в этой волне: чинить отдельно (состояние между тестами, не процесс).
