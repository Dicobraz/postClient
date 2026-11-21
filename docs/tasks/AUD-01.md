# AUD-01: дискавери CalDAV/CardDAV у IMAP-учётки потерян (регресс PLG-09)

lane: x/aud-core
status: done
deps: []
class: A                   # регресс поведения S7-04в: у imaps:// области DAV снова определяются
files: [crates/core/src/backend/mail.rs]
read: [docs/adr/0004-protocol-plugins.md, docs/specs/S7.md]
checks: [nextest]
manual: no                 # покрыто тестом детекта

## Задача
- Регресс PLG-09: `RemoteMail` (заместитель почтового пути, PLG-08) не реализует `area_host`/`has_area`/
  `with_area`, поэтому `Registry::detect` (crates/core/src/backend/mod.rs) не дописывает `?dav=` в адрес
  учётки — у `imaps://` календарь и контакты перестали определяться при «Добавить учётную запись».
- Вернуть поведение до PLG-09 (реализация старого плагина: `git show b20cddd~1:crates/backend-imap/src/lib.rs`,
  строки ~173-184): области DAV ищутся на узле адреса учётки.

## Сделано
- `RemoteMail` реализовал методы `MailPlugin` (процесс не требуется — детект работает и без поднятого набора):
  `area_host` — узел адреса (`imaps://узел[:порт]?…` → `узел`) у путей со своей схемой (`Claim::Scheme`;
  IMAP своих областей не ведёт, RFC 6764); JMAP и EWS области ведут сами — у них `None`, адрес не меняется;
  `has_area`/`with_area` — проверки и дописывание `?key=…`/`&key=…` по общему `area_address`.
- Тест `detect_of_an_imap_account_finds_dav_areas`: у `imaps://` адрес дополняется `?dav=…`, порт в адрес
  области не попадает, существующий адрес области не трогается, у пути со своими областями адрес как был.

## Не сделано
- SRV-записи (RFC 6186) — как было за рамками детекта (решение S7-04в: без своего DNS-клиента).
