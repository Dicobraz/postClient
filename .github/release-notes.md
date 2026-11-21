<!-- Страница выпуска для пользователей: job `publish` (ci.yml) вырезает лишние блоки и подставляет {version}. -->
<!-- pre -->
**Бета-версия.** Основное работает, но возможны ошибки. Нашли ошибку — «Настройки» → «Общие» → «Отчёт для поддержки» и приложите файл к сообщению.

**Как перейти на бету с 0.2.0.** Стабильной версии беты не предлагаются: включите канал «Предварительный» — «Настройки» → «Общие» → «Обновления» → «Канал: Предварительный», — и эта версия предложится сама. Версии 0.1.0-беты обновлением её не получат: у выпусков с 0.2.0 новый ключ подписи обновлений — один раз установите версию вручную (файлы ниже), письма и вход сохранятся.
<!-- /pre -->

## Что нового

- **Надёжнее отправка и перенос.** «Отправить позже» у серверов JMAP снова держит письмо на самом сервере: оно уйдёт в назначенный час, даже если Postman закрыт. У письма-«шага» поручения снова есть окно «Отменить» сразу после отправки. Календарь и контакты у учётной записи IMAP подхватываются сами при добавлении, как раньше.
- **Каналы — кнопка «Повторить»**: если Telegram или Matrix не подключился, в «Настройки» → «Каналы» можно повторить подключение сразу, без перезапуска Postman.
- **Экспорт**: в списке форматов — «Apple mbox» (пакет папки для импорта в Apple Mail); экспорт в MSG (Outlook) снова доступен.
- **Безопасность**: политика «только TLS 1.3» теперь действует и для почты Exchange (EWS), и для IMAP/SMTP.
- **Просмотр вложений**: модули просмотра работают в заявленных пределах; если модуль не загрузился, в настройках видно «Не загружается» с причиной.
<!-- stable -->
**Стабильная версия** для повседневной работы.
<!-- /stable -->

## Скачать — Windows 10 и 11 (64 бита)

