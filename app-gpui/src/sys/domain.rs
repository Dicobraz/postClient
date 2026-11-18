//! Контроллер домена компьютера — функция ОС (решение владельца №12 — Active Directory,
//! `docs/decisions/2026-11-02-ldap-active-directory.md`): подсказка «Настройки → Безопасность → Каталог» и сервер
//! каталога по умолчанию в запросе плагину (PLG-06, ADR 0013 — каталог работает процессом набора, сервер приносит
//! запрос). Прежде это звало плагин LDAP (`postman_backend_ldap::domain_controller`), теперь — здесь: процессу
//! песочницы файлы и билет Kerberos не нужны.
//!
//! Windows — служба каталогов (`DsGetDcNameW`); macOS — DNS SRV по области билета Kerberos через mDNSResponder
//! ([`macos`], MAC-05); Linux — DNS SRV через резолвер glibc по области билета MIT Kerberos ([`linux`], LIN-05).
//! Единственное место `unsafe` о каталоге в интерфейсе; у каждого блока — почему он безопасен.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod srv;

/// Поиск контроллера домена — не дольше на всех ОС (PAR-AUDIT №35, ADR 0011): служба Windows, DNS SRV macOS и Linux.
#[cfg_attr(not(any(windows, target_os = "macos", target_os = "linux")), allow(dead_code))]
const DC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Блокирующий поиск `find` в отдельном потоке, не дольше [`DC_TIMEOUT`]: вышло время — `None` (как «не в домене»),
/// поток доживает сам. События — DEBUG: детали ОС, без имён.
#[cfg_attr(not(any(windows, target_os = "macos", target_os = "linux")), allow(dead_code))]
pub(crate) fn bounded<T: Send + 'static>(find: impl FnOnce() -> Option<T> + Send + 'static) -> Option<T> {
    let (tx, rx) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new().name("directory-dc".into()).spawn(move || {
        let _ = tx.send(find());
    });
    if spawned.is_err() {
        tracing::debug!("directory: domain controller lookup thread not started");
        return None;
    }
    match rx.recv_timeout(DC_TIMEOUT) {
        Ok(found) => found,
        Err(_) => {
            tracing::debug!("directory: domain controller lookup timed out");
            None
        }
    }
}

/// Windows: служба каталогов компьютера (`DsGetDcNameW`).
#[cfg(windows)]
pub(crate) fn domain_controller() -> Option<String> {
    bounded(dc_service)
}

#[cfg(windows)]
fn dc_service() -> Option<String> {
    /// <dsgetdc.h>: сведения о контроллере домена; читаем только имя, объявлен используемый префикс.
    #[repr(C)]
    struct DomainControllerInfoW {
        domain_controller_name: *mut u16,
        #[allow(dead_code)]
        domain_controller_address: *mut u16,
    }

    #[link(name = "netapi32")]
    unsafe extern "system" {
        fn DsGetDcNameW(
            computer_name: *const u16,
            domain_name: *const u16,
            domain_guid: *const u8,
            site_name: *const u16,
            flags: u32,
            info: *mut *mut DomainControllerInfoW,
        ) -> u32;
        fn NetApiBufferFree(buffer: *mut std::ffi::c_void) -> u32;
    }

    const DS_DIRECTORY_SERVICE_REQUIRED: u32 = 16;
    const DS_RETURN_DNS_NAME: u32 = 0x4000_0000;

    let mut info: *mut DomainControllerInfoW = std::ptr::null_mut();
    // SAFETY: компьютер, домен, GUID и сайт — null (текущие); `info` — выходной указатель на буфер NetApi, который
    // при успехе освобождается ниже ровно один раз.
    let rc = unsafe {
        DsGetDcNameW(
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            DS_DIRECTORY_SERVICE_REQUIRED | DS_RETURN_DNS_NAME,
            &mut info,
        )
    };
    if rc != 0 || info.is_null() {
        tracing::debug!(rc, "directory: no domain controller (DsGetDcNameW)");
        return None;
    }
    // SAFETY: успех — `info` указывает на заполненную `DOMAIN_CONTROLLER_INFOW`; имя — строка UTF-16 с нулём на
    // конце (или null), живёт до `NetApiBufferFree`.
    let name = unsafe { wide((*info).domain_controller_name) };
    // SAFETY: буфер выдал `DsGetDcNameW`, после этой строки он не используется.
    unsafe { NetApiBufferFree(info.cast()) };
    name.map(|n| n.trim_start_matches('\\').to_owned()).filter(|n| !n.is_empty())
}

/// Строка UTF-16 с нулём на конце.
///
/// # Safety
/// `p` — null или указатель на строку UTF-16, оканчивающуюся нулём.
#[cfg(windows)]
unsafe fn wide(p: *const u16) -> Option<String> {
    if p.is_null() {
        return None;
    }
    let mut len = 0usize;
    // SAFETY: по договору функции строка оканчивается нулём — чтение не выходит за неё.
    while unsafe { *p.add(len) } != 0 {
        len += 1;
    }
    // SAFETY: `len` символов до нуля прочитаны выше.
    Some(String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, len) }))
}

/// macOS: DNS SRV по области билета Kerberos (MAC-05).
#[cfg(target_os = "macos")]
pub(crate) fn domain_controller() -> Option<String> {
    macos::domain_controller()
}

/// Linux: DNS SRV по области билета MIT Kerberos (LIN-05).
#[cfg(target_os = "linux")]
pub(crate) fn domain_controller() -> Option<String> {
    linux::domain_controller()
}

/// Другие ОС: домена нет — только сервер из настроек.
#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
pub(crate) fn domain_controller() -> Option<String> {
    None
}
