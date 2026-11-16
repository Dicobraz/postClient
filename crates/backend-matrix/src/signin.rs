//! Вход в Matrix (ADR 0012 «Проверка договора»: `Server` → `Login` и `Password` или `Browser`). Сервер — имя
//! (`example.test`, адрес находится по `.well-known/matrix/client`), адрес `https://…` или сразу `@user:server`.
//! Пароль — `m.login.password`; браузер — SSO (`m.login.sso`; на серверах с MAS — его слой совместимости): ссылка
//! возврата — на `127.0.0.1`, слушает ядро и передаёт токен плагину (ADR 0013: песочница входящих не принимает),
//! вход — `m.login.token`.
//!
//! Вход идёт прямым запросом `/login`, без своего хранилища шифрования: устройство и его ключи заводит подключение
//! (`connect`) уже со своим хранилищем в базе профиля — ключи нового устройства не теряются между входом и
//! подключением. Пароль и токен входа нигде не сохраняются; в журнал — только коды отказов.

use std::rc::Rc;

use matrix_sdk::Client;
use matrix_sdk::ruma::api::client::session::get_login_types::v3::LoginType;
use matrix_sdk::ruma::api::client::session::login;
use postman_core::backend::BoxFuture;
use postman_core::channel::{AccountId, Answer, ChannelError, Flow, Prompt, Sso};

use crate::secret::Session;
use crate::{BAD_LOGIN, BAD_PASSWORD, BAD_SERVER, NO_LOGIN_FLOW};

/// Имя устройства на сервере (видно в списке сеансов учётки).
const DEVICE_NAME: &str = "Postman";

/// Что сервер позволяет.
#[derive(Clone, Copy)]
struct Ways {
    password: bool,
    sso: bool,
}

enum Step {
    Start,
    Server,
    Login { client: Client, ways: Ways, user: Option<String> },
    Password { client: Client, ways: Ways, user: String },
    /// Порт возврата слушает ядро ([`Sso`]); повтор входа ждёт тот же порт.
    Browser { client: Client, ways: Ways, port: u16, user: Option<String> },
    Over,
}

pub struct SignIn {
    step: Step,
    sso: Option<Rc<dyn Sso>>,
}

impl SignIn {
    pub fn new(sso: Option<Rc<dyn Sso>>) -> Self {
        Self { step: Step::Start, sso }
    }
}

/// Ввод сервера: `@user:server` — сервер из id, человек — уже известен.
fn server_of(input: &str) -> Option<(String, Option<String>)> {
    let s = input.trim();
    if s.is_empty() || s.contains(char::is_whitespace) {
        return None;
    }
    if let Some(rest) = s.strip_prefix('@') {
        let (_, server) = rest.split_once(':')?;
        return (!server.is_empty()).then(|| (server.to_owned(), Some(s.to_owned())));
    }
    Some((s.trim_end_matches('/').to_owned(), None))
}

async fn ways(client: &Client) -> Result<Ways, ChannelError> {
    let types = client.matrix_auth().get_login_types().await.map_err(|e| crate::client::http(&e))?;
    let mut w = Ways { password: false, sso: false };
    for t in types.flows {
        match t {
            LoginType::Password(_) => w.password = true,
            LoginType::Sso(_) => w.sso = true,
            _ => {}
        }
    }
    Ok(w)
}

/// Отказ сервера при входе: неверные данные — код, остальное — как есть.
fn refused(e: matrix_sdk::HttpError, bad: &str) -> ChannelError {
    match crate::client::http(&e) {
        ChannelError::Denied(_) | ChannelError::NotFound => ChannelError::Denied(bad.into()),
        other => other,
    }
}

/// Готовая сессия после `/login`.
fn done(client: &Client, r: login::v3::Response) -> Result<Prompt, ChannelError> {
    let homeserver = client.homeserver().to_string();
    let user = r.user_id.to_string();
    let name = r.user_id.localpart().to_owned();
    let s = Session::new(homeserver, user.clone(), r.device_id.to_string(), r.access_token, r.refresh_token).map_err(ChannelError::Storage)?;
    tracing::info!("matrix: signed in, new device");
    Ok(Prompt::Done { account: AccountId::new(user), name, secret: s.to_secret() })
}

fn identifier(user: &str) -> Result<matrix_sdk::ruma::api::client::uiaa::UserIdentifier, ChannelError> {
    serde_json::from_value(serde_json::json!({ "type": "m.id.user", "user": user })).map_err(|e| ChannelError::Protocol(e.to_string()))
}

async fn login(client: &Client, info: login::v3::LoginInfo, bad: &str) -> Result<Prompt, ChannelError> {
    let mut req = login::v3::Request::new(info);
    req.initial_device_display_name = Some(DEVICE_NAME.into());
    let r = client.send(req).await.map_err(|e| refused(e, bad))?;
    done(client, r)
}

/// Начать вход через браузер: порт возврата — у ядра, ссылка SSO ведёт туда.
async fn browser(client: &Client, sso: &Rc<dyn Sso>) -> Result<(u16, String), ChannelError> {
    let port = sso.port()?;
    let redirect = format!("http://127.0.0.1:{port}/");
    let url = client.matrix_auth().get_sso_login_url(&redirect, None).await.map_err(crate::client::err)?;
    Ok((port, url))
}

