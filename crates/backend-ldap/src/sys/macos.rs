//! macOS (MAC-05): вход учётной записью — публичный GSS-API GSS.framework (RFC 2744), мост для `kerberos.rs`.
//! Контроллер домена — функция ОС в интерфейсе (`app-gpui/src/sys`, PLG-06): сервер каталога приносит запрос.
//! Каждый блок `unsafe` — с обоснованием.

use std::ffi::{c_int, c_void};
use std::ptr::{null, null_mut};

use crate::kerberos::{Gss, GssStep};

type OmUint32 = u32;
type Name = *mut c_void;
type Ctx = *mut c_void;
type Cred = *mut c_void;

// <GSS/gssapi.h>: на Apple структуры GSS-API упакованы по 2 байта (`#pragma pack(push, 2)`).
#[repr(C, packed(2))]
struct Buffer {
    length: usize,
    value: *mut c_void,
}

impl Buffer {
    const EMPTY: Buffer = Buffer { length: 0, value: null_mut() };

    /// Вход GSS-API: функции его только читают.
    fn of(data: &[u8]) -> Buffer {
        Buffer { length: data.len(), value: data.as_ptr().cast_mut().cast() }
    }
}

#[repr(C, packed(2))]
struct Oid {
    length: OmUint32,
    elements: *const c_void,
}

#[repr(C, packed(2))]
struct OidSet {
    count: usize,
    elements: *const Oid,
}

#[repr(C, packed(2))]
struct ChannelBindings {
    initiator_addrtype: OmUint32,
    initiator_address: Buffer,
    acceptor_addrtype: OmUint32,
    acceptor_address: Buffer,
    application_data: Buffer,
}

/// Механизм Kerberos 5 (1.2.840.113554.1.2.2), DER без тега и длины.
static MECH_KRB5: [u8; 9] = *b"\x2a\x86\x48\x86\xf7\x12\x01\x02\x02";
/// Имя службы `служба@узел` (1.2.840.113554.1.2.1.4, `GSS_C_NT_HOSTBASED_SERVICE`).
static NT_HOSTBASED_SERVICE: [u8; 10] = *b"\x2a\x86\x48\x86\xf7\x12\x01\x02\x01\x04";

/// Описатель OID — в куче, не в `static`: упакованный по 2 байта указатель в данных компоновщик macOS не принимает
/// («pointer not aligned»). Живёт не меньше, чем то, чему его отдали (контекст хранит свой).
fn oid(bytes: &'static [u8]) -> Box<Oid> {
    Box::new(Oid { length: bytes.len() as OmUint32, elements: bytes.as_ptr().cast() })
}

const GSS_C_MUTUAL_FLAG: OmUint32 = 2;
const GSS_C_CONF_FLAG: OmUint32 = 16;
const GSS_C_INTEG_FLAG: OmUint32 = 32;
const GSS_S_CONTINUE_NEEDED: OmUint32 = 1;
const GSS_C_GSS_CODE: c_int = 1;
const GSS_C_MECH_CODE: c_int = 2;
const GSS_C_QOP_DEFAULT: OmUint32 = 0;

/// Ошибка GSS-API — в старших 16 битах кода (ошибка вызова и подпрограммы).
fn gss_error(major: OmUint32) -> bool {
    major & 0xFFFF_0000 != 0
}

