# PLG-06: LDAP — отдельным процессом

lane: x/plg-ldap            # crates/backend-ldap, core (договор каталога), app-gpui
status: done
deps: [PLG-05]
class: A
files: [crates/backend-ldap/*, crates/plugins/*, crates/core/src/smime/directory.rs, crates/core/src/backend/*, app-gpui/Cargo.toml, app-gpui/src/plugins.rs, app-gpui/src/directory.rs, app-gpui/src/settings_directory.rs, app-gpui/src/sys/*, crates/core/tests/layers.rs, Cargo.toml, Cargo.lock]
read: [docs/adr/0013-plugins-on-demand.md, docs/decisions/2026-11-14-plugins-on-demand.md, docs/decisions/2026-11-02-ldap-active-directory.md]
checks: [nextest]
manual: yes                 # docs/manual-tests/PLG-06.md: каталог организации, вход учётной записью Windows

## Задача
- Договор каталога (`smime::directory`) — запрос и ответ по обмену; `backend-ldap` — в наборе плагинов.
- Процесс стартует по первому запросу к каталогу и завершается после 5 минут простоя.
- Профиль песочницы — с корпоративной проверкой подлинности (Kerberos, вход учётной записью Windows); на macOS и
  Linux — что нужно открыть для Kerberos, по итогам PLG-01.
- `domain_controller` — функция ОС в `app-gpui/src/sys`, не в плагине.

## Готово
«Найти в каталоге», поиск при отправке и «Проверить» работают через процесс набора `postman-plugins --plugin=ldap`
(общий файл — почта и каталог вместе, решение владельца 2026-11-16), процесс стартует по первому запросу и
завершается после 5 минут простоя; ручная проверка — `docs/manual-tests/PLG-06.md`, владелец. `ldap3` нет в дереве
зависимостей `postman-gpui` (`layers::interface_does_not_link_plugins`: `postman-backend-ldap` убран из
`LINKED_FOR_NOW`). Тесты зелёные: `nextest --workspace` — **1580/1580** (school-99 Linux, полная песочница,
`POSTMAN_REQUIRE_FULL_SANDBOX=1`; было 1574 — +6), clippy `-D warnings` и `fmt --check` чисты; Windows
локально — `postman-backend-ldap` (28 тестов, в т. ч. обмен) зелёные, сборка `postman-gpui` чистая
(`domain.rs` с `DsGetDcNameW` компилируется), полный прогон тестов локально не влез по диску C (см. «Не сделано»).

## Сделано
- **Договор каталога по обмену** (`crates/core/src/backend/wire.rs`, версия `WIRE` 3): `Ask::Certificates(DirectoryQuery)`
  → `Reply::Certs(Result<Vec<Vec<u8>>, DirectoryError>)`; `DirectoryQuery` — настройки «Настройки → Безопасность»
  (сервер, база, StartTLS, вход) и адреса; пароль простого входа — байтами (`channel::Secret`: `Debug` без значения,
  нулится), как секрет учётки в `Ask::Open`. `DirectoryError` сериализуем (serde у ошибки каталога в
  `smime::directory`). Тесты «туда и обратно» дополнены (`wire::tests::every_variant_round_trips`).
- **Сторона плагина**: `remote::serve_directory` (`crates/core/src/backend/remote/serve.rs`) — вопросы `Hello`/память
  без рантайма, поиск — своим потоком (не задерживает ответы сторожу), настройки — с каждым запросом, фабрика
  каталога из запроса; `Leave` — завершение (каталог ничего не держит). У `backend-ldap` — `ID` (`"ldap"`) и точка
  входа `serve` (`PluginLink`) с отображением `DirectoryQuery` → `Settings`/`Login`.
- **Заместитель каталога** (`RemoteDirectory`, `crates/core/src/backend/remote.rs`): тот же `Directory` (блокирующий
  вызов, как прежде), внутри — процесс через хост плагинов: подъём по первому запросу (`Host::directory`), ожидание
  готовности, «проблема» — одна повторная попытка, тайм-аут ответа — 6 с (поиск в плагине ограничен 5 с).
- **Простой — 5 минут** ([`IDLE_EXIT`], ADR 0013 «по запросу»): сторож процесса при опросе памяти видит `idle`
  и время последнего запроса ([`Plugin::last_use`]), завершает процесс; поток плагина отличает «завершён по простою»
  (`Ended::Idle`, фаза `Phase::Idle`) от падения — перезапуска нет, следующий запрос поднимает процесс заново.
  У каналов поведение прежнее (`idle` не задан).
- **`Claim::None`** (ядро): у каталога своего адреса нет — поиск по настройкам, не по адресу учётки; тест строк
  (`rows_claim_addresses_without_a_process`) дополнен.
- **Общий набор**: `SetEntry` каталога в `crates/plugins` (`Kind::Directory`, профиль `Network { enterprise_auth:
  true }` — право корпоративной проверки подлинности для входа учётной записью Windows, SSPI; `Claim::None`;
  `serve: postman_backend_ldap::serve`). Упаковка не тронута — `postman-plugins` уже в пакетах (PLG-03), состав
  общего файла — решение владельца 2026-11-16 (почта и каталог вместе).
- **Строка корня**: `LDAP` в `app-gpui/src/plugins.rs` (`set: "postman-plugins"`, совпадение с набором — тест
  `rows_match_the_plugin_sets`), `remote_directory(query)` — заместитель вместо прямого плагина;
  `postman-backend-ldap` из зависимостей `app-gpui` убран (Cargo.toml, Cargo.lock).
- **Контроллер домена — функция ОС в интерфейсе** (решение №12, карточка): `app-gpui/src/sys/domain` —
  Windows `DsGetDcNameW` (объявления локальные: своих `windows`/`windows-sys` у интерфейса нет), macOS — DNS SRV
  mDNSResponder + область билета GSS.framework (MAC-05), Linux — `res_query` + MIT Kerberos (LIN-05), разбор SRV
  (`domain/srv.rs`, тесты перенесены); блокировка — не дольше 5 с (`bounded`, PAR-AUDIT №35). `from_prefs`
  (`app-gpui/src/directory.rs`) резолвит контроллер и кладёт сервер в запрос; нет ни сервера, ни домена — «каталог
  не настроен» без процесса (`NotConfigured`). Плагин контроллер не ищет вовсе: песочнице «сеть без файлов» файлы
  (`resolv.conf`, кэш билетов) не нужны — сервер приносит запрос; из `backend-ldap` ушли `sys.rs` (Windows),
  `sys/linux.rs`, `srv.rs`, `domain_controller`; `sys/macos.rs` оставил только мост GSS для входа (`kerberos.rs`).
  Подсказка настроек зовёт `crate::sys::domain_controller`.
- **Тесты**: через обмен — `crates/backend-ldap/tests/remote.rs` (настоящая точка входа `serve`, кадры `postman-proc`,
  подставной сервер `fake` на 127.0.0.1: поиск сертификатов туда и обратно, «не настроен» без сервера, простой вход
  без TLS не начинается); настоящий процесс набора — `crates/plugins/tests/ldap.rs` (`Hello` + отказ «не настроен»
  по обмену в песочнице, работает и на Windows; строка набора; полный поиск с `fake` — кроме Windows: loopback
  AppContainer закрыт, см. карточку PLG-05 п. 8 — проверен на Linux). Подменный сервер LDAP вынесен из unit-тестов
  в `backend-ldap::fake` (признак `testkit`, как `fake` у Telegram) — им пользуются тесты крейта, обмен и процесс.
- **Слои**: `layers.rs` — `postman-backend-ldap` вне `LINKED_FOR_NOW` (`interface_does_not_link_plugins` проходит);
  набор и плагин — в своих таблицах как были. Замер — строка №17 в `docs/SIZE.md`.
- Вне `files:` (без них не собирается и не проверяется): `crates/core/src/backend/wire.rs` уже в списке через
  `crates/core/src/backend/*`; `docs/manual-tests/PLG-06.md`, `docs/tasks/PLG-06.md`; `Cargo.lock` — deps LDAP
  (self `testkit`, `postman-proc` в dev, `windows-sys` ушёл из `backend-ldap`).

## Не сделано
- Ручная проверка на настоящем каталоге (контроллер домена, вход учётной записью Windows, поиск при отправке,
  «Проверить», простой 5 минут) — `docs/manual-tests/PLG-06.md`, владелец: нужен домен с Active Directory и
  тестовые учётные записи.
- Полный поиск настоящим процессом с подставным сервером в автотестах — только вне Windows (loopback у
  песочницы AppContainer закрыт): на Windows обмен проверяют `backend-ldap/tests/remote.rs` (без песочницы, в
  потоке теста) и процесс с отказом «не настроен» (`plugins/tests/ldap.rs`); настоящий доменный поиск на
  Windows — ручная проверка.
- Вход учётной записью на macOS и Linux в песочнице: профиль «сеть без файлов» не открывает кэш билетов Kerberos
  и `/etc/krb5.conf` (PLG-01: «открывать — отдельным правилом после проверки на стенде») — простоящее поведение:
  вход простым паролем по TLS и анонимный работают, Kerberos на macOS/Linux — после PLG-01/PLG-12. Контроллер
  домена на macOS/Linux резолвит интерфейс (вне песочницы) — без регрессии к PLG-06.
- Замер памяти живого процесса каталога (`plugins_rss_mb`) — с ручной проверкой; в тестах рабочий набор
  запрашивается сторожем как у всех плагинов.
- Локальный полный прогон `cargo test` на Windows — диск C исчерпан (0 свободно), `target` локально не держим:
  школьный прогон Linux полный и зелёный; Windows покрывают сборка `postman-gpui`, 28 тестов `postman-backend-ldap`
  (до исчерпания диска) и sandbox-тесты из `postman-core`/`postman-proc` — они же в школьном прогоне
  (`POSTMAN_REQUIRE_FULL_SANDBOX=1`) — а на Windows процесс-тесты `plugins/tests/ldap.rs` и
  `backend-ldap/tests/remote.rs` зелёные до выноса локального прогона.
- Windows-урок PLG-05 (отложенная загрузка) не понадобился: дерево зависимостей набора с LDAP не тянет
  `user32`/`win32k`-библиотеки статически (на Windows процесс-тест зелёный; `windows-sys` из `backend-ldap` ушёл
  вместе с `DsGetDcNameW` в интерфейс). Если ручная проверка найдёт 0xC0000142 на стенде — `/DELAYLOAD` в
  `build.rs` набора, как у Telegram.
