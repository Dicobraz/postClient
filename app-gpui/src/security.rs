//! «Настройки → Безопасность» (S5-30): что действует — выбор пользователя или политика организации. Выбор — только
//! строже прежнего (решение владельца 2026-10-22): картинки «никогда» или «спрашивать», TLS 1.2 или 1.3, пароль —
//! в Диспетчере учётных данных или не хранить; проверка отзыва — всегда. Политика — `Software\Policies\Pets\Postman`
//! (HKLM, затем HKCU) или профиль конфигурации macOS; ключи и типы — одни для всех ОС (POL-01,
//! `docs/tasks/POL-01.md`): `ExternalImages` = `never` | `ask`, `MinTls` = `1.2` | `1.3` (строка; macOS — и число),
//! `StorePasswords`, `AllowUpdate`, `AllowChannelTelegram`, `AllowChannelMatrix` = `0` | `1` (строка, `REG_DWORD`,
//! логическое), `ChannelE2EStorePlaintext` = `2` | `1` | `0` (шифрованные беседы: хранить и искать / хранить, не
//! искать / только в памяти; число или строка) — закрепляет значение, в настройках
//! оно недоступно. Неверный тип или значение — WARN в журнал, не закреплено. Linux (LIN-03) — файл
//! `/etc/postman/policies.json` (JSON-объект, те же имена и типы; `sys::shell::POLICY_FILE`); там же
//! `SandboxNamespaces` = `optional` | `required` ([`sandbox_namespaces`]).

use std::sync::OnceLock;

use postman_core::prefs::{Images, Prefs};

use crate::sys::PolicyValue;

/// Закреплённое политикой.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Policy {
    pub(crate) images: Option<Images>,
    pub(crate) tls13: Option<bool>,
    pub(crate) store_password: Option<bool>,
    /// `AllowChannelTelegram` (ADR 0012 п. 11): `0` — канала нет, `1` — включён без выбора.
    pub(crate) telegram: Option<bool>,
    /// `AllowChannelMatrix` (ADR 0012 п. 11, CH-05): как `AllowChannelTelegram`.
    pub(crate) matrix: Option<bool>,
    /// `ChannelE2EStorePlaintext` (ADR 0012, решение п. 1): выбор «Шифрованные беседы» за пользователя.
    pub(crate) encrypted: Option<postman_core::channel::e2e::E2eStore>,
}

impl Policy {
    /// Политика из значений `read(имя)`; непонятное — не закреплено, WARN.
    pub(crate) fn read_with(read: impl Fn(&str) -> Option<PolicyValue>) -> Policy {
        Policy {
            images: policy_images("ExternalImages", read("ExternalImages")),
            tls13: policy_tls13("MinTls", read("MinTls")),
            store_password: policy_flag("StorePasswords", read("StorePasswords")),
            telegram: policy_flag("AllowChannelTelegram", read("AllowChannelTelegram")),
            matrix: policy_flag("AllowChannelMatrix", read("AllowChannelMatrix")),
            encrypted: policy_e2e("ChannelE2EStorePlaintext", read("ChannelE2EStorePlaintext")),
        }
    }
}

/// Шифрованные беседы: `2` | `1` | `0` числом (`REG_DWORD`, `<integer>`) или строкой.
pub(crate) fn policy_e2e(name: &str, value: Option<PolicyValue>) -> Option<postman_core::channel::e2e::E2eStore> {
    use postman_core::channel::e2e::E2eStore;
    let value = value?;
    let code = match &value {
        PolicyValue::Int(n) => Some(*n),
        PolicyValue::Text(t) => t.trim().parse().ok(),
        _ => None,
    };
    match code.and_then(E2eStore::from_policy) {
        Some(m) => Some(m),
        None => rejected(name, &value),
    }
}

/// Значение не применяется: в журнал, без молчания.
fn rejected<T>(name: &str, value: &PolicyValue) -> Option<T> {
    tracing::warn!(policy = name, value = ?value, "policy: value of unexpected type or range, not applied");
    None
}

/// Выключатель: `0`/`1` числом (`REG_DWORD`, `<integer>`), логическое (`<true/>`) или строкой (`0`, `1`, `false`,
/// `true`, `off`, `on`, `no`, `yes`).
pub(crate) fn policy_flag(name: &str, value: Option<PolicyValue>) -> Option<bool> {
    let value = value?;
    match &value {
        PolicyValue::Int(0) => Some(false),
        PolicyValue::Int(1) => Some(true),
        PolicyValue::Bool(b) => Some(*b),
        PolicyValue::Text(t) => {
            let t = t.trim();
            let is = |words: [&str; 4]| words.iter().any(|w| t.eq_ignore_ascii_case(w));
            if is(["0", "false", "off", "no"]) {
                Some(false)
            } else if is(["1", "true", "on", "yes"]) {
                Some(true)
            } else {
                rejected(name, &value)
            }
        }
        _ => rejected(name, &value),
    }
}

