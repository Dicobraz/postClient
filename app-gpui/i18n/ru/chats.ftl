# Беседы каналов (CH-04, DESIGN «Каналы»): дерево, список, лента чата с полем ввода.
chats-channel-mail = Почта
chats-channel-demo = Демо-канал
chats-all = Все беседы
chats-group-pinned = Закреплённые
chats-group-others = Остальные
chats-last-mine = Вы: { $text }
chats-last-author = { $author }: { $text }
chats-empty-list-title = Бесед пока нет
chats-empty-list-text = Когда в канале появятся беседы, они будут здесь.
chats-empty-pane-title = Выберите беседу
chats-empty-pane-text = Лента сообщений и поле ввода появятся здесь.
chats-loading = Загрузка беседы…
chats-no-messages = Сообщений пока нет.
chats-earlier = Показать более ранние
chats-new-messages = Новые сообщения
chats-kind-direct = личная беседа
chats-kind-group = группа
chats-kind-broadcast = рассылка
chats-kind-mail = беседа почты
chats-members = { $n ->
    [one] { $n } участник
    [few] { $n } участника
   *[many] { $n } участников
}
chats-encrypted = сквозное шифрование
chats-mute = Заглушить
chats-unmute = Вернуть звук
chats-pin = Закрепить
chats-unpin = Открепить
chats-muted = Беседа заглушена.
chats-unmuted = Звук беседы возвращён.
chats-pinned = Беседа закреплена.
chats-unpinned = Беседа откреплена.
chats-pin-full = Закреплено уже { $n } бесед — открепите одну, чтобы закрепить эту.
chats-input-placeholder = Сообщение — Enter, чтобы отправить
chats-send = Отправить
chats-save = Сохранить правку
chats-emoji = Эмодзи
chats-cancel = Отмена
chats-read-only = В этой беседе пишут только владельцы.
chats-reply = Ответить
chats-edit = Изменить
chats-react = Реакция
chats-more = Ещё
chats-copy = Копировать текст
chats-delete-own = Удалить у себя
chats-delete-all = Удалить у всех
chats-delete-own-ask = Удалить сообщение у себя?
chats-delete-all-ask = Удалить сообщение у всех участников?
chats-delete-yes = Удалить
chats-reply-to = Ответ: { $who }
chats-reply-gone = Сообщение недоступно
chats-editing = Изменение сообщения
chats-edited = изменено
chats-sending = отправляется
chats-unsend = Отменить
chats-forwarded = Переслано от { $who }
chats-deleted = Сообщение удалено
chats-undecryptable = Сообщение не расшифровано: придёт ключ — откроется.
chats-att-voice = Голосовое сообщение
chats-att-contact = Контакт
chats-service-created = Беседа создана
chats-service-joined = { $who } — в беседе
chats-service-left = { $who } — больше не в беседе
chats-service-renamed = Новое название: { $title }
chats-service-pinned = Сообщение закреплено
chats-typing-one = { $who } печатает…
chats-typing-many = { $who } печатают…
chats-too-long = Сообщение длиннее { $max } знаков: канал такое не примет.
chats-send-failed = Сообщение не отправлено. { $why }
chats-err-network = Нет связи с каналом: повторится, когда связь появится.
chats-err-signed-out = Вход в канал истёк. Войдите снова в «Настройки → Каналы».
chats-err-wait = Канал просит подождать { $secs } с.
chats-err-denied = Канал отказал.
chats-err-not-found = Беседы или сообщения уже нет.
chats-err-too-large = Слишком большое для этого канала.
chats-err-unsupported = Канал этого не умеет.
chats-err-other = Не удалось выполнить действие в канале.
chats-err-plugin = Компонент канала не работает: нажмите «Повторить», чтобы подключиться снова.
chats-err-not-running = Канал выключен или учётка не подключена: включите его в настройках.
chats-pick-first = Сначала выберите беседу.
chat-bar-pin = Закрепить
chat-bar-mute = Заглушить
chat-bar-read = Прочитано
chat-bar-reply = Ответить
chats-poll-line = Опрос: { $question }
chats-poll-closed = Опрос закрыт
chats-poll-votes = { $n ->
    [one] { $n } голос
    [few] { $n } голоса
   *[many] { $n } голосов
}
chats-poll-vote = Проголосовать
chats-poll-retract = Отозвать голос
chats-ok = ОК
chats-link-offered = Бот предлагает открыть ссылку
chats-link-open = Открыть
chats-demo-alert = Сборка перезапущена. Журнал — по ссылке.
chats-smart = Умные папки
chats-smart-hint = Беседы всех каналов и почты в одном списке. Что показывать в дереве папок:
chats-smart-unread = Непрочитанное везде
chats-smart-mentions = Упоминания меня
chats-smart-pinned = Закреплённые
chats-smart-forget = Убрать папку человека
chats-person-no-channels = Каналы бесед не подключены: включите канал в «Настройки → Каналы».
chats-policy = Политика организации
chats-policy-set = задано организацией
chats-policy-on = Включён, выключить нельзя
chats-policy-off = Запрещён
chats-none-title = Каналов бесед нет
chats-none-text = В этой сборке каналов нет или они запрещены политикой организации.
chats-empty-unread-title = Всё прочитано
chats-empty-unread-text = Непрочитанных бесед нет ни в почте, ни в каналах.
chats-empty-mentions-title = Упоминаний нет
chats-empty-mentions-text = Беседы, где вас упомянули в непрочитанном, появятся здесь.
chats-empty-pinned-title = Закреплённых бесед нет
chats-empty-pinned-text = Закрепите беседу — она будет здесь и вверху своей папки.
chats-empty-person-title = Бесед с этим человеком нет
chats-empty-person-text = Ни в почте, ни в подключённых каналах бесед с ним не нашлось.
chats-off-title = Каналы бесед выключены
chats-off-text = Беседы Telegram и Matrix рядом с почтой: одна лента, общий поиск, умные папки.
chats-turn-on = Включить
chats-attach = Вложить файл
chats-file-remove = Убрать вложение
chats-uploading = { $n ->
    [one] Выгружается { $n } файл…
    [few] Выгружаются { $n } файла…
   *[many] Выгружаются { $n } файлов…
}
chats-wait-upload = Файлы ещё выгружаются — отправьте, когда закончится.
chats-file-too-large = Файл «{ $name }» больше, чем принимает канал.
chats-file-unread = Файл «{ $name }» не прочитан.
chats-upload-failed = Файл не выгружен. { $why }
