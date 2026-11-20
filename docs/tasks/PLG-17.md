# PLG-17: пакеты без файлов каналов, компонентные ассеты выпуска

lane: x/plg-17-pack        # scripts (полоса ci) — упаковка трёх ОС
status: done
deps: [PLG-16]
class: B                   # состав пакета и выпуск
files: [scripts/windows/package.ps1, scripts/linux/package.sh, scripts/linux/data/apparmor/postman, scripts/macos/package.sh, docs/DEV.md]
read: [docs/adr/0013-plugins-on-demand.md, docs/decisions/2026-11-19-components.md, docs/decisions/2026-11-16-plugin-files.md]
checks: [release]
manual: yes                # docs/manual-tests/PLG-17.md: пакеты без каналов, ассеты, докачка на трёх ОС

## Задача
- Базовые пакеты — без файлов каналов (решение владельца 2026-11-19 «components»: «пакеты без каналов», вес
  каналов — в их файлах). MSI не меняется: оба файла в его кабинете (офлайн-раскатка), по умолчанию ставятся
  (PLG-13).
- В выпуск — компонентные zips `postman-plugins-<канал>-<ОС>-<арх>.zip` (+ `.minisig` от publish, glob уже
  покрывает `*.zip`): их докачивает приложение (PLG-15) и принимает `--install-component` (PLG-16).

## Сделано
- `scripts/windows/package.ps1`: zip собирается по списку — база без `postman-plugins-telegram|matrix.exe`;
  MSI по-прежнему из полного комплекта (`$dir`); в `dist/assets` — два компонентных zip (внутри exe под своим
  именем, Compress-Archive).
- `scripts/linux/package.sh`: `bins` — база; каналы — в отдельном списке `components` (проверки существования,
  glibc-минимум и связанные библиотеки — по всем, пакуются только `bins`); rpm `%files` без двух строк;
  компонентные zip — python3 `zipfile` (deflate), имя записи — имя файла (проверено: ровно то, что ищет
  `extract` в приложении).
- `scripts/linux/data/apparmor/postman`: профили `/usr/bin/postman-plugins-telegram|matrix` → один паттерн на
  каталог компонентов `/home/*/.local/share/Postman/Components/**` (AppArmor 4 разрешает userns по пути
  вложения; deb/rpm ставят профиль, компоненты докачиваются в этот каталог позже).
- `scripts/macos/package.sh`: бандл без helpers-каналов; каналы подписываются тем же вызовом там, где их
  собрала сборка, и упаковываются `ditto` в свои zip в assets.
- `docs/DEV.md` «Выпуск»: строка таблицы про компонентные zips, уточнение состава zip/MSI.

## Не сделано (адресно)
- Замеры размеров (установка легче на ~26 МиБ, MSI не меняется; строка `docs/SIZE.md`) — по факту первой
  release-сборки этой волны, догоняющим коммитом (решение «no-blocking-measurements»).
- Полный сценарий докачки на трёх ОС — `docs/manual-tests/PLG-17.md` и ручная проверка владельца после первого
  прогона release этого PR.
