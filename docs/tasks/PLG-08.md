# PLG-08: услуги ядра почтовому пути, EWS — отдельным процессом

lane: x/plg-ews             # core (договор, очередь отправки), backend-ews, app-gpui
status: todo
deps: [PLG-07]
class: B                    # услуга хранилища учётки, окно «Отменить» в ядре
files: [crates/core/src/backend/*, crates/core/src/sync.rs, crates/core/src/send_later.rs, crates/core/src/tasks/*, crates/core/src/invite.rs, crates/core/src/lifecycle.rs, crates/core/src/rules.rs, crates/core/src/migrate.rs, crates/core/src/smime/*, crates/cache/src/*, crates/backend-ews/*, crates/plugins/*, app-gpui/Cargo.toml, app-gpui/src/plugins.rs, app-gpui/src/signin.rs, crates/core/tests/layers.rs, Cargo.toml, Cargo.lock]
read: [docs/adr/0013-plugins-on-demand.md, docs/decisions/2026-11-14-plugins-on-demand.md, docs/adr/0006-single-store.md, docs/decisions/2026-11-14-all-in-db.md]
checks: [nextest, bench]
manual: yes                 # docs/manual-tests/PLG-08.md: Exchange — почта, календарь, вход учётной записью Windows

## Задача
- Первым шагом — опись: какие методы кэша и функции ядра достижимы из почтовых путей (сейчас 143 обращения к
  35 методам, плюс `tasks`, `invite`, `send_later`, `rules`, `lifecycle`, `migrate`). Опись — в карточку.
- Услуга хранилища учётки: эти методы — вызовами по обмену; база открыта только главным процессом. Записи без ответа
  идут подряд, ошибка — на ближайшем вызове с ответом; порядок записей сохраняется.
- Услуги: индекс поиска, разбор письма в воркере (плагин процессов не запускает), подпись и шифрование S/MIME
  (`seal_outgoing`, `sign_outgoing` — у ядра: ключи пользователя плагину недоступны).
- Окно «Отменить» и очередь отправки — в ядре (в базе профиля): плагин получает письмо в момент отправки и
  подтверждает итог; `holding()` и `leave()` плагинов уходят, выход дожидается очереди ядра.
- `backend-ews` — в наборе плагинов (почта и свои области в одном процессе). `is_ews_url` — строкой описания плагина;
  отказы входа (`WINDOWS_SIGN_IN_UNAVAILABLE`, `CREDENTIALS_REJECTED`) — в `model::ErrorKind`.
- Вход учётной записью Windows — в профиле песочницы с корпоративной проверкой подлинности.
- Сторож скорости: `first_sync_ms` и время первой синхронизации демо-ящика Exchange до и после — не хуже чем на 10 %.

## Готово
Exchange — почта, папки, календарь, занятость, справочник, отправка с «Отменить» — через процесс; падение процесса
при письме в окне «Отменить» письмо не теряет (тест); тесты стенда EWS зелёные; `postman-ews` нет в дереве
зависимостей `postman-gpui`.

## Опись

Замер 2026-11-17, `origin/main` 30996c6 (ветка `x/plg08-inventory`). «Почтовые пути» — крейты `crates/backend-*`.
Кэш приходит в `Spawn { cache, index }` и в `AreaCtx { cache }` (`crates/core/src/backend/mod.rs`); сами бэкенды
файл базы не открывают (`Cache::open_in_memory` — только в тестовых модулях). Считались обращения `cache.<метод>(`
в `src/`; тестовые модули `#[cfg(test)]` — отдельной строкой. Файлы: imap — `src/lib.rs`, ews — `src/sync.rs`,
jmap — `src/sync.rs` (иначе — по имени).

### 1. Методы кэша — «услуга хранилища»

**35 методов, 142 обращения** (IMAP 58, EWS 50, JMAP 34; в карточке и ADR 0013 — 143: на main 89bc5f63 у JMAP было
35, одно обращение с тех пор ушло). Производственный код (без тестов): **31 метод, 121 обращение** — IMAP 38,
EWS 50, JMAP 33; тесты добавляют 21 (IMAP 20, JMAP 1), в том числе 4 метода — только тестами (`check_index`,
`check_refs`, `create_folder`, `move_letters`, все — тесты IMAP). Класс всех строк таблицы — «услуга хранилища»:
вызовы по обмену, записи без ответа идут подряд, ошибка — на ближайшем вызове с ответом.

| Метод | Кратко | Произв. | Откуда (репрезентативно) |
|---|---|---|---|
| `upsert_emails` | `(rows: &[EmailRow])` — записать строки списка | 11 | ews sync.rs:783; imap lib.rs:842; jmap sync.rs:2261 |
| `rows_by_ids` | `(ids: &[String]) -> Vec<EmailRow>` | 11 | ews sync.rs:1054, 1162; imap lib.rs:986, 1177; jmap sync.rs:1778 |
| `delete_emails` | `(ids: &[String])` | 10 | ews sync.rs:1032, 1048; imap lib.rs:839; jmap sync.rs:856 |
| `set_state` | `(kind, value)` — состояние синхронизации | 8 | ews sync.rs:786, 1063; imap lib.rs:844; jmap sync.rs:830 |
| `put_body` | `(id, &BodyRow)` — разобранное тело | 7 | ews sync.rs:1161; imap lib.rs:984; jmap sync.rs:677 |
| `state` | `(kind) -> Option<String>` | 6 | ews sync.rs:785; imap lib.rs:785; jmap sync.rs:795 |
| `body` | `(id) -> Option<BodyRow>` | 6 | ews sync.rs:1284; imap lib.rs:380; jmap sync.rs:2263 |
| `remotes` | `(LetterId) -> Vec<RemoteRef>` | 6 | ews sync.rs:1716; imap lib.rs:1655; jmap sync.rs:1341, 1476 |
| `letter_content` | `(LetterId) -> Option<Vec<u8>>` — письмо целиком | 5 | ews sync.rs:1090; imap lib.rs:899; jmap sync.rs:1404 |
| `count` | `(mailbox) -> usize` | 4 | ews sync.rs:217, 1420; imap lib.rs:301; jmap sync.rs:1811 |
| `replace_accounts_of` | `(profile, &[AccountRow])` | 4 | ews sync.rs:513; imap lib.rs:533; jmap sync.rs:312, 370 |
| `mailboxes` | `() -> Vec<MailboxRow>` | 4 | ews sync.rs:541; imap lib.rs:563; jmap sync.rs:1673 |
| `letter_of` | `(account, id сервера) -> Option<LetterId>` | 4 | ews sync.rs:1088, 1279; imap lib.rs:897, 1573 |
| `replace_mailboxes` | `(account, &[MailboxRow])` | 3 | ews sync.rs:1003; imap lib.rs:778; jmap sync.rs:788 |
| `blob` | `(blob_id) -> Option<Vec<u8>>` | 3 | ews sync.rs:1197; imap lib.rs:997; jmap sync.rs:1954 |
| `put_blob` | `(blob_id, bytes)` | 3 | ews sync.rs:1204; imap lib.rs:1004; jmap sync.rs:1057 |
| `folder` | `(FolderId) -> Option<FolderRow>` | 3 | ews sync.rs:1738; imap lib.rs:1582; jmap sync.rs:1347 |
| `missing_bodies` | `(limit) -> Vec<String>` | 3 | jmap sync.rs:391, 471, 550 |
| `page` | `(mailbox, offset, limit)` | 2 | ews sync.rs:218; imap lib.rs:302 |
| `put_content` | `(LetterId, raw, &[SplitPart]) -> Hash` | 2 | ews sync.rs:1114; imap lib.rs:922 |
| `folder_of` | `(account, id сервера) -> Option<FolderId>` | 2 | ews sync.rs:1276; imap lib.rs:368 |
| `letter` | `(LetterId) -> Option<LetterRow>` | 2 | ews sync.rs:1280; imap lib.rs:374 |
| `attach_remote` | `(LetterId, RemoteRef)` | 2 | ews sync.rs:1282; imap lib.rs:376 |
| `upsert_remotes` | `(account, &[(id, LetterRow)]) -> Vec<LetterId>` | 2 | ews sync.rs:1290; imap lib.rs:386 |
| `thread` | `(thread_id) -> Vec<EmailRow>` | 2 | ews sync.rs:1499; imap lib.rs:1351 |
| `upsert_folder` | `(&FolderRow) -> FolderId` | 1 | imap lib.rs:361 |
| `patch_emails` | `(&[Patch]) -> Vec<EmailRow>` | 1 | jmap sync.rs:1705 |
| `oldest` | `(mailbox) -> Option<String>` | 1 | jmap sync.rs:1804 |
| `remove_state` | `(kind)` | 1 | jmap sync.rs:2108 |
| `set_recipients` | `(&[(id, Recipients)])` | 1 | jmap sync.rs:2282 |
| `set_keywords` | `(id, add, remove) -> Option<EmailRow>` | 1 | jmap categories.rs:163 |
| `check_index`, `check_refs`, `create_folder`, `move_letters` | диагностика/папки | 0 (тесты) | imap lib.rs:2030–2050 (`#[cfg(test)]`) |

DAV, LDAP, Telegram, Matrix к методам кэша не обращаются (0): DAV — только `tasks::handle` с `ctx.cache` (п. 2),
LDAP даёт ядру `smime::directory::Directory` (услуга в обратную сторону), каналы — `PluginStore` (ADR 0012).

### 2. Функции ядра — «остаётся в ядре» (услуги ядра по обмену)

| Что | Кратко | Обращений | Откуда |
|---|---|---|---|
| `tasks::handle` | `(store: &impl TaskStore, &Cache, Request, notify)` — движок задач и приглашений поверх кэша (Load/Refresh/Reconcile/Flush/Save/…) | 7 | jmap sync.rs:348, 527, 649, 1204, 1255; dav lib.rs:851, 932 (через `AreaCtx.cache`; команды — imap lib.rs:538, 605) |
| `tasks::TaskStore` | колбек движка в плагин (шаги протокола) | 2 реализ. | jmap tasks.rs:153 (`JmapStore`); dav lib.rs:684 (`DavTasks`) |
| `smime::seal_outgoing` | `(raw, to)` — шифрование исходящего (CNG/СКЗИ) | 4 | ews sync.rs:1888; imap lib.rs:1889; jmap sync.rs:1939, 2008 |
| `smime::sign_outgoing` | `(raw, from, hwnd, now)` — подпись исходящего | 1 | jmap sync.rs:2004 |
| воркер (`postman_worker::Client`) | разбор писем: `mime`, `mime_part`, `mime_split`, `threads` | 7 | imap lib.rs:914, 946, 960, 1003; ews sync.rs:1106, 1136, 1203 |
| `indexer::rows` | `(index, &Cache, &[EmailRow])` — индекс поиска | 1 | jmap sync.rs:2264 |
| окно «Отменить» | `held` в памяти IMAP и EWS (`hold`, `release_held`, `Command::Unsend`) | состояние | imap lib.rs:442, 1244–1254; ews sync.rs:316, 1457–1487 — по карточке переезжает в базу ядра, `holding()`/`leave()` плагинов уходят |

IMAP держит `Index` без использования (`_index`, imap lib.rs:482) — при выносе плагину не нужен.

### 3. Чистые функции ядра — «переезжает в плагин» (без базы и ключей, компилируются в процесс плагина)

| Функция | Кратко | Обращений | Откуда |
|---|---|---|---|
| `send_later::release_now` / `max_delay` | срок «позже», предел задержки | 2 / 3 | imap lib.rs:1247; ews sync.rs:1460; jmap sync.rs:2042, 2043, 2187 |
| `invite::reply_draft` / `delivery` / `import_event` | ответы на приглашения | 3 / 3 / 1 | imap lib.rs:1732, 1750; ews sync.rs:1822, 1840; jmap sync.rs:2206, 2223; jmap invite.rs:30 |
| `lifecycle::newsletter_sweep` (+`ARCHIVE`) | чистка рассылок | 1 | jmap sync.rs:1692 |
| `rules::parse` / `rules::generate` | скрипт Sieve (правила, отпуск) | 1 / 2 | jmap settings.rs:150, 172, 323 |
| `tasks::new_uid` | UID задач | 2 | dav lib.rs:130; jmap settings.rs:258 |
| `store::parts` | раскладка частей письма (с выводом воркера) | 2 | imap lib.rs:914; ews sync.rs:1106 |
| `migrate::RETRY` | константа паузы (движок переноса — в ядре, плагин получает `Import`/`Copy`/`Destroy`) | 3 | imap lib.rs:331; ews sync.rs:258; jmap sync.rs:1355 |
| `contacts::directory` | сводный справочник | 3 | jmap sync.rs:1299, 1828, 1852 |

Итого 23 обращения к 11 функциям. Ещё чистые помощники и типы, базой не касаются: ключи кэша (`scoped`,
`unscoped`, `in_profile`, `account_of`, `profile_of`), цикл синхронизации (`sync::admit`, `report`, `supervise`,
`link::*`), `settings::effective`, `autoreply`, `categories`, `labels`, `schedule`, `query`, типы `model::*`.
`smime::split_for_encryption` (jmap sync.rs:960) — чистая, идёт с `seal_outgoing`.

### 4. Итог

- «Услуга хранилища»: **35 методов / 142 обращения** (производственный код — 31 / 121).
- «Остаётся в ядре»: `tasks::handle` (7 вызовов + 2 реализации `TaskStore`), S/MIME `seal_outgoing` (4) и
  `sign_outgoing` (1), воркер разбора писем (7 вызовов у IMAP и EWS), индекс поиска `indexer::rows` (1), окно
  «Отменить» и очередь отправки (сегодня в памяти IMAP/EWS).
- «Переезжает в плагин»: протокольный код целиком и чистые функции (23 обращения к 11) с типами `model`.
- Сверка с карточкой: перечисленные в ней услуги найдены полностью и только они касаются базы или ключей —
  хранилище (п. 1), индекс поиска, воркер, S/MIME, окно «Отменить» (п. 2); модули `tasks`, `invite`, `send_later`,
  `rules`, `lifecycle`, `migrate` покрыты п. 2–3: движки и то, что поверх кэша, — в ядре, чистые части — в плагин.
