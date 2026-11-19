# PLG-01: общий механизм процессов и песочница «сеть без файлов»

lane: x/plg-proc            # новый крейт crates/proc, crates/worker, deps, lanes.toml
status: done
deps: []
class: B                    # песочница (CODEOWNERS), новый крейт, lanes.toml
files: [crates/proc/*, crates/worker/src/lib.rs, crates/worker/src/sandbox/*, crates/worker/tests/*, crates/worker/Cargo.toml, crates/core/tests/layers.rs, Cargo.toml, Cargo.lock, lanes.toml, docs/adr/0013-plugins-on-demand.md, docs/00-ARCHITECTURE.md, docs/DEV.md]
read: [docs/adr/0013-plugins-on-demand.md, docs/decisions/2026-11-14-plugins-on-demand.md, docs/adr/0007-format-viewers.md, docs/adr/0009-macos-sandbox.md, docs/adr/0010-linux-sandbox.md, docs/adr/0011-platform-contract.md]
checks: [nextest]
manual: no

> Влиты в main: 1cc52af (PR #705). CI main зелёный (nextest, clippy, macOS, CodeQL, 706433e).

## Задача
- Крейт `postman-proc` (L1, полоса `sandbox`): из `crates/worker` переезжают запуск в песочнице (`sandbox::spawn`,
  `lockdown`), кадры «заголовок + двоичная часть», журнал дочернего процесса (уровни, предел частоты), сторож,
  перезапуск, смерть клиента. `postman-worker` и `postman-viewer` пользуются им; их поведение, пределы и тесты не
  меняются.
- Длинная двусторонняя связь на тех же кадрах: сообщения в обе стороны, вызов с ответом по номеру, `Ping`/`Pong`,
  остановка по `Shutdown`. Кодек сообщений — postcard.
- Профиль песочницы «сеть без файлов» (ADR 0013, таблица): Windows — AppContainer с правами исходящей сети (интернет,
  локальная сеть организации) и, по описанию плагина, корпоративной проверки подлинности; macOS — Seatbelt с
  исходящей сетью, разрешением имён и проверкой сертификатов; Linux — Landlock и seccomp с TCP наружу и чтением
  настроек имён и корневых сертификатов. Профиль задаётся при запуске и не понижается; профили воркера и просмотрщика
  не трогаются. Удаление программы снимает и новый профиль AppContainer (как PAR-05).
- Пониженный приоритет процессора и ввода-вывода на время прогрева и его возврат — функцией крейта (три ОС).
- Проверить на трёх ОС и записать в ADR 0013: системный прокси, корпоративный корневой сертификат, вход учётной
  записью Windows (SSPI), Kerberos. Что не работает в профиле — строкой в ADR с причиной, без слабого режима.
- Замер: создание процесса в песочнице → ответ на приветствие (повторный и первый запуск), три ОС — в `docs/DEV.md`.

## Готово
Тесты песочницы воркера и просмотрщика зелёные без правок ожиданий; новый профиль проверен тестами на трёх ОС (сеть
наружу есть; файлов, дочерних процессов и входящих соединений нет); замер старта записан; слой `postman-proc` — в
`layers.rs`.

## Сделано
- Крейт `postman-proc` (L1, полоса `sandbox`, `layers.rs`, `lanes.toml`): из `crates/worker` переехали `sandbox::spawn` /
  `isolate` / `lockdown`, кадры, журнал stderr, сторож, коды выхода. `postman-worker` и `postman-viewer` пользуются им;
  их тесты зелёные без правок ожиданий (Linux: 168/168).
- Длинная связь `postman_proc::link`: сообщения, вызов с ответом по номеру, `Ping`/`Pong`, `Shutdown`; postcard.
- Профиль `Profile::Network` на трёх ОС; удаление программы снимает оба профиля AppContainer (тот же
  `postman-worker --remove-appcontainer`).
- Приоритет на время прогрева: `Process::background` (Windows — класс приоритета, macOS — фоновый режим процесса,
  Linux — `SCHED_BATCH` и класс ввода-вывода idle у всех потоков).
- Тесты `crates/proc/tests/network.rs`; замер старта Linux — `docs/DEV.md`; таблица проверки — ADR 0013.

## Не сделано
- Замер старта на Windows и macOS (нужен стенд).
- Проверка на стенде: системный прокси, корпоративный корневой сертификат, вход учётной записью Windows (SSPI),
  Kerberos — строки «не проверено» в ADR 0013. Kerberos на macOS и Linux в профиле не работает (причина — там же).
- HTTPS в профиле на Windows и macOS — с первым плагином через процесс (PLG-02).
