# AUD-16: файл THIRD-PARTY — атрибуция значков Fluent (ADR 0008, P2-legal)

lane: x/aud-16
status: done
deps: []
class: A
files: [THIRD-PARTY, app-gpui/res/icons/README.md]
read: [docs/adr/0008-fluent-system-icons.md, docs/tasks/T1-26.md]
checks: []
manual: no

## Задача
- Найдено аудитом 2026-11-21 (проверка ADR 0008): файл `THIRD-PARTY` обещан и в решении ADR («лицензия — в
  `THIRD-PARTY`»), и в карточке T1-26 (в `files:`), но в репозитории не существует.
- Уточнение по факту: текст лицензии MIT с копирайтом не отсутствует целиком — он лежит в
  `app-gpui/res/icons/README.md` («Лицензия»). Пропуск — в отсутствии обещанного корневого файла, единая
  точка атрибуции компонентов третьих сторон; README набора в выпускаемые комплекты не входит, а значки
  вшиты в exe (`include_bytes!`). Условие MIT («копирайт и лицензия — во всех копиях») для дистрибутива
  этим не закрыто.

## Сделано
- `THIRD-PARTY` в корне: назначение, состав (значки `app-gpui/res/icons/*.svg`, вшиты в exe), источник
  `@fluentui/svg-icons`, полный текст MIT с копирайтом (согласован с текстом из README набора,
  «Copyright (c) 2020 Microsoft Corporation») и ссылка на дублирующий текст в README.
- `app-gpui/res/icons/README.md`: строка-указатель на `THIRD-PARTY` (текст в README оставлен — рядом с активами).

## Не сделано (адресно)
- Атрибуция прочих сторонних компонентов (в т.ч. значки Lucide из gpui-kit — идут зависимостью crates.io,
  лицензии крейтов проверяет `cargo deny`): решение владельца, отдельная карточка при необходимости.
