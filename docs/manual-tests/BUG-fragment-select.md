# Ручная проверка BUG-fragment-select (MacBook владельца, ~10 минут)

Цель: на «живом» macOS подтвердить причину флака сцены `fragment-select` кадром момента отказа — видно, по какому
месту письма прошла протяжка (пустое место — гонка укладки; зависшее выделение — другое). Проверка без сети и без
ящика: демо-режим.

## Подготовка

Сборка ветки с фиксом и диагностикой (или готовый `.app` из артефакта `postman-0.1.0-beta.7-macos-arm64.zip`
прогона Actions ветки `fix/screens-drag`):

```bash
git fetch && git checkout fix/screens-drag
cargo build --release -p postman-gpui -p postman-worker -p postman-plugins
```

## Шаги

1. Обычный прогон трёх сцен (фикс должен пройти; если упало — снимок `<сцена>-fail.png` появится рядом):
   ```bash
   mkdir -p shots && ./target/release/postman --screens shots --lang ru
   ```
   Ждём: все три файла `reply-inline.png`, `fragment-select.png`, `reply-fragments.png`, выход 0.
2. Режим «снять момент отказа и продолжить» — если на шаге 1 сцена застряла:
   ```bash
   mkdir -p shots2 && POSTMAN_SCREENS_SHOT_ON_FAIL=1 POSTMAN_SCREENS_ONLY=reply-inline,fragment-select,reply-fragments \
     ./target/release/postman --screens shots2 --lang ru
   ```
   Ждём: в `shots2/fragment-select.png` — кадр в момент зависания.
3. Что смотреть в кадре момента отказа:
   - **выделения нет, протяжка прошла по пустому месту/подписи картинки** — гонка укладки (ожидалось, фикс прав);
   - **выделение висит, кнопка «Ответить на фрагмент» есть** — протяжка не завершилась (другая причина — сообщите,
     поправим фикс);
   - **текст выделен, но сцена всё равно ждала** — куски выделения не собираются отрисовкой.
4. Итог: приложить `fragment-select.png` (и `-fail.png`, если был) к карточке `docs/tasks/BUG-fragment-select.md`
   — владелец или агент вписывает вывод одной строкой.

## Чистота

Своих данных нет (демо-ящик), сеть не нужна, настройки не меняются; рабочая копия демо удаляется сама.
