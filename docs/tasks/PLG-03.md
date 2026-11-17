# PLG-03: набор плагинов в пакетах трёх ОС

lane: x/plg-pack            # tools/pack, scripts, .github (job release) — полоса ci
status: done
deps: [PLG-02]
class: B                    # состав пакета и выпуск
files: [tools/pack/*, scripts/windows/*, scripts/macos/*, scripts/linux/*, .github/workflows/*, docs/DEV.md, docs/SIZE.md, docs/releases/*]
read: [docs/adr/0013-plugins-on-demand.md, docs/decisions/2026-11-14-plugins-on-demand.md, docs/decisions/2026-11-07-installer-msi-winget.md, docs/decisions/2026-11-11-self-update.md]
checks: [nextest, release]
manual: yes                 # docs/manual-tests/PLG-03.md: установка, обновление, удаление на трёх ОС

## Задача
- Файлы наборов плагинов — по списку, а не по имени: архив и MSI Windows, `.app` macOS (`Contents/MacOS`, подпись
  вместе с воркером), deb/rpm и архив Linux. Сколько наборов — по ответу владельца на вопрос №1 ADR 0013
  (рекомендация — общий набор и отдельный файл Matrix компонентом установщика).
- Подпись (Authenticode, подпись Apple), minisign пакета обновления — файлы наборов в том же пакете, одна версия.
- Самообновление: набор заменяется вместе с `postman.exe`; прерванное обновление — версии не совпали, плагин не
  используется (сообщение «нужен перезапуск»), данные целы.
- Удаление: снимается и профиль AppContainer сетевой песочницы набора.
- job `release`: сборка набора; время job — записать; если `gate` выходит за 7 минут — набор собирается только в
  `release`.
- Строка бюджета файла набора в `docs/SIZE.md` — по факту первой сборки, решением владельца.

## Готово
Пакеты трёх ОС содержат набор; установка, обновление с прошлой версии и удаление проверены вручную; `msi-check` и
проверки пакетов в CI зелёные.

## Решение владельца
2026-11-16, `docs/decisions/2026-11-16-plugin-files.md` (ответ вписан и в ADR 0013 «Вопросы владельцу» №1): общий файл
набора — вся почта и каталог (JMAP, IMAP + SMTP, EWS, DAV, LDAP) вместе; Telegram и Matrix — отдельными файлами набора;
Matrix в выпуск 0.2.0 не входит. Сейчас в наборе только демо-канал (PLG-02) — пакуется то, что есть; состав расширяют
PLG-04…10 (перенос плагина между файлами — правка упаковки, код и договор не меняются).

## Сделано
- Windows: `package.ps1` кладёт `postman-plugins.exe` в zip рядом с воркером; MSI — строка в `ITEMS`
  (`tools/pack/src/msi.rs`, короткое имя `POSTMA~3.EXE`), тести пакета обновлены (файлов в пакете 5, административная
  установка раскладывает набор); `install.ps1` копирует и снимает файл набора, `Assert-NotRunning` видит процесс
  `postman-plugins`; `msi-check.ps1` проверяет файл набора в установке и снятие обоих профилей AppContainer
  (`Postman.MailWorker`, `Postman.Plugins`) при удалении — снятие делает `postman-worker --remove-appcontainer`
  (`sandbox::remove_profile` снимает оба профиля, PLG-02); подпись Authenticode — `postman-plugins.exe` добавлен в шаг
  «Подпись exe» job `release` (`.github/workflows/ci.yml`), MSI/zip с набором подписывает minisign `publish` без правок.
- macOS: `scripts/macos/package.sh` — набор в `Contents/MacOS`, подпись `codesign` вместе с воркером и просмотрщиком
  (ad-hoc или Developer ID тем же вызовом); README перечисляет набор.
- Linux: `scripts/linux/package.sh` — `postman-plugins` в `bins` (deb `/usr/bin`, rpm `%files`, AppImage `usr/bin`);
  проверки glibc-символов и связанных библиотек охватывают набор автоматически (цикл по `bins`); профиль AppArmor
  `postman-plugins` (`userns`) в `data/apparmor/postman` — deb и rpm ставят его вместе с профилями воркера.
- Самообновление: код (`pets-update`, T3-12; `app-gpui/src/update.rs`) правок не потребовал — portable заменяет файлы
  по содержимому архива (набор в архиве — заменяется), macOS заменяет бандл `Postman.app` целиком, MSI и deb/rpm —
  своими механизмами, AppImage — один файл; `keep` набор не трогает. Прерванное обновление ловит сверка версий в
  `Hello` (PLG-02): плагин не используется, данные целы.
- CI: job `release` собирает набор тем же вызовом `cargo build --release --workspace` (набор — член workspace с PLG-02),
  отдельных шагов сборки PLG-03 не добавляет; `gate`/`test` тоже (тесты `crates/plugins` в `--workspace` с PLG-02).
  Время — в «Замерах»; если после мержа `gate` выйдет за 7 минут — сужать сборку надо по карточкам PLG-04+ (тесты
  набора из `--workspace` не убрать, но время собираемости видно в замерах).
- `docs/SIZE.md`: строка бюджета №14 и раздел «Файл набора плагинов (PLG-03, 2026-11-16)» — факт первой release-сборки
  **1,15 МиБ** (1 211 392 байт, MSVC, `cargo build --release -p postman-plugins`): в наборе только демо-канал, сетевого
  стека в файле нет; оценка на полный общий файл ~15 МиБ (ADR 0013), бюджет — по факту наполнения PLG-04…10; строки
  №11 и №12 (Matrix) закрыты решением 2026-11-16.
- Документы: DEV.md «Выпуск» (четыре бинаря в пакетах Linux, AppArmor с набором) и «Запуск» (набор в пакетах трёх ОС);
  README архивов Windows и macOS перечисляют набор; ручные проверки — `docs/manual-tests/PLG-03.md`.

## Замеры
- Размер набора: **1,15 МиБ** (1 211 392 байт) — Windows, MSVC, release, ветка `x/plg-pack`.
- Linux (school-99, контейнер postbuild, `-j6`, `CARGO_TARGET_DIR=/build/target-plg03`): `cargo nextest run
  --workspace --locked --profile ci` — **17 мин** (холодный кэш; повторные прогоны быстрее); release-сборка workspace
  для `package.sh` (`cargo build --release --workspace --locked --features postman-worker/viewer`) — **26 мин**;
  deb/rpm/AppImage по ней — минуты. Windows: `cargo check -p postman-gpui` — зелёный; `cargo nextest -p postman-pack` —
  8/8, включая административную установку MSI.
- CI: job `release` на трёх платформах; точное время job — из первого прогона PR (на черновике CI выключен, решение
  2026-11-12), записывает дирижёр после `gh pr ready`.

## Не сделано (адресно)
- Ручные проверки установки/обновления/удаления на трёх ОС — владелец, шаги в `docs/manual-tests/PLG-03.md`
  (нужны артефакты выпуска: MSI/zip, `.dmg`, deb/rpm/AppImage).
- Время job `release` и `gate` из CI — заполнит дирижёр по первому прогону PR (ветка стеком на PLG-02, PR не открывался).
- Скачивание набора отдельным файлом выпуска («Нет компонента», S8-38) не добавлено: сегодня отсутствие набора в
  выпуске ничего не ломает (демо-канал в release идёт внутри `postman.exe`); адресно — вместе с PLG-04/PLG-05, когда в
  наборе появятся настоящие протоколы и файл станет обязателен.
- Отдельные файлы наборов Telegram и Matrix появятся в их волнах (PLG-04, PLG-05): упаковка готова к добавлению строк
  (по списку, не по имени), код и договор не меняются.
