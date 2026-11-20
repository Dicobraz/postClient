# PLG-11: уборка — `postman.exe` без протоколов

lane: x/plg-cleanup         # app-gpui, core (тест слоёв), docs
status: done
deps: [PLG-10]
class: B                    # бюджеты размера, ADR и архитектура
files: [app-gpui/Cargo.toml, app-gpui/src/plugins.rs, crates/core/src/backend/*, crates/core/tests/layers.rs, Cargo.toml, Cargo.lock, docs/00-ARCHITECTURE.md, docs/adr/0004-protocol-plugins.md, docs/adr/0012-channels.md, docs/adr/0013-plugins-on-demand.md, docs/SIZE.md, docs/DEV.md, docs/tasks/SIZE-01.md]
read: [docs/adr/0013-plugins-on-demand.md, docs/decisions/2026-11-14-plugins-on-demand.md]
checks: [nextest, bench, release]
manual: no

## Задача
- `postman-gpui` не зависит ни от одного `postman-backend-*`: список исключений `interface_does_not_link_plugins`
  пуст; путь «плагин внутри процесса» остаётся только в тестах плагинов.
- Признаки сборки каналов и остатки прямых вызовов плагинов из интерфейса — убрать.
- Документы: `docs/00-ARCHITECTURE.md` (слои, процессы), ADR 0004 и 0012 — ссылка на 0013, ADR 0013 — статус
  и фактические цифры, `docs/DEV.md` — как собирать и запускать с набором.
- Размер: release-замер всех файлов, разбивка `postman.exe`; новые бюджеты — строкой в `docs/SIZE.md`, решением
  владельца (правило малого шага). SIZE-01: `panic = "abort"` для процессов плагинов — в ту же задачу.

## Готово
`postman-gpui` не зависит ни от одного `postman-backend-*` (PLG-08…10 убрали по волне: EWS, IMAP+SMTP, JMAP;
список исключений `LINKED_FOR_NOW` пуст, тест слоёв зелёный); `reqwest` и `rustls` вне дерева `postman-gpui`
(`cargo tree`, PLG-10). Путь «плагин внутри процесса» остался только в тестах плагинов (стенды и
`plugins/tests/*`: локальные услуги `LocalCache`/`LocalWorker`/`LocalSeal` поверх того же договора) и в
dev-сборке у демо-канала (`demo_remote`). Признаков сборки каналов больше нет: `matrix-release` удалён ещё
в PLG-04 (оптимизации крейтов Matrix из №12 — блоком `[profile.release.package]` на файл набора
`postman-plugins-matrix`, комментарий поправлен), прямых вызовов плагинов из интерфейса нет (реестр — строки-заместители).
Документы: `00-ARCHITECTURE.md` — таблица слоёв (L2b целиком, строки наборов процессов, «в `postman.exe` нет
сетевого стека протоколов»); ADR 0013 — статус «принято и выполнено», раздел «Как стало (0.2.0)» с фактами,
откат после PLG-11; ADR 0004 и 0012 — ссылка на 0013; `DEV.md` — сборка и запуск с наборами. Замер всех файлов
(2026-11-20, Linux x86_64, release): `postman` 72,89 (76 440 128), воркер 12,73, просмотрщик 1,97, общий набор
17,92 (18 788 168), Telegram 4,27, Matrix 23,24 — строкой в `docs/SIZE.md` с **предложением бюджета**
(`postman.exe` ≤ 74 не задет; общий набор — **≤ 19 МиБ** против оценки ~15 из №14; Telegram ≤ 5, Matrix ≤ 24,
просмотрщик ≤ 2; воркер ≤ 12 — Windows-замер решения №9 без перемены) — **решает владелец**.

## Не сделано
- `panic = "abort"` для процессов плагинов (SIZE-01): cargo запрещает `panic` в `[profile.*.package]`
  («may not be specified in a package profile» — проверено), а правка release-job в `.github` (собрать наборы
  профилем `[profile.dist] inherits = "release"` с `panic = "abort"`, упаковке — `--target-dir`) — за владельцем.
  Рецепт — в предложении бюджета `docs/SIZE.md`; до этого наборы собираются общим профилем `release` (unwind,
  решение №1 для `postman.exe` — не задето).
- Разбивка `postman.exe` по крейтам (`cargo bloat`) — не снята: состав exe по дереву зависимостей (№21:
  интерфейс + ядро + хранилище + gpui/шрифты; протоколов и HTTP-стека нет). При желании — отдельной задачей
  (cargo-bloat в контейнер school-99).
- Новые бюджеты — предложение в `docs/SIZE.md`, решение за владельцем (правило малого шага).