| | Что это | Права |
|---|---|---|
| [**Установщик** (.msi)](https://github.com/Dicobraz/postClient/releases/download/v{version}/postman-{version}-windows-x64.msi) | Обычный выбор. Откройте файл — Postman установится для всех на этом компьютере и появится в меню «Пуск». | Нужны права администратора: Windows попросит подтвердить. |
| [**Архив** (.zip)](https://github.com/Dicobraz/postClient/releases/download/v{version}/postman-{version}-windows-x64.zip) | Если прав администратора нет. Распакуйте и запустите `install.ps1` — как, написано в `README.txt` внутри. Postman установится только для вас. | Не нужны. |

Обновления Postman ставит сам и спрашивает перед установкой. Вручную — так же, как установка: новая версия ставится поверх старой, письма и вход сохраняются. Перед этим выйдите из Postman: «Выход» в меню его значка в трее (крестик только прячет окно).
<!-- unsigned -->

Если Windows пишет «Система Windows защитила ваш компьютер», нажмите «Подробнее» → «Выполнить в любом случае».
<!-- /unsigned -->

Остальные файлы ниже (`postman-worker.exe`, `postman-viewer.exe`, `postman-plugins-telegram-windows-x64.zip`, `postman-plugins-matrix-windows-x64.zip`) нужны, только если Postman сам попросит скачать недостающий компонент — вкладка «Каналы» в настройках предложит «Скачать» (или «Установить из файла…» с zip-файлом компонента, без интернета).

## Скачать — macOS 13 и новее, Apple Silicon (M1 и новее)

| | Что это |
|---|---|
| [**Образ диска** (.dmg)](https://github.com/Dicobraz/postClient/releases/download/v{version}/postman-{version}-macos-arm64.dmg) | Обычный выбор. Откройте файл и перетащите **Postman** на ярлык **Applications** («Программы») в том же окне. |
| [**Архив** (.zip)](https://github.com/Dicobraz/postClient/releases/download/v{version}/postman-{version}-macos-arm64.zip) | То же приложение: распакуйте двойным щелчком и перенесите `Postman.app` в «Программы». |

На Mac с процессором Intel Postman не работает.

Обновления Postman ставит сам: когда выйдет новая версия, он предложит её установить («Настройки» → «Общие» → «Проверить обновления» — сразу). Письма и вход сохраняются.

<!-- mac-unsigned -->

**Первый запуск.** Подписи Apple у Postman пока нет, поэтому macOS при первом открытии откажет: «Не удаётся проверить разработчика» или «Postman повреждён». Это нужно сделать один раз:

1. Откройте Postman из «Программ» — появится отказ, нажмите «Готово».
2. «Системные настройки» → «Конфиденциальность и безопасность» → внизу «Postman заблокирован…» → **«Всё равно открыть»**, подтвердите паролем или Touch ID.
3. Снова откройте Postman → «Открыть».

Или одной командой в Терминале: `xattr -dr com.apple.quarantine /Applications/Postman.app`
<!-- /mac-unsigned -->

## Скачать — Linux (x86_64)

Предварительно: Linux пока проверяется только автоматически. Нужен 64-битный дистрибутив не старше Ubuntu 22.04, Debian 12, Fedora 35 или RHEL (Alma, Rocky) 9.

| | Что это |
|---|---|
| [**Пакет для Ubuntu и Debian** (.deb)](https://github.com/Dicobraz/postClient/releases/download/v{version}/postman-{version}-linux-x64.deb) | `sudo apt install ./postman-{version}-linux-x64.deb` — Postman появится в меню приложений. |
| [**Пакет для Fedora, RHEL и openSUSE** (.rpm)](https://github.com/Dicobraz/postClient/releases/download/v{version}/postman-{version}-linux-x64.rpm) | `sudo dnf install ./postman-{version}-linux-x64.rpm` (openSUSE — `sudo zypper install …`). |
| [**AppImage**](https://github.com/Dicobraz/postClient/releases/download/v{version}/postman-{version}-linux-x64.AppImage) | Один файл для любого дистрибутива, без установки: сделайте его исполняемым (`chmod +x`) и запустите. |

Пакеты .deb и .rpm лучше AppImage на Ubuntu 23.10 и новее: с ними вложения открываются в более строгой изоляции. Обновления: AppImage Postman обновляет сам; с пакетами .deb и .rpm он сообщит о новой версии и даст ссылку «Скачать» — поставьте её так же, как при установке, письма и вход сохранятся.

*English:* Beta 0.3.0-beta.1. More reliable sending and moving: send-later for JMAP servers keeps the message on the server itself (it goes out on time even if Postman is closed), the Undo window is back on delegation-step emails, and calendar/contacts over DAV are detected again for IMAP accounts. Channels: a "Retry" button in Settings → Channels when Telegram or Matrix fails to connect. Export: "Apple mbox" (a folder package for Apple Mail) in the format list, and MSG (Outlook) export enabled again. Security: the "TLS 1.3 only" policy now also covers Exchange (EWS) mail and IMAP/SMTP. Attachment viewers: module limits are enforced, and Settings show "Does not load" with the reason. Postman updates itself (Settings → General → Updates; betas arrive on the "Pre-release" channel). Windows — the **.msi** installs Postman for all users (administrator rights required); the **.zip** installs it for you only, no administrator rights needed. macOS (Apple Silicon, 13+) — open the **.dmg** and drag Postman onto Applications. Linux (x86_64) — the **.deb** (Ubuntu, Debian) or **.rpm** (Fedora, RHEL 9, openSUSE) package, or the **.AppImage** for any distribution.
<!-- mac-unsigned -->
The app is not signed by Apple yet: on first launch go to System Settings → Privacy & Security → "Open Anyway", or run `xattr -dr com.apple.quarantine /Applications/Postman.app`.
<!-- /mac-unsigned -->
