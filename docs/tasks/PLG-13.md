# PLG-13: каналы в MSI — по умолчанию всё, состав раскатки — CHANNELS

lane: x/plg-pack           # tools/pack, scripts/windows — полоса ci
status: done
deps: [PLG-03]
class: B                   # состав пакета и свойства установщика
files: [tools/pack/src/msi.rs, scripts/windows/msi-check.ps1, docs/DEV.md, docs/decisions/2026-11-19-components.md, docs/adr/0013-plugins-on-demand.md, docs/manual-tests/PLG-13.md]
read: [docs/adr/0013-plugins-on-demand.md, docs/decisions/2026-11-07-installer-msi-winget.md, docs/decisions/2026-11-16-plugin-files.md, docs/decisions/2026-11-19-components.md]
checks: [nextest, release]
manual: yes                # docs/manual-tests/PLG-13.md: CHANNELS, обновление с сохранением состава, REMOVE

## Задача
- MSI (решение владельца 2026-11-19, `docs/decisions/2026-11-19-components.md`): Telegram и Matrix ставятся по
  умолчанию вместе со всем — обычному пользователю выбирать ничего не надо; неиспользуемый канал не запускается
  (ADR 0013, плагины по запросу).
- Состав каналов для сетевой раскатки — свойство `CHANNELS` (тихая установка, без мастера): точный список
  (`CHANNELS=Telegram`, `CHANNELS=Telegram,Matrix`), `none`/пустое — без каналов, не задано — все.
- Выбор переживает большие обновления в обе стороны (маркеры в реестре); самообновление (`msiexec /passive`
  без свойств) состав не теряет.
- `REMOVE=имя` снимает канал как обычно; после такого снятия обновление канал не возвращает.
- Без мастера UI: Windows Installer показывает базовый ход установки, как сейчас.

## Сделано
- `tools/pack/src/msi.rs`: фичи `Main`, `Telegram`, `Matrix` (уровень 1 у всех — по умолчанию ставится всё);
  у каждого канала свой компонент файла и компонент-маркер (ключевой путь — запись реестра
  `HKLM\SOFTWARE\Pets\Postman\Channels\<Канал>`, снятие канала убирает значение); состав ведут действия Type 51
  до `CostFinalize` в обеих последовательностях: `No<Канал>` (CHANNELS задан и имени в нём нет — имя фичи в
  REMOVE) и `KeepNo<Канал>` (обновление без CHANNELS, маркера нет — имя фичи в REMOVE); `AppSearch`+`RegLocator`
  читают маркеры в `PREVTELEGRAM`/`PREVMATRIX` (после `FindRelatedProducts`, до расчёта стоимости);
  `CHANNELS` и `PREV*` — в `SecureCustomProperties`; условия уважают явный `REMOVE`; при полном удалении ключ
  маркеров снимается целиком (`ChannelsKey`, имя «-»).
- `scripts/windows/msi-check.ps1`: установка по умолчанию ставит оба канала; `CHANNELS=none` снимает; обновление
  держит состав в обе стороны; `CHANNELS=Telegram` возвращает только его; `REMOVE=Telegram` снимает и переживает
  обновление; при удалении уходят ключ маркеров и оба профиля AppContainer, как раньше.
- Тесты пакета: фичи уровня 1, привязка компонентов и маркеров, `AppSearch`/действия/условия до `CostFinalize`,
  `SecureCustomProperties`; административная установка по-прежнему раскладывает весь комплект (8/8, msiexec /a).
- Документы: решение владельца `docs/decisions/2026-11-19-components.md`; ADR 0013 п. 7 (докачка из приложения —
  волны PLG-14…PLG-17); DEV.md «Выпуск» — свойства тихой установки; ручная проверка — `docs/manual-tests/PLG-13.md`.

## Не сделано (адресно)
- Прогон `msi-check.ps1` — job `release` (на рабочей машине скрипт меняет систему — не гонять); время job —
  дирижёр по первому прогону PR после `gh pr ready`.
- Ручные проверки CHANNELS/обновления — владелец по `docs/manual-tests/PLG-13.md` (нужны артефакты выпуска).
- Докачка файлов каналов из приложения и пакеты без каналов — PLG-14…PLG-17.
