//! Подменный сервер LDAP (признак `testkit`, S5-12 Ф5): на 127.0.0.1 отвечает сообщениями BER по RFC 4511 — как AD
//! (rootDSE, записи с `userCertificate`, ссылки на другие разделы, предел записей). Им пользуются тесты этого
//! крейта, через обмен (`tests/remote.rs`, ADR 0013) и настоящий процесс набора (`crates/plugins/tests/ldap.rs`,
//! PLG-06); сети вне теста нет.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // тестовый стенд: «не падать» — для кода приложения (S2-03)

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

/// База поиска тестового AD.
pub const BASE: &str = "DC=corp,DC=example";

pub fn tlv(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    let len = body.len();
    if len < 0x80 {
        out.push(len as u8);
    } else {
        let bytes: Vec<u8> = len.to_be_bytes().into_iter().skip_while(|b| *b == 0).collect();
        out.push(0x80 | bytes.len() as u8);
        out.extend(bytes);
    }
    out.extend_from_slice(body);
    out
}

pub fn int(v: u32) -> Vec<u8> {
    let mut b = v.to_be_bytes().into_iter().skip_while(|b| *b == 0).collect::<Vec<_>>();
    if b.first().is_none_or(|f| f & 0x80 != 0) {
        b.insert(0, 0);
    }
    tlv(0x02, &b)
}

pub fn octet(b: &[u8]) -> Vec<u8> {
    tlv(0x04, b)
}

pub fn message(id: u32, op: Vec<u8>) -> Vec<u8> {
    tlv(0x30, &[int(id), op].concat())
}

pub fn result(tag: u8, rc: u8) -> Vec<u8> {
    tlv(tag, &[tlv(0x0a, &[rc]), octet(b""), octet(b"")].concat())
}

/// `SearchResultEntry`: DN и атрибуты.
pub fn entry(dn: &str, attrs: &[(&str, &[&[u8]])]) -> Vec<u8> {
    let attrs: Vec<u8> = attrs
        .iter()
        .flat_map(|(name, vals)| tlv(0x30, &[octet(name.as_bytes()), tlv(0x31, &vals.iter().flat_map(|v| octet(v)).collect::<Vec<u8>>())].concat()))
        .collect();
    tlv(0x64, &[octet(dn.as_bytes()), tlv(0x30, &attrs)].concat())
}

/// Одна TLV из начала `b`: тег, содержимое, остаток.
pub fn take(b: &[u8]) -> (u8, &[u8], &[u8]) {
    let (len, head) = match b[1] {
        l if l < 0x80 => (usize::from(l), 2),
        l => {
            let n = usize::from(l & 0x7f);
            (b[2..2 + n].iter().fold(0usize, |a, x| a << 8 | usize::from(*x)), 2 + n)
        }
    };
    (b[0], &b[head..head + len], &b[head + len..])
}

fn read_message(s: &mut TcpStream) -> Option<Vec<u8>> {
    let mut head = [0u8; 2];
    s.read_exact(&mut head).ok()?;
    let mut msg = head.to_vec();
    let len = if head[1] < 0x80 {
        usize::from(head[1])
    } else {
        let mut n = vec![0u8; usize::from(head[1] & 0x7f)];
        s.read_exact(&mut n).ok()?;
        msg.extend(&n);
        n.iter().fold(0usize, |a, x| a << 8 | usize::from(*x))
    };
    let mut body = vec![0u8; len];
    s.read_exact(&mut body).ok()?;
    msg.extend(body);
    Some(msg)
}

/// Что сервер видел: операции (тег) и базы поиска.
#[derive(Default)]
pub struct Seen {
    pub ops: Vec<u8>,
    pub bases: Vec<String>,
}

/// Подменный сервер: на каждый поиск — ответ `answer(база)` (записи и код итога). Адрес для запроса и что сервер
/// видел.
pub fn server(answer: impl Fn(&str) -> (Vec<Vec<u8>>, u8) + Send + 'static) -> (String, Arc<Mutex<Seen>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let seen = Arc::new(Mutex::new(Seen::default()));
    let log = seen.clone();
    std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        while let Some(msg) = read_message(&mut s) {
            let (_, body, _) = take(&msg);
            let (_, id, rest) = take(body);
            let id = id.iter().fold(0u32, |a, x| a << 8 | u32::from(*x));
            let (op, req, _) = take(rest);
            log.lock().unwrap().ops.push(op);
            let reply = match op {
                0x60 => message(id, result(0x61, 0)),
                0x63 => {
                    let (_, base, _) = take(req);
                    let base = String::from_utf8(base.to_vec()).unwrap();
                    let (entries, rc) = answer(&base);
                    log.lock().unwrap().bases.push(base);
                    let mut out: Vec<u8> = entries.into_iter().flat_map(|e| message(id, e)).collect();
                    out.extend(message(id, result(0x65, rc)));
                    out
                }
                _ => break, // выход (unbind)
            };
            s.write_all(&reply).unwrap();
        }
    });
    (addr, seen)
}

/// Закрытый порт на 127.0.0.1: соединение сразу отказывает.
pub fn closed_port() -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap().to_string();
    drop(l);
    addr
}
