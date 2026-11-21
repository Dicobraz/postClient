# AUD-15: флейк — `wrong_password_is_one_attempt_per_login` падает под голым `cargo test`

lane: x/aud-flake
status: todo
deps: []
class: A                   # флейк теста: глобальное NTLM-состояние процесса между параллельными тестами libtest
files: [crates/backend-ews/tests/mock.rs, crates/ews/src/http.rs]
read: [docs/tasks/T1-02а.md]
checks: [nextest, cargo test -p postman-backend-ews]
manual: no

## Задача
- `backend-ews::wrong_password_is_one_attempt_per_login` падает под голым `cargo test` и зелёный под nextest
  (каждый тест — свой процесс). Воспроизводится на чистом `main` 2026-11-21, подтверждено stash-ом.
- Причина: процессное глобальное состояние NTLM в `crates/ews/src/http.rs` — `static ACCEPTED:
  Mutex<Vec<(String, String, bool)>>` («введённый логин → принятый, была ли привязка к каналу»). Под libtest тесты
  одного файла идут параллельными потоками одного процесса: соседний тест (`email_login_falls_back_to_domain_user_once`,
  тот же логин `anna@corp.example`) успевает запомнить `anna@corp.example → EXAMPLE\anna`, и у теста с неверным
  паролем `remembered()` подставляет чужой ответ — в журнале мока не те две строки `NTLM REJECTED …`, ассерт падает.
- Чинить глобальное состояние/изоляцию теста: состояние «запомненного входа» — в клиенте/диагностике соединения
  (как `Diag.tries`), а не в static; либо явный сброс состояния в тестах. Правило — тот же тест зелёный и под
  `cargo test -p postman-backend-ews`, и под nextest.

## Сделано
- —

## Не сделано
- Вся задача.