#[link(name = "GSS", kind = "framework")]
unsafe extern "C" {
    fn gss_import_name(minor: *mut OmUint32, name: *const Buffer, name_type: *const Oid, out: *mut Name) -> OmUint32;
    fn gss_release_name(minor: *mut OmUint32, name: *mut Name) -> OmUint32;
    fn gss_init_sec_context(
        minor: *mut OmUint32,
        cred: Cred,
        ctx: *mut Ctx,
        target: Name,
        mech: *const Oid,
        flags: OmUint32,
        time_req: OmUint32,
        bindings: *const ChannelBindings,
        input: *const Buffer,
        actual_mech: *mut *const Oid,
        output: *mut Buffer,
        ret_flags: *mut OmUint32,
        time_rec: *mut OmUint32,
    ) -> OmUint32;
    fn gss_delete_sec_context(minor: *mut OmUint32, ctx: *mut Ctx, output: *mut Buffer) -> OmUint32;
    fn gss_wrap(
        minor: *mut OmUint32,
        ctx: Ctx,
        conf_req: c_int,
        qop: OmUint32,
        input: *const Buffer,
        conf_state: *mut c_int,
        output: *mut Buffer,
    ) -> OmUint32;
    fn gss_unwrap(minor: *mut OmUint32, ctx: Ctx, input: *const Buffer, output: *mut Buffer, conf_state: *mut c_int, qop: *mut OmUint32) -> OmUint32;
    fn gss_wrap_size_limit(
        minor: *mut OmUint32,
        ctx: Ctx,
        conf_req: c_int,
        qop: OmUint32,
        max_output: OmUint32,
        max_input: *mut OmUint32,
    ) -> OmUint32;
    fn gss_release_buffer(minor: *mut OmUint32, buffer: *mut Buffer) -> OmUint32;
    fn gss_display_status(
        minor: *mut OmUint32,
        status: OmUint32,
        status_type: c_int,
        mech: *const Oid,
        message_context: *mut OmUint32,
        out: *mut Buffer,
    ) -> OmUint32;
}

/// Байты буфера, выданного GSS-API, — и освободить его.
fn take(buf: &mut Buffer) -> Vec<u8> {
    let (len, ptr) = (buf.length, buf.value);
    let out = if ptr.is_null() || len == 0 {
        Vec::new()
    } else {
        // SAFETY: буфер заполнил GSS-API: `value` указывает на `length` байт, живущих до `gss_release_buffer`.
        unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), len) }.to_vec()
    };
    let mut minor = 0;
    // SAFETY: буфер выдал GSS-API (или он пуст), после этого он не используется.
    unsafe { gss_release_buffer(&mut minor, buf) };
    out
}

/// Полный текст ошибки GSS-API — только для журнала DEBUG: в сообщениях механизма Kerberos бывают принципалы и узлы
/// (PAR-AUDIT №18, как EWS).
fn status(major: OmUint32, minor: OmUint32) -> String {
    let mut parts = Vec::new();
    let krb5 = oid(&MECH_KRB5);
    for (code, kind, mech) in [(major, GSS_C_GSS_CODE, null()), (minor, GSS_C_MECH_CODE, std::ptr::from_ref(&*krb5))] {
        if code == 0 {
            continue;
        }
        let mut context = 0;
        for _ in 0..8 {
            let (mut m, mut out) = (0, Buffer::EMPTY);
            // SAFETY: все указатели — на живые локальные значения; `out` освобождает `take`.
            let rc = unsafe { gss_display_status(&mut m, code, kind, mech, &mut context, &mut out) };
            let text = take(&mut out);
            if gss_error(rc) {
                break;
            }
            parts.push(String::from_utf8_lossy(&text).into_owned());
            if context == 0 {
                break;
            }
        }
    }
    format!("GSS-API error {major:#x}/{minor:#x}: {}", parts.join("; "))
}

/// Ошибка вызова GSS для `DirectoryError`: только коды; полный текст — в журнал DEBUG.
fn fail(call: &'static str, major: OmUint32, minor: OmUint32) -> String {
    tracing::debug!(call, "ldap: {}", status(major, minor));
    format!("GSS-API {call} error {major:#x}/{minor:#x}")
}

/// Контекст Kerberos для `ldap@<узел>`: билет пользователя (вход в домен, `kinit` или единый вход Kerberos).
pub(crate) struct Kerberos {
    ctx: Ctx,
    target: Name,
    /// Привязка к каналу TLS (`tls-server-end-point:…`).
    binding: Option<Vec<u8>>,
    /// OID механизма и типа имени: GSS-API может хранить указатели на них, пока живы контекст и имя.
    mech: Box<Oid>,
    _name_type: Box<Oid>,
}

// SAFETY: контекст и имя — объекты GSS.framework без привязки к потоку; одновременных вызовов нет: методы берут
// `&mut self`, мост держит контекст под `Mutex`.
unsafe impl Send for Kerberos {}

