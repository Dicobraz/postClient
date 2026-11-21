# AUD-02: «Отправить позже» у JMAP уходит серверу (FUTURERELEASE), а не держится локально

lane: x/aud-core
status: done
deps: []
class: A                   # в карточке PLG-10 заявлено («Отправить позже» сервером), а обойдено очередью ядра
files: [crates/core/src/backend/mail.rs, crates/core/src/backend/remote.rs, crates/core/src/model.rs, crates/backend-jmap/src/convert.rs, crates/demo/src/lib.rs, app-gpui/src/features.rs]
read: [docs/adr/0013-plugins-on-demand.md, docs/specs/S5.md, docs/STALWART.md]
checks: [nextest]
manual: no                 # тесты: фича объявлена — команда уходит пути; не объявлена — локальная очередь

## Задача
- `command()` сессии ядра перехватывал `Send` с `Release::After/At` у всех путей, не проверяя поддержку
  серверного планирования, — у JMAP (FUTURERELEASE, crates/backend-jmap/src/sync.rs) письмо вместо сервера
  держала локальная очередь ядра: закрыли Postman — запланированное не ушло в свой час.
- Сделать: путь декларирует поддержку фичей (`Feature::ServerSchedule`, по образцу существующих `Feature`);
  ядро держит письмо локально только когда фичи нет; когда есть — форвардит `Send` с release как есть
  («Отменить» — существующий fallback `Unsend`→forward). Списки ждущих (`Command::Scheduled`) — тоже пути.

## Сделано
- `Feature::ServerSchedule` в `model::Feature::ALL` (ядро); JMAP объявляет по `maxDelayedSend` сервера
  (`convert::features` — нет FUTURERELEASE, нет фичи); IMAP и EWS не объявляют.
- Демо — один к одному с `Feature` (`postman_demo::Has.server_schedule`: Stalwart — да, IMAP и EWS — нет),
  перевод в интерфейсе (`of_demo`) — тем же списком.
- Сессия хоста запоминает возможности пути: `MailScope.features` (remote.rs) из `Event::Features`; до первого
  события — `NONE` (консервативно: держим локально).
- `command()`: с фичей `Send` с `Release::After/At` и `Command::Scheduled` уходят пути как есть; без фичи —
  как было, очередь ядра (письмо в базе профиля, окно «Отменить», ответ `Scheduled` с `max_delay: 0`).
- Тесты: `server_schedule_sends_go_to_the_path` (команда и список ждущих ушли пути, очередь пуста),
  `without_server_schedule_the_letter_is_queued_locally` (очередь ядра, свой список с пределом 0);
  `MockMail` объявляет фичи `Event::Features` до ответа на вход — как JMAP сразу после входа.

## Не сделано (адресно)
- Путь с фичей не держит у ядра и списка ждущих — список приходит от сервера (`notify_scheduled` у JMAP
  отвечает `Event::Scheduled` сам); локальная очередь при этом пуста.
- JMAP-сервер без FUTURERELEASE (не Stalwart): фича не объявлена, шаг поручения (AUD-03) держится локальной
  очередью ядра — правка своей копии задачи доезжает до сервера обычной сверкой задач, а не первым шагом
  `delegate` плагина; для целевого стенда (Stalwart) разницы нет.
