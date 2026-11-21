# AUD-10: генератор PARITY — обработчики через алиас `mod act`

lane: x/aud-ui               # app-gpui (commands.rs) + генерируемый docs/PARITY.md
status: done
deps: []
class: A                     # приёмка соответствия Outlook (S2-04)
files: [app-gpui/src/commands.rs, docs/PARITY.md]
read: [docs/adr/0005-part-registration.md]
checks: [nextest]
manual: no

## Задача
- `docs/PARITY.md` ложно показывал «нет» у команд с обработчиками, написанными через алиас `mod act`:
  парсер `handled()` извлекает последние два сегмента пути типа (`act::Mute`), а `action_name()` возвращает
  `mail::Mute` — не совпадает. Было «Сделано: 166 из 170», хотя все реализованы (comfort.rs — Mute/Pin,
  update.rs — Check/Install).

## Сделано
- `commands.rs` (parity): при прямом несовпадении — сравнение по имени типа (последний сегмент пути), и только
  если у команд без прямого совпадения оно одно (иначе алиас мог бы указать на чужое действие).
- Тест `aliased_action_names_do_not_collide`: коллизия имён типов у несопоставленных команд — громкий отказ
  приёмки, а не молчаливое «нет» в документе или чужое «есть».
- Документ перегенерирован как положено: `POSTMAN_UPDATE_DOCS=1 cargo test -p postman-gpui parity`.
  Итог: **Сделано: 175 из 175 (100 %)** — в реестре на этой базе 175 команд (в постановке было 170 — реестр
  уже вырос); четыре ложных «нет» (`mail.mute`, `mail.pin`, `update.check`, `update.install`) стали «есть»,
  заодно догнали отставшую строку `thread.media`.

## Не сделано (адресно)
- —
