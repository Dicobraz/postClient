# AUD-08: политика «только TLS 1.3» — почтовый путь EWS и IMAP/SMTP в процессах плагинов

lane: x/aud-tls         # crates/ews, crates/imap, crates/backend-ews, crates/backend-imap
status: done
deps: []
class: A                   # дыра политики безопасности к бете 0.3.0-beta.1 (аудит main)
files: [crates/backend-ews/src/lib.rs, crates/backend-ews/src/sync.rs, crates/backend-imap/src/lib.rs, crates/ews/src/http.rs, crates/imap/src/lib.rs, crates/imap/src/smtp.rs]
read: [docs/tasks/POL-01.md, docs/tasks/PLG-10.md]
checks: [nextest]
manual: yes                # docs/manual-tests/AUD-08.md: при политике MinTls=1.3 почта Exchange и IMAP/SMTP ходят по 1.3

## Задача
- Аудит к бете 0.3.0-beta.1: политика администратора `MinTls`/«только TLS 1.3» (POL-01, S5-30) доходит до процессов
  плагинов (`Host::set_settings` → `Settings.tls13_only`), но применяют её только JMAP (PLG-10), DAV (PLG-07) и
  области EWS (`EwsAreaServer::connect`). Не применяют: почтовый путь EWS в процессе (внутрипроцессный
  `EwsPlugin::set_tls13_only` реестра после PLG-08 не участвует) и IMAP/SMTP — переключателя там нет вовсе:
  учётка Exchange или «стандарты» с политикой «только TLS 1.3» ходила на сервер по TLS 1.2.

## Сделано
- Почтовый путь EWS (PLG-08): процесс применяет `Settings.tls13_only` на входе учётки (`EwsServer::open` — в потоке
  синхронизации, и `sync::run` — перед входом), как у пути JMAP и областей EWS; клиент почты, подписки на события
  и области учётки (`postman_ews`) читают политику при каждом новом соединении (минимум версии — в конструкторе
  SChannel, `postman_ews::http`).
- IMAP/SMTP (PLG-09): то же в `ImapServer::open` и `run` процесса; в `postman_imap` заведён переключатель
  (`set_tls13_only`) — версии rustls-коннектора: политика — только TLS 1.3, иначе 1.2 и 1.3. Отправка SMTP теперь
  тем же коннектором (`SmtpClientBuilder::new_with_tls_connector`), проверка сертификата Windows и стенд
  (`POSTMAN_INSECURE_TLS`) — как были.
- Тесты (пламбинг, без рукопожатия): у `postman_imap` политика переключает версии коннектора (только 1.3 ↔ 1.2+1.3)
  и коннектор собирается в обоих случаях; у `postman_ews::http` политика задаёт минимальную версию конструктора
  соединения (1.3 ↔ 1.2).

## Не сделано (адресно)
- Ограничение PLG-10 — относится и к EWS, и к IMAP: смена политики доезжает до живого процесса настройками хоста,
  но применяется на его следующий вход сессии; до перезапуска процесса старые подключения живут по старой политике.
- Ручная проверка на стенде с политикой MinTls=1.3 (почта Exchange, IMAP/SMTP) — владельцу.
