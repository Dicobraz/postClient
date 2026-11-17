//! Мост к GSS.framework (macOS, MAC-05) для входа учётной записью (`kerberos.rs`): контекст Kerberos и разбор
//! ошибок — `sys/macos.rs`. Контроллер домена — функция ОС в интерфейсе (`app-gpui/src/sys`, PLG-06, ADR 0013):
//! сервер каталога приносит запрос по обмену, процессу песочницы файлы и билет не нужны.

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub(crate) use macos::Kerberos;
