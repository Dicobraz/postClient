//! Возврат SSO из браузера слушает ядро (ADR 0013 «Договор»): песочница плагина входящих соединений не принимает.
//! Плагин просит порт ([`SsoOp::Listen`]), собирает ссылку возврата и ждёт токен ([`SsoOp::Wait`]); эта страница
//! ответа браузеру — здесь, токен уходит плагину по обмену. Тот же вход работал внутри плагина до выноса в процесс
//! (CH-05); сюда переехал листенер loopback.

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener};
use std::time::{Duration, Instant};

use crate::channel::{ChannelError, matrix};

/// Сколько ждать возврата из браузера.
pub const BROWSER_WAIT: Duration = Duration::from_secs(5 * 60);
/// Как часто спрашивать слушатель в неблокирующем цикле.
const POLL: Duration = Duration::from_millis(50);

/// Страница в браузере после возврата: закрыть её и вернуться в Postman (на двух языках интерфейса).
const BACK_PAGE: &str =
    "<!doctype html><meta charset=utf-8><title>Postman</title><p>Вход выполнен — вернитесь в Postman.</p><p>Signed in — return to Postman.</p>";

/// Слушать `127.0.0.1:0` для возврата браузера; порт — для ссылки возврата плагина.
pub fn listen() -> io::Result<(TcpListener, u16)> {
    let l = TcpListener::bind(("127.0.0.1", 0))?;
    let port = l.local_addr()?.port();
    Ok((l, port))
}

/// Ждать возврата браузера: `GET /?loginToken=…`; прочие запросы (значок сайта) — 404. Блокирует свой поток до токена
/// или [`BROWSER_WAIT`]; слушатель остаётся годен для повтора.
pub fn wait_token(listener: &TcpListener) -> Result<String, ChannelError> {
    listener.set_nonblocking(true).map_err(|e| ChannelError::Storage(format!("loopback: {e}")))?;
    let deadline = Instant::now() + BROWSER_WAIT;
    let mut buf = vec![0u8; 8192];
    loop {
        match listener.accept() {
            Ok((mut sock, _)) => {
                let _ = sock.set_nonblocking(false);
                let _ = sock.set_read_timeout(Some(Duration::from_secs(10)));
                let _ = sock.set_write_timeout(Some(Duration::from_secs(10)));
                let head = match sock.read(&mut buf) {
                    Ok(n) => String::from_utf8_lossy(&buf[..n]).into_owned(),
                    Err(_) => continue,
                };
                let token = head.lines().next().and_then(|line| line.split(' ').nth(1)).and_then(|path| {
                    let query = path.split_once('?')?.1;
                    query.split('&').find_map(|kv| kv.strip_prefix("loginToken=")).and_then(crate::channel::decode_segment)
                });
                let reply = match &token {
                    Some(_) => format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{BACK_PAGE}",
                        BACK_PAGE.len()
                    ),
                    None => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned(),
                };
                let _ = sock.write_all(reply.as_bytes());
                let _ = sock.shutdown(Shutdown::Both);
                if let Some(t) = token.filter(|t| !t.is_empty()) {
                    return Ok(t);
                }
            }
            // Нет соединения пока — не ошибка: цикл с паузой.
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(ChannelError::Storage(format!("loopback: {e}"))),
        }
        if Instant::now() >= deadline {
            return Err(ChannelError::Denied(matrix::BROWSER_TIMEOUT.into()));
        }
        std::thread::sleep(POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpStream;

    /// Браузер возвращается с токеном — страница ответа «Postman», токен у плагина.
    #[test]
    fn browser_return_gives_the_token() {
        let (listener, port) = listen().unwrap();
        let asked = std::thread::spawn(move || {
            let mut sock = TcpStream::connect(("127.0.0.1", port)).unwrap();
            sock.write_all(b"GET /?loginToken=tok%2D1 HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").unwrap();
            let mut page = String::new();
            std::io::Read::read_to_string(&mut sock, &mut page).unwrap();
            page
        });
        let token = wait_token(&listener).unwrap();
        assert_eq!(token, "tok-1");
        let page = asked.join().unwrap();
        assert!(page.starts_with("HTTP/1.1 200") && page.contains("Postman"), "{page}");
    }

    /// Чужой запрос (значок сайта) — 404, ожидание продолжается до следующего возврата.
    #[test]
    fn stray_requests_get_404_and_waiting_continues() {
        let (listener, port) = listen().unwrap();
        let stray = std::thread::spawn(move || {
            let mut sock = TcpStream::connect(("127.0.0.1", port)).unwrap();
            sock.write_all(b"GET /favicon.ico HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").unwrap();
            let mut page = String::new();
            std::io::Read::read_to_string(&mut sock, &mut page).unwrap();
            page
        });
        let back = std::thread::spawn(move || {
            std::thread::sleep(POLL * 3);
            let mut sock = TcpStream::connect(("127.0.0.1", port)).unwrap();
            sock.write_all(b"GET /?loginToken=ok HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").unwrap();
        });
        // Обслуживает и чужой запрос (404), и возврат с токеном; страницы читаются потоками параллельно.
        assert_eq!(wait_token(&listener).unwrap(), "ok");
        assert!(stray.join().unwrap().starts_with("HTTP/1.1 404"));
        back.join().unwrap();
    }
}
