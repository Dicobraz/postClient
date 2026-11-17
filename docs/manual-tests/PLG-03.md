# Ручная проверка PLG-03: набор плагинов в пакетах трёх ОС

Проверяет владелец на артефактах выпуска (job `release` прогонов CI или страницы Releases): набор `postman-plugins.exe`
(общий файл — почта и каталог вместе, решение 2026-11-16 `docs/decisions/2026-11-16-plugin-files.md`; сейчас в нём
демо-канал) лежит в каждом пакете рядом с `postman`, подписан тем же сертификатом и заменяется самообновлением вместе
с программой. Демо-канал поднимается процессом только в отладочной сборке с `--demo`; в выпуске набор просто лежит
в пакете и проверяется подсчётом процессов (его не должно быть без включённого канала). Перед проверкой закрыть
Postman («Выход» в меню значка в трее).

## Windows

### MSI (нужны права администратора)

1. Установить `postman-<версия>-windows-x64.msi` двойным щелчком (UAC — подтвердить). В `C:\Program Files\Postman`
   лежат `postman.exe`, `postman-worker.exe`, `postman-viewer.exe` и **`postman-plugins.exe`** (свойства файла:
   «Цифровые подписи» есть, если сборка подписана).
2. Запустить Postman, открыть демо (`postman.exe --demo` из папки установки): окно работает, набор не нужен (в выпуске
   демо-канал в своём процессе внутри `postman.exe`); в «Диспетчере задач» процесса `postman-plugins` нет.
3. Обновление: поставить MSI следующей версии поверх (шаг «Обновить» вручную или тост самообновления с UAC). Файл
   набора в папке установки — новой версии (та же версия файла, что у `postman.exe`, на вкладке «Подробно»).
4. Прерванное обновление (по желанию): скопировать в папку установки `postman-plugins.exe` от другой версии и
   запустить Postman с `--demo` отладочной сборки — плагин не используется, в журнале ошибка сверки, данные целы
   (механизм PLG-02: сверка версий в `Hello`). Вернуть файл своей версии.
5. Удаление: «Параметры → Приложения → Postman → Удалить». Папка установки пуста; в
   `HKCU\Software\Classes\Local Settings\Software\Microsoft\Windows\CurrentVersion\AppContainer\Mappings` не осталось
   профилей с Moniker `Postman.MailWorker` и `Postman.Plugins` (regedit, поиск по «Postman.»). Данные
   (`%LOCALAPPDATA%\Postman`) и сохранённый вход остались.

### Архив (без прав администратора)

6. Распаковать `postman-<версия>-windows-x64.zip`: в папке `postman-plugins.exe` и `install.ps1`. Выполнить
   `powershell -ExecutionPolicy Bypass -File .\install.ps1`: в `%LOCALAPPDATA%\Programs\Postman` — все файлы, включая
   набор; ярлык «Postman» в «Пуске».
7. Самообновление портативной копии: тостом «Установить» (или кнопкой) обновиться на следующую версию — после
   перезапуска в папке установки `postman.exe` и `postman-plugins.exe` новой версии, `postman.portable` на месте,
   профиль и настройки целы.
8. `powershell -ExecutionPolicy Bypass -File .\install.ps1 -Uninstall`: файлы и ярлык сняты (включая набор), профили
   AppContainer (оба, см. п. 5) сняты, данные остались.

## macOS

1. Смонтировать `postman-<версия>-macos-arm64.dmg`, перетащить `Postman.app` в «Программы». В
   `/Applications/Postman.app/Contents/MacOS` — `postman`, `postman-worker`, `postman-viewer` и **`postman-plugins`**
   (`codesign --verify --deep --strict /Applications/Postman.app` — подпись действительна, набор подписан вместе
   с воркером; Терминал: `codesign -dv` показывает ту же подпись у всех четырёх).
2. Запуск (первый — «Всё равно открыть», подпись ad-hoc): окно работает; `ps aux | grep postman-plugins` — пусто
   (в выпуске набор не запускается).
3. Обновление: «Настройки → Общие → Проверить обновления» → «Установить» — бандл заменяется целиком: после
   перезапуска `Contents/MacOS/postman-plugins` — новой версии (`--version` у файла не выводится; сверить дату и
   размер с архивом выпуска).
4. Удаление: `Postman.app` в Корзину. Профиль (`~/Library/Application Support/Postman`) и журнал
   (`~/Library/Logs/Postman`) остаются — удалить вручную при желании.

## Linux

1. Пакет deb (Ubuntu/Debian): `sudo apt install ./postman-<версия>-linux-x64.deb`. В `/usr/bin` — `postman`,
   `postman-worker`, `postman-viewer` и **`postman-plugins`**; `dpkg -L postman | grep plugins` — путь; на системах
   с AppArmor 4 (Ubuntu 24.04) `/etc/apparmor.d/postman` содержит профиль `postman-plugins` (`userns`).
2. Пакет rpm (Fedora/Alma): `sudo dnf install ./postman-<версия>-linux-x64.rpm` — то же (`rpm -ql postman`).
3. AppImage: `chmod +x` и запустить — работает без установки; в диагностике песочницы видно, что профиля AppArmor нет
   (как у воркера).
4. `postman --version` отвечает; процесс набора не запускается (`pgrep -af postman-plugins` — пусто) — в выпуске
   набор лежит в пакете и ждёт волн PLG-04…10.
5. Обновление пакета: `sudo apt install ./postman-<новая>-linux-x64.deb` (или `dnf upgrade`) — все четыре бинаря
   новой версии; AppImage — самообновлением, файл заменяется рядом целиком.
6. Удаление: `sudo apt remove postman` (dnf: `sudo dnf remove postman`) — все четыре бинаря и
   `/etc/apparmor.d/postman` сняты; `~/.local/share/Postman` (данные) остался.