/// Минимальный TLS: `1.2` | `1.3` строкой или дробным числом (`<real>1.3</real>`).
pub(crate) fn policy_tls13(name: &str, value: Option<PolicyValue>) -> Option<bool> {
    let value = value?;
    let near = |x: f64, y: f64| (x - y).abs() < 1e-6;
    match &value {
        PolicyValue::Text(t) if t.trim() == "1.3" => Some(true),
        PolicyValue::Text(t) if t.trim() == "1.2" => Some(false),
        PolicyValue::Real(x) if near(*x, 1.3) => Some(true),
        PolicyValue::Real(x) if near(*x, 1.2) => Some(false),
        _ => rejected(name, &value),
    }
}

/// Внешние картинки: `never` | `ask` строкой.
pub(crate) fn policy_images(name: &str, value: Option<PolicyValue>) -> Option<Images> {
    let value = value?;
    match &value {
        PolicyValue::Text(t) if t.trim().eq_ignore_ascii_case("never") => Some(Images::Never),
        PolicyValue::Text(t) if t.trim().eq_ignore_ascii_case("ask") => Some(Images::Ask),
        _ => rejected(name, &value),
    }
}

/// Политика — читается один раз (реестр, микросекунды).
pub(crate) fn policy() -> Policy {
    static POLICY: OnceLock<Policy> = OnceLock::new();
    *POLICY.get_or_init(|| Policy::read_with(crate::sys::shell::policy))
}

/// Что действует: политика сильнее выбора.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Effective {
    pub(crate) images: Images,
    pub(crate) tls13: bool,
    pub(crate) store_password: bool,
}

pub(crate) fn effective_with(p: &Prefs, policy: Policy) -> Effective {
    Effective {
        images: policy.images.unwrap_or(p.security.images),
        tls13: policy.tls13.unwrap_or(p.security.tls13),
        store_password: policy.store_password.unwrap_or(p.security.store_password),
    }
}

pub(crate) fn effective(p: &Prefs) -> Effective {
    effective_with(p, policy())
}

/// Минимальная версия TLS — во все пути к серверам (новые подключения): слинкованным плагинам — реестром,
/// процессам наборов — настройками хоста (`ToPlugin::Settings`, процессы читают их на входе).
pub(crate) fn apply(p: &Prefs) {
    let tls13 = effective(p).tls13;
    crate::plugins::registry().set_tls13_only(tls13);
    let mut settings = crate::plugins::host().settings();
    settings.tls13_only = tls13;
    crate::plugins::host().set_settings(settings);
}

/// Политика `SandboxNamespaces` (LIN-03, ADR 0010): `required` — воркер и хост модулей без user/net/ipc namespaces не
/// работают (письмо — текстом), `optional` или нет политики — по умолчанию. Нераспознанное — WARN и строже
/// (`required`, ADR 0011 п. 5). Действует на Linux; на других ОС namespaces нет.
#[cfg_attr(not(target_os = "linux"), allow(dead_code, reason = "namespaces are Linux only; the parser is tested on every OS"))]
pub(crate) fn sandbox_namespaces(value: Option<PolicyValue>) -> postman_worker::Namespaces {
    use postman_worker::Namespaces;
    match &value {
        None => Namespaces::Optional,
        Some(PolicyValue::Text(v)) if v.trim().eq_ignore_ascii_case("optional") => Namespaces::Optional,
        Some(PolicyValue::Text(v)) if v.trim().eq_ignore_ascii_case("required") => Namespaces::Required,
        Some(v) => {
            tracing::warn!(policy = "SandboxNamespaces", value = ?v, "policy: unsupported value, using the stricter `required`");
            Namespaces::Required
        }
    }
}