impl Kerberos {
    pub(crate) fn new(host: &str, binding: Option<&[u8]>) -> Result<Kerberos, String> {
        let service = format!("ldap@{host}");
        let mut minor = 0;
        let mut target = null_mut();
        let name_type = oid(&NT_HOSTBASED_SERVICE);
        // SAFETY: буфер имени указывает на живую строку; тип — OID, живущий вместе с `Kerberos`; `target` — выход.
        let major = unsafe { gss_import_name(&mut minor, &Buffer::of(service.as_bytes()), &*name_type, &mut target) };
        if gss_error(major) {
            return Err(fail("gss_import_name", major, minor));
        }
        Ok(Kerberos { ctx: null_mut(), target, binding: binding.map(<[u8]>::to_vec), mech: oid(&MECH_KRB5), _name_type: name_type })
    }
}

impl Drop for Kerberos {
    fn drop(&mut self) {
        let mut minor = 0;
        // SAFETY: контекст и имя выдал GSS-API (или они null), после `drop` не используются.
        unsafe {
            if !self.ctx.is_null() {
                gss_delete_sec_context(&mut minor, &mut self.ctx, null_mut());
            }
            if !self.target.is_null() {
                gss_release_name(&mut minor, &mut self.target);
            }
        }
    }
}

impl Gss for Kerberos {
    fn step(&mut self, input: Option<&[u8]>) -> Result<GssStep, String> {
        let input = input.map(Buffer::of);
        let bindings = self.binding.as_deref().map(|b| ChannelBindings {
            initiator_addrtype: 0,
            initiator_address: Buffer::EMPTY,
            acceptor_addrtype: 0,
            acceptor_address: Buffer::EMPTY,
            application_data: Buffer::of(b),
        });
        let (mut minor, mut out, mut ret_flags) = (0, Buffer::EMPTY, 0);
        // SAFETY: учётные данные — по умолчанию (null); контекст — свой, GSS-API создаёт его на первом шаге; имя,
        // OID, привязка и входной токен живы на время вызова; `out` освобождает `take`.
        let major = unsafe {
            gss_init_sec_context(
                &mut minor,
                null_mut(),
                &mut self.ctx,
                self.target,
                &*self.mech,
                GSS_C_MUTUAL_FLAG | GSS_C_INTEG_FLAG | GSS_C_CONF_FLAG,
                0,
                bindings.as_ref().map_or(null(), std::ptr::from_ref),
                input.as_ref().map_or(null(), std::ptr::from_ref),
                null_mut(),
                &mut out,
                &mut ret_flags,
                null_mut(),
            )
        };
        let token = take(&mut out);
        if gss_error(major) {
            return Err(fail("gss_init_sec_context", major, minor));
        }
        Ok(GssStep { token, done: major & GSS_S_CONTINUE_NEEDED == 0 })
    }

    fn wrap(&mut self, encrypt: bool, data: &[u8]) -> Result<Vec<u8>, String> {
        let (mut minor, mut conf, mut out) = (0, 0, Buffer::EMPTY);
        // SAFETY: контекст установлен (`bind` зовёт после готовности); вход жив на время вызова; `out` — `take`.
        let major = unsafe { gss_wrap(&mut minor, self.ctx, c_int::from(encrypt), GSS_C_QOP_DEFAULT, &Buffer::of(data), &mut conf, &mut out) };
        let token = take(&mut out);
        if gss_error(major) {
            return Err(fail("gss_wrap", major, minor));
        }
        if encrypt && conf == 0 {
            return Err("GSS-API wrap without confidentiality".into());
        }
        Ok(token)
    }

    fn unwrap(&mut self, data: &[u8]) -> Result<Vec<u8>, String> {
        let (mut minor, mut conf, mut qop, mut out) = (0, 0, 0, Buffer::EMPTY);
        // SAFETY: как у `wrap`.
        let major = unsafe { gss_unwrap(&mut minor, self.ctx, &Buffer::of(data), &mut out, &mut conf, &mut qop) };
        let plain = take(&mut out);
        if gss_error(major) {
            return Err(fail("gss_unwrap", major, minor));
        }
        Ok(plain)
    }

    fn wrap_limit(&mut self, max_token: u32) -> Result<u32, String> {
        let (mut minor, mut max) = (0, 0);
        // SAFETY: контекст установлен; выход — локальная переменная.
        let major = unsafe { gss_wrap_size_limit(&mut minor, self.ctx, 1, GSS_C_QOP_DEFAULT, max_token, &mut max) };
        if gss_error(major) {
            return Err(fail("gss_wrap_size_limit", major, minor));
        }
        Ok(max)
    }
}