impl SignIn {
    async fn next(&mut self, answer: Option<Answer>) -> Result<Prompt, ChannelError> {
        let step = std::mem::replace(&mut self.step, Step::Over);
        match (step, answer) {
            (Step::Start, _) => {
                self.step = Step::Server;
                Ok(Prompt::Server)
            }
            (Step::Server, Some(Answer::Text(input))) => {
                self.step = Step::Server;
                let (server, user) = server_of(&input).ok_or_else(|| ChannelError::Denied(BAD_SERVER.into()))?;
                let client = crate::client::build(&server, true, None).await?;
                let ways = ways(&client).await.map_err(|e| match e {
                    ChannelError::Network(_) => e,
                    _ => ChannelError::Denied(BAD_SERVER.into()),
                })?;
                if !ways.password && !ways.sso {
                    return Err(ChannelError::Denied(NO_LOGIN_FLOW.into()));
                }
                if !ways.password {
                    let sso = self.sso().cloned()?;
                    let (port, url) = browser(&client, &sso).await?;
                    self.step = Step::Browser { client, ways, port, user };
                    return Ok(Prompt::Browser { url });
                }
                match user {
                    Some(user) => {
                        self.step = Step::Password { client, ways, user };
                        Ok(Prompt::Password { hint: None })
                    }
                    None => {
                        self.step = Step::Login { client, ways, user: None };
                        Ok(Prompt::Login)
                    }
                }
            }
            (Step::Login { client, ways, user }, Some(Answer::Confirm(true))) if ways.sso => {
                let sso = self.sso().cloned()?;
                let (port, url) = match browser(&client, &sso).await {
                    Ok(x) => x,
                    Err(e) => {
                        self.step = Step::Login { client, ways, user };
                        return Err(e);
                    }
                };
                self.step = Step::Browser { client, ways, port, user };
                Ok(Prompt::Browser { url })
            }
            (Step::Login { client, ways, .. }, Some(Answer::Text(user))) => {
                let user = user.trim().to_owned();
                if user.is_empty() {
                    self.step = Step::Login { client, ways, user: None };
                    return Err(ChannelError::Denied(BAD_LOGIN.into()));
                }
                self.step = Step::Password { client, ways, user };
                Ok(Prompt::Password { hint: None })
            }
            (Step::Password { client, ways, user }, Some(Answer::Secret(pw))) => {
                let password = String::from_utf8_lossy(pw.expose()).into_owned();
                self.password(client, ways, user, password).await
            }
            (Step::Password { client, ways, user }, Some(Answer::Text(password))) => self.password(client, ways, user, password).await,
            // Пароль — другим человеком: снова вопрос имени.
            (Step::Password { client, ways, .. }, Some(Answer::Confirm(false))) => {
                self.step = Step::Login { client, ways, user: None };
                Ok(Prompt::Login)
            }
            (Step::Browser { client, ways, port, user }, Some(Answer::Confirm(true))) => {
                let sso = self.sso().cloned()?;
                let token = match sso.token().await {
                    Ok(t) => t,
                    Err(e) => {
                        self.step = Step::Browser { client, ways, port, user };
                        return Err(e);
                    }
                };
                match login(&client, login::v3::LoginInfo::Token(login::v3::Token::new(token)), BAD_LOGIN).await {
                    Ok(p) => Ok(p),
                    Err(e) => {
                        self.step = Step::Browser { client, ways, port, user };
                        Err(e)
                    }
                }
            }
            // Передумал: обратно к имени и паролю (если сервер их даёт).
            (Step::Browser { client, ways, user, .. }, Some(Answer::Confirm(false))) if ways.password => {
                self.step = Step::Login { client, ways, user };
                Ok(Prompt::Login)
            }
            (step @ Step::Over, _) => {
                self.step = step;
                Ok(Prompt::Finished)
            }
            (step, other) => {
                self.step = step;
                Err(ChannelError::Protocol(format!("unexpected answer {}", answer_kind(other.as_ref()))))
            }
        }
    }

    /// Услуга возврата SSO у ядра; нет её (вход в процессе ядра без обмена) — через браузер нельзя.
    fn sso(&self) -> Result<&Rc<dyn Sso>, ChannelError> {
        self.sso.as_ref().ok_or_else(|| ChannelError::Plugin("sso service is not available".into()))
    }

    async fn password(&mut self, client: Client, ways: Ways, user: String, password: String) -> Result<Prompt, ChannelError> {
        let info = login::v3::LoginInfo::Password(login::v3::Password::new(identifier(&user)?, password));
        match login(&client, info, BAD_PASSWORD).await {
            Ok(p) => Ok(p),
            Err(e) => {
                self.step = Step::Password { client, ways, user };
                Err(e)
            }
        }
    }
}

/// Вид ответа для журнала — без значения.
fn answer_kind(a: Option<&Answer>) -> &'static str {
    match a {
        None => "none",
        Some(Answer::Text(_)) => "text",
        Some(Answer::Secret(_)) => "secret",
        Some(Answer::Confirm(_)) => "confirm",
        Some(Answer::Resend) => "resend",
    }
}

impl Flow for SignIn {
    fn step<'a>(&'a mut self, answer: Option<Answer>) -> BoxFuture<'a, Result<Prompt, ChannelError>> {
        Box::pin(self.next(answer))
    }
}

#[cfg(test)]
mod tests {
    use super::server_of;

    #[test]
    fn server_input() {
        assert_eq!(server_of("example.test"), Some(("example.test".into(), None)));
        assert_eq!(server_of("https://matrix.example.test/"), Some(("https://matrix.example.test".into(), None)));
        assert_eq!(server_of("@anna:example.test"), Some(("example.test".into(), Some("@anna:example.test".into()))));
        assert_eq!(server_of("@anna"), None);
        assert_eq!(server_of(" "), None);
    }
}
