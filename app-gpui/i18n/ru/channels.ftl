# Каналы бесед: Telegram в «Настройки → Аккаунты» (CH-03).
tg-channels = Каналы бесед
tg-channel = Telegram
tg-channel-hint = Беседы Telegram рядом с почтой. Канал в разработке — по умолчанию выключен.
tg-policy = Задано политикой организации.
tg-add = Добавить учётку Telegram
tg-no-key = В этой сборке нет ключа приложения Telegram: вход недоступен.
comp-missing = Компонент канала не установлен.
tg-sign-out = Выйти
tg-state-online = В сети
tg-state-connecting = Подключение…
tg-state-offline = Нет связи, повтор через { $secs } с
tg-signin-title = Вход в Telegram
tg-connecting = Подключение к Telegram…
tg-wait = Ждём ответа Telegram…
tg-phone = Номер телефона
tg-phone-hint = С кодом страны, например +7 900 000-00-00.
tg-get-code = Получить код
tg-use-qr = Войти по QR-коду
tg-use-phone = Войти по номеру телефона
tg-qr-hint = Откройте Telegram на телефоне: «Настройки» → «Устройства» → «Подключить устройство» — и наведите камеру на код.
tg-code = { $via ->
    [app] Код из приложения Telegram
    [sms] Код из SMS
    [call] Код из звонка
    [email] Код из письма
   *[other] Код
}
tg-code-length = Цифр в коде: { $n }.
tg-resend = Отправить код снова
tg-sign-in = Войти
tg-password = Пароль двухэтапной проверки
tg-password-hint = Подсказка: { $hint }
tg-err-phone = Telegram не принял номер: проверьте его и код страны.
tg-err-code = Неверный код: проверьте и введите снова.
tg-err-code-expired = Код устарел: отправьте его снова.
tg-err-password = Неверный пароль двухэтапной проверки.
tg-err-sign-up = Этот номер не зарегистрирован в Telegram: зарегистрируйтесь в официальном приложении.
tg-err-denied = Telegram отказал во входе.
tg-err-wait = Telegram просит подождать { $secs } с.
tg-err-network = Нет связи с Telegram: проверьте сеть и повторите.
tg-err-signed-out = Вход истёк: войдите снова.
tg-err-other = Не удалось войти в Telegram.

# Matrix (CH-05): вход, проверка устройства, резервная копия ключей, шифрованные беседы.
mx-channel = Matrix
mx-channel-hint = Комнаты Matrix рядом с почтой, сквозное шифрование — на этом компьютере. Канал в разработке — по умолчанию выключен.
mx-add = Добавить учётку Matrix
mx-signin-title = Вход в Matrix
mx-connecting = Подключение…
mx-wait = Ждём ответа сервера…
mx-server = Сервер
mx-server-hint = Имя сервера (например, matrix.org) или ваш адрес вида @имя:сервер.
mx-continue = Далее
mx-login = Имя пользователя
mx-login-hint = Имя на сервере или полный адрес @имя:сервер.
mx-use-browser = Войти через браузер
mx-password = Пароль
mx-sign-in = Войти
mx-browser-hint = Страница входа сервера открыта в браузере. Войдите там — Postman продолжит сам.
mx-browser-again = Открыть страницу входа снова
mx-verify = Проверить это устройство
mx-recovery = Восстановить ключи
mx-verify-title = Проверка устройства
mx-verify-wait = Подтвердите запрос на другом своём устройстве Matrix (в приложении, где вы уже вошли).
mx-verify-compare = Сравните символы с теми, что показаны на другом устройстве.
mx-verify-match = Совпадают
mx-verify-mismatch = Не совпадают
mx-verify-done = Устройство проверено: зашифрованные сообщения будут открываться.
mx-recovery-title = Восстановление ключей
mx-recovery-hint = Ключ восстановления или секретная фраза резервной копии ключей.
mx-recovery-go = Восстановить
mx-recovery-done = Ключи восстановлены из резервной копии.
mx-close = Закрыть
mx-e2e = Шифрованные беседы
mx-e2e-hint = Где хранить расшифрованный текст бесед со сквозным шифрованием. Только в памяти — после перезапуска текст расшифровывается заново, поиска нет.
mx-e2e-search = Хранить и искать
mx-e2e-store = Хранить, не искать
mx-e2e-memory = Только в памяти
mx-err-server = Сервер Matrix не найден: проверьте имя или адрес.
mx-err-login = Введите имя пользователя.
mx-err-password = Неверное имя или пароль.
mx-err-unsupported = Сервер не позволяет войти ни паролем, ни через браузер.
mx-err-browser = Вход в браузере не завершён: начните снова.
mx-err-cross-signing = У учётки не настроена проверка устройств: включите её в основном приложении Matrix.
mx-err-verify = Проверка отменена: символы не совпали или другое устройство отказалось.
mx-err-recovery = Ключ или фраза не подошли.
mx-err-denied = Сервер Matrix отказал.
mx-err-wait = Сервер просит подождать { $secs } с.
mx-err-network = Нет связи с сервером Matrix: проверьте сеть и повторите.
mx-err-other = Не удалось выполнить действие Matrix.
tg-step-phone = Телефон
tg-step-code = Код
tg-step-password = Пароль
tg-by-phone = По номеру
tg-by-qr = По QR-коду
