# PLG-08: услуги ядра почтовому пути, EWS — отдельным процессом

lane: x/plg-ews             # core (договор, очередь отправки), backend-ews, app-gpui
status: done
deps: [PLG-07]
class: B                    # услуга хранилища учётки, окно «Отменить» в ядре
files: [crates/core/src/backend/*, crates/core/src/sync.rs, crates/core/src/send_later.rs, crates/core/src/tasks/*, crates/core/src/invite.rs, crates/core/src/lifecycle.rs, crates/core/src/rules.rs, crates/core/src/migrate.rs, crates/core/src/smime/*, crates/cache/src/*, crates/backend-ews/*, crates/plugins/*, app-gpui/Cargo.toml, app-gpui/src/plugins.rs, app-gpui/src/signin.rs, crates/core/tests/layers.rs, Cargo.toml, Cargo.lock]
read: [docs/adr/0013-plugins-on-demand.md, docs/decisions/2026-11-14-plugins-on-demand.md, docs/adr/0006-single-store.md, docs/decisions/2026-11-14-all-in-db.md]
checks: [nextest, bench]
manual: yes                 # docs/manual-tests/PLG-08.md: Exchange — почта, календарь, вход учётной записью Windows

## Задача
- Первым шагом — опись: какие методы кэша и функции ядра достижимы из почтовых путей (сейчас 143 обращения к
  35 методам, плюс `tasks`, `invite`, `send_later`, `rules`, `lifecycle`, `migrate`). Опись — в карточку.
- Услуга хранилища учётки: эти методы — вызовом по обмену; база открыта только главным процессом. Записи без ответа
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
EWS — процессом общего набора (`postman-plugins --plugin=ews`): почта, папки, отправка с «Отменить» у ядра,
календарь, занятость, справочник — почта и свои области одним процессом; `postman-backend-ews` нет в дереве
`postman-gpui` (`layers::interface_does_not_link_plugins`, `LINKED_FOR_NOW` без ews). Услуги ядра по обмену:
хранилище учётки ([`MailCache`] — 25 операций из описи, записи без ответа подряд), воркер разбора (4 операции,
плагин процесс не запускает), S/MIME `seal_outgoing`/`sign_outgoing` (ключи у ядра); чистые функции описи
(`send_later`, `invite`, `lifecycle`, `rules`, `migrate::RETRY`, `store::parts`, `contacts::directory`) — в
плагине как были (вызовы ядра без базы и ключей линкуются в процесс). Окно «Отменить» и очередь отправки — в ядре
(`MailQueue`, база профиля): путь получает письмо в момент отправки и подтверждает итог; падение процесса (и самой
программы) письмо не теряет — тест; `holding()`/`leave()` пути EWS ушли, выход дожидается очереди ядра. Отказы
входа — родами `ErrorKind` (`WindowsSignInUnavailable`, `CredentialsRejected`, `is_auth()`), `is_ews_url` —
строкой описания плагина ([`Claim::Path`], [`Row::claims`]). Тесты: CI трёх ОС — см. PR; локально `postman-core`
410, `postman-plugins` (настоящий процесс в песочнице — отказ входа по обмену, строка набора), `backend-ews` (mock —
роды отказов). Ручная проверка — `docs/manual-tests/PLG-08.md`, владельцу.

## Сделано
- **Договор обмена №5** (`backend::wire`): `Ask::MailOpen`/`MailCommand`, события `FromPlugin::MailEvent` (тот же
  `sync::Event`), услуги ядра — вызовы связи `FromPlugin::Cache` (чтения) и `CacheWrite` (записи без ответа),
  `Worker`, `Smime`; ответы `ToPlugin::Cached`/`Worked`/`Sealed`, `Reply::MailOpened`. Тест «туда и обратно» — на
  каждый вариант.
- **Услуга хранилища учётки** (`backend::mail::MailCache`, 25 операций из описи PLG-08): у пути-процесса —
  [`RemoteCache`] (записи — `send` без ответа, чтения — `call` с ответом, ошибка прошлых записей — на ближайшем
  чтении), у ядра — исполнитель `run_cache` на связи (`Conn`), в потоке ядра и в тестах — [`LocalCache`]. База
  открыта только главным процессом (ADR 0006); порядок записей сохраняется (одна связь = один порядок).
- **Услуги воркера и S/MIME** ([`MailWorker`], [`MailSeal`]): воркер-песочницу запускает ядро ([`LocalWorker`] у
  хоста, один на процесс плагина), плагин процессов не запускает; разбор (`mime`, `mime_part`, `mime_split`,
  `threads`) — операциями по обмену. S/MIME (`seal_outgoing`, `sign_outgoing`) — у ядра ([`LocalSeal`], ключи
  пользователя плагину недоступны).
- **Путь-процесс** (`serve_mail_area`, `MailServer`): вход синхронизации учётки ([`Ask::MailOpen`]), команды
  ([`Ask::MailCommand`]), события (`FromPlugin::MailEvent`); области — те же сессии, что у DAV (PLG-07). Задача в
  `spawn_local` — по одной операции, как у областей; процесс создаётся при первом вопросе.
- **Заместитель** ([`RemoteMail`], `Host::mail`): тот же [`MailPlugin`], внутри — процесс; канал команд интерфейсу
  отдаёт сразу (процесс поднимается в фоне, как поток учётки), события — в канал учётки, процесс упал — вход заново
  (`MailOpen`), отказ синхронизации (отказ входа) — конец, как у пути в потоке ядра.
- **EWS в наборе**: `SetEntry { id: "ews", kind: Mail, claim: Path("/ews/exchange.asmx"), profile: Network {
  enterprise_auth: true } }`, точка входа `backend_ews::serve` → `serve_mail_area` — почта ([`EwsServer`]) и свои
  области ([`EwsAreaServer`] — у IMAP-учётки на Exchange, `?ews=…`) одним процессом. Вход учётной записью Windows —
  право корпоративной проверки подлинности (как у каталога LDAP). `EwsPlugin` (в потоке ядра) остался для тестов
  подменного сервера — из корня его убрали вместе с зависимостью `app-gpui`.
- **Окно «Отменить» и очередь отправки — у ядра** ([`MailQueue`] в `backend::mail`, база профиля — таблица `local`):
  `Send { release: After/At }` очередь берёт себе (интерфейсу — «отправлено» с окном сразу), путь получает письмо в
  момент отправки ([`Release::Now`] со своим билетом, [`INTERNAL_TICKET`]) и подтверждает итог (`Sent`/`ComposeFailed`
  — очереди, интерфейсу — её ошибки). Письмо переживает падение процесса (вход заново — дослать) и закрытие
  программы (в базе профиля; следующий запуск — в путь сразу). Выход: `mail_leave()`/`mail_holding()`
  (`finish_sends`), `Unsend`/`Withhold`/`Scheduled` очередь отвечает сама; `holding()`/`leave()` пути EWS удалены.
- **Рода отказов входа — в договоре** (`model::ErrorKind`): `WindowsSignInUnavailable` («войти с паролем») и
  `CredentialsRejected` (подсказка про вид логина), `is_auth()` — поведение строки входа и повторов; `is_ews_url`
  интерфейса — [`Row::claims`] по [`Claim::Path`] (строка `ews` в корне), `signin.rs` плагин пути не линкует.
- **Тесты**: ядро — очередь (`a_letter_from_a_crashed_run_leaves_at_the_next_sign_in`, `unsend_cancels_a_queued_letter`,
  `leave_flushes_the_undo_window_and_waits_for_the_path`, `queue_round_trip`) и договор (новые варианты «туда и
  обратно», воркер байтами postcard); набор — настоящий процесс в песочнице «сеть без файлов»
  (`plugins/tests/ews.rs`: отказ входа по обмену, строка набора по [`Claim::Path`]); пути — подменный сервер EWS
  (`backend-ews/tests/mock.rs` — роды отказов входа); корень — строка `ews` совпадает с мнением набора.

## Не сделано
- Ручная проверка на живом стенде Exchange (почта, папки, календарь, занятость, справочник, вход учётной записью
  Windows, окно «Отменить», `first_sync_ms` демо-ящика до/после — не хуже 10 %) — `docs/manual-tests/PLG-08.md`,
  владельцу: нужны учётки стенда (`POSTMAN_STAND_*`); автотесты стенда без них пропускаются.
- Замер №19 (`docs/SIZE.md`, release j2) — school-99, отдельно после зелёного CI (владелец организует).
- Паника в задаче процесса (`spawn_local` у `serve_mail_area`) завершает задачу молча — защита «паника → ответ
  ошибкой» (`catch_unwind` на границе задачи) — отдельная задача до PLG-12 (как у PLG-07).
- Индекс поиска (`indexer::rows`) услугой по обмену не заведён: EWS его не зовёт (поиск — на сервере), услуга
  понадобится JMAP при его выносе (PLG-10) — договор тогда дополнится без смены смысла (№5 и дальше растёт
  вариантами).