/// Режим namespaces всех воркеров процесса — из политики (до первого воркера).
#[cfg(target_os = "linux")]
pub(crate) fn apply_sandbox_policy() {
    let mode = sandbox_namespaces(crate::sys::shell::policy("SandboxNamespaces"));
    if mode == postman_worker::Namespaces::Required {
        tracing::info!("policy: sandbox namespaces are required");
    }
    postman_worker::set_default_namespaces(mode);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values(images: Option<PolicyValue>, tls: Option<PolicyValue>, store: Option<PolicyValue>) -> Policy {
        Policy::read_with(|name| match name {
            "ExternalImages" => images.clone(),
            "MinTls" => tls.clone(),
            "StorePasswords" => store.clone(),
            _ => None,
        })
    }

    fn text(s: &str) -> Option<PolicyValue> {
        Some(PolicyValue::Text(s.to_owned()))
    }

    /// LIN-03: `SandboxNamespaces` — без политики и `optional` — по умолчанию; `required` — обязательны; непонятное —
    /// строже.
    #[test]
    fn sandbox_namespaces_policy() {
        use postman_worker::Namespaces;
        assert_eq!(sandbox_namespaces(None), Namespaces::Optional);
        assert_eq!(sandbox_namespaces(text("Optional")), Namespaces::Optional);
        assert_eq!(sandbox_namespaces(text(" required ")), Namespaces::Required);
        assert_eq!(sandbox_namespaces(Some(PolicyValue::Int(1))), Namespaces::Required);
    }

    /// Политика закрепляет значение; непонятное — не закреплено; без политики — выбор пользователя.
    #[test]
    fn policy_pins_and_choice_follows() {
        let pol = values(text("Ask"), text("1.3"), text("0"));
        assert_eq!(pol, Policy { images: Some(Images::Ask), tls13: Some(true), store_password: Some(false), ..Policy::default() });
        let tg = Policy::read_with(|n| (n == "AllowChannelTelegram").then_some(PolicyValue::Int(0)));
        assert_eq!(tg.telegram, Some(false), "AllowChannelTelegram = 0 — канала нет");
        let mx = Policy::read_with(|n| (n == "AllowChannelMatrix").then_some(PolicyValue::Int(1)));
        assert_eq!((mx.matrix, mx.telegram), (Some(true), None));
        use postman_core::channel::e2e::E2eStore;
        let e2e = |v: PolicyValue| Policy::read_with(move |n| (n == "ChannelE2EStorePlaintext").then(|| v.clone())).encrypted;
        assert_eq!(e2e(PolicyValue::Int(0)), Some(E2eStore::Memory));
        assert_eq!(e2e(PolicyValue::Text(" 1 ".into())), Some(E2eStore::Store));
        assert_eq!(e2e(PolicyValue::Int(2)), Some(E2eStore::Search));
        assert_eq!(e2e(PolicyValue::Int(3)), None, "чужое — не закреплено");
        assert_eq!(e2e(PolicyValue::Bool(true)), None);
        assert_eq!(values(text("always"), text("1.0"), text("maybe")), Policy::default(), "слабее — не бывает");
        assert_eq!(values(None, None, None), Policy::default());
        let mut p = Prefs::default();
        let e = effective_with(&p, Policy::default());
        assert_eq!(e, Effective { images: Images::Ask, tls13: false, store_password: true });
        p.security.tls13 = true;
        p.security.store_password = false;
        let e = effective_with(&p, Policy { tls13: Some(false), ..Policy::default() });
        assert!(!e.tls13, "политика сильнее выбора");
        assert!(!e.store_password);
    }

    /// POL-01: одна семантика для всех типов — `REG_SZ`, `REG_DWORD`, логическое и дробное профиля macOS.
    #[test]
    fn policy_all_types() {
        use PolicyValue::{Bool, Int, Other, Real};
        for (v, want) in [
            (text("0"), Some(false)),
            (text(" 1 "), Some(true)),
            (text("OFF"), Some(false)),
            (text("yes"), Some(true)),
            (Some(Int(0)), Some(false)),
            (Some(Int(1)), Some(true)),
            (Some(Bool(false)), Some(false)),
            (Some(Bool(true)), Some(true)),
            (Some(Int(2)), None),
            (Some(Real(0.0)), None),
            (text("2"), None),
            (Some(Other("REG_BINARY")), None),
            (None, None),
        ] {
            assert_eq!(policy_flag("StorePasswords", v.clone()), want, "{v:?}");
        }
        for (v, want) in [
            (text("1.3"), Some(true)),
            (text("1.2"), Some(false)),
            (Some(Real(1.3)), Some(true)),
            (Some(Real(1.2)), Some(false)),
            (Some(Int(1)), None),
            (Some(Real(1.0)), None),
            (text("1.1"), None),
            (Some(Bool(true)), None),
        ] {
            assert_eq!(policy_tls13("MinTls", v.clone()), want, "{v:?}");
        }
        assert_eq!(policy_images("ExternalImages", text("NEVER")), Some(Images::Never));
        assert_eq!(policy_images("ExternalImages", Some(Int(0))), None);
        let dword = values(None, None, Some(Int(0)));
        assert_eq!(dword.store_password, Some(false), "StorePasswords = DWORD 0 действует");
    }
}
