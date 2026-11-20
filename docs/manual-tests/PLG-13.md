# Ручная проверка PLG-13: каналы в MSI — состав CHANNELS и обновления

Проверяет владелец на MSI из job `release` (или локальной сборки `postman-pack msi`): Telegram и Matrix ставятся
по умолчанию вместе со всем (решение 2026-11-19 `docs/decisions/2026-11-19-components.md`); состав каналов для
сетевой раскатки задаёт `CHANNELS`; выбор переживает обновления; `REMOVE` снимает канал. Скрипт
`scripts\windows\msi-check.ps1` проверяет то же автоматически (только на CI-раннере — он меняет систему).
Все команды — от администратора. Перед проверками закрыть Postman.

1. **Установка по умолчанию**: `msiexec /i postman-<версия>-windows-x64.msi /qn`. В `C:\Program Files\Postman`
   лежат, кроме базы, `postman-plugins-telegram.exe` и `postman-plugins-matrix.exe`; в реестре
   `HKLM\SOFTWARE\Pets\Postman\Channels` — значения `Telegram` и `Matrix`. Запустить Postman: без учёток каналов
   процессов `postman-plugins-telegram`/`-matrix` в «Диспетчере задач» нет (плагины по запросу, ADR 0013).
2. **Без каналов**: `msiexec /i <msi> /qn CHANNELS=none` поверх — файлов каналов нет, значения в
   `SOFTWARE\Pets\Postman\Channels` ушли, база (`postman.exe`, воркер, просмотрщик, набор) на месте.
3. **Обновление держит состав**: поставить MSI следующей версии поверх без свойств (`msiexec /i … /qn` или тост
   самообновления) — каналы не вернулись (файлов нет, маркеров нет).
4. **Частичный состав**: `msiexec /i <msi> /qn CHANNELS=Telegram` — вернулся только Telegram (файл и маркер),
   Matrix не появился.
5. **Снять канал**: `msiexec /i <msi> /qn REMOVE=Telegram` — Telegram ушёл; следующее обновление без свойств его
   не возвращает.
6. **Возврат всего**: `msiexec /i <msi> /qn` (без CHANNELS на свежей машине) — оба канала на месте.
7. **Удаление**: «Параметры → Приложения → Postman → Удалить» — ключ `SOFTWARE\Pets\Postman\Channels` в реестре
   исчез целиком; профили AppContainer `Postman.MailWorker` и `Postman.Plugins` сняты (как в PLG-03); данные
   `%LOCALAPPDATA%\Postman` остались.
