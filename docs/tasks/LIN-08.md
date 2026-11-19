# LIN-08: кнопки окна по настройке ОС; при кнопках слева — раскладка macOS

lane: app-gpui
status: done
deps: [LIN-02, MAC-DES]
class: A
files: [app-gpui/src/chrome.rs, app-gpui/src/mailer.rs, app-gpui/src/bar.rs, app-gpui/src/actions.rs, app-gpui/src/sys/, docs/manual-tests/LIN-08.md]
read: [docs/DESIGN.md#linux, docs/DESIGN.md#macos, docs/tasks/MAC-DES.md]
checks: [clippy, nextest, ci-linux, screens:list-left]
manual: docs/manual-tests/LIN-08.md

> Решение владельца 2026-11-14 (сессия дизайна). Макет: `MainLinuxLeft`.
> Влиты в main: 98c7bdf (PR #7; ранняя версия — dfa50b5, PR #680). CI main зелёный (nextest, clippy, 706433e).

## Сделать
1. Читать `button-layout` (XDG Settings portal, `org.gnome.desktop.wm.preferences`), следить за сменой; разбор строки
   «close,minimize:maximize» — разбор с тестом. Нет портала или ключа — справа, все три.
2. Кнопки справа — как сейчас. Слева — раскладка MAC-DES (общий код, не копия): кнопки окна вместо «светофора»,
   разделы над деревом, «Новое письмо» над списком; шестерёнка — справа в заголовке.
3. Полный экран — раскладка Windows без кнопок окна.

## Приёмка
- Тест разбора `button-layout`; снимок `list` под Xvfb с кнопками слева — как макет.

## Итог (2026-11-16)
- `sys/buttons_linux.rs` (LIN-03 portal, ashpd уже в зависимостях): чтение и слежение `button-layout`; разбор —
  `chrome::parse_buttons`, тест. Portal нет — INFO в журнал и справа, все три.
- Слева — общий код MAC-DES (не копия): `bar_sections`/`section_bar` без `cfg(macos)`; вкладки — по середине
  заголовка; шестерёнка — справа. Кнопки слева — круги 24 px, нейтральная заливка, «✕» — красная при наведении.
  Полный экран — раскладка Windows без кнопок окна; смена настройки — на лету (`refresh_windows`).
- Сцена `list-left` (только Linux, продолжает `list`): ставит настройку «close,minimize,maximize:», снимок под Xvfb;
  сброс — `chrome.rs` `reset`.
- Проверено: nextest и clippy `-D warnings` на Windows и Linux (school-99); снимки `list`, `list-left`, `letter`
  под Xvfb. Ручная проверка — отложена (стенда Linux нет, решение linux-tier1).
