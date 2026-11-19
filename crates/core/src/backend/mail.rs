//! Услуги ядра почтовому пути и путь-процесс (PLG-08, ADR 0013): база открыта только у ядра (ADR 0006), воркер и
//! ключи S/MIME — тоже у ядра. Путь-процесс получает их услугами по обмену:
//!
//! - **Хранилище учётки** ([`MailCache`]): методы кэша из описи PLG-08 — операциями [`CacheOp`]. Записи — без ответа
//!   ([`FromPlugin::CacheWrite`]), идут подряд, ошибка — на ближайшем вызове с ответом, порядок сохраняется; чтения —
//!   вызовом с ответом ([`ToPlugin::Cached`]). У ядра — исполнитель [`run_cache`], у пути — [`RemoteCache`]; в потоке
//!   ядра (до своей волны) и в тестах — [`LocalCache`] над настоящим кэшем.
//! - **Разбор письма** ([`MailWorker`]): воркер-песочница запускает ядро, не плагин — плагин процессов не запускает
//!   (`postman-worker.exe` рядом с постманом). [`LocalWorker`] — настоящий воркер, [`RemoteWorker`] — по обмену.
//! - **S/MIME исходящего** ([`MailSeal`]): ключи пользователя плагину недоступны — шифрование и подпись делает ядро
//!   ([`LocalSeal`], процессу — [`WireSeal`]).
//!
//! Путь-процесс — [`MailServer`] ([`serve_mail_area`]) и его заместитель — [`RemoteMail`] ([`MailPlugin`], тот же
//! трейт, что у пути в потоке ядра). Тексты ошибок обмена — по-английски, как у всего договора.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::Duration;

use postman_cache::{
    AccountRow, BodyRow, Cache, EmailRow, FolderId, FolderRow, Hash, LetterId, LetterRow, MailboxRow, RemoteRef, Result as CacheResult, SplitPart,
};
use postman_compose::Draft;
use postman_worker::mime::Mime as ParsedMime;
use tokio::sync::Notify;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

use super::remote::{MailScope, PROBE_WAIT, PluginLink, STORE_WAIT, lock, ready_async};
use super::wire::{CacheDone, CacheOp, FromPlugin, MailLogin, SmimeOp, ToPlugin, WorkerDone, WorkerOp};
use super::{BoxFuture, MailPlugin, Spawn};
use crate::model::Auth;
use crate::sync::{Account, Command, Event, SyncError, UnboundedSender};

/// Сколько ждать воркер на разборе письма (у ядра, как у путей в их потоках).
pub const WORKER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Хранилище учётки для потока синхронизации (PLG-08): методы кэша из описи. Записи у пути-процесса уходят без
/// ответа (ошибка — на ближайшем чтении), у ядра — как были; остальное у обоих — с ответом.
pub trait MailCache: Send {
    fn replace_mailboxes(&self, account: Option<&str>, rows: &[MailboxRow]) -> CacheResult<()>;
    fn replace_accounts_of(&self, profile: Option<&str>, rows: &[AccountRow]) -> CacheResult<()>;
    fn upsert_emails(&self, rows: &[EmailRow]) -> CacheResult<()>;
    fn delete_emails(&self, ids: &[String]) -> CacheResult<()>;
    fn set_state(&self, kind: &str, value: &str) -> CacheResult<()>;
    fn put_body(&self, id: &str, b: &BodyRow) -> CacheResult<()>;
    fn put_blob(&self, blob_id: &str, data: &[u8]) -> CacheResult<()>;
    fn put_content(&self, letter: LetterId, raw: &[u8], split: &[SplitPart]) -> CacheResult<Hash>;
    fn attach_remote(&self, letter: LetterId, link: RemoteRef) -> CacheResult<()>;
    fn upsert_remotes(&self, account: &str, rows: &[(String, LetterRow)]) -> CacheResult<Vec<LetterId>>;
    fn remember_senders(&self, senders: &[crate::sync::Sender]) -> CacheResult<()>;
    fn state(&self, kind: &str) -> CacheResult<Option<String>>;
    fn body(&self, id: &str) -> CacheResult<Option<BodyRow>>;
    fn rows_by_ids(&self, ids: &[String]) -> CacheResult<Vec<EmailRow>>;
    fn remotes(&self, letter: LetterId) -> CacheResult<Vec<RemoteRef>>;
    fn letter_content(&self, letter: LetterId) -> CacheResult<Option<Vec<u8>>>;
    fn count(&self, mailbox_id: &str) -> CacheResult<usize>;
    fn mailboxes(&self) -> CacheResult<Vec<MailboxRow>>;
    fn letter_of(&self, account: &str, id: &str) -> CacheResult<Option<LetterId>>;
    fn blob(&self, blob_id: &str) -> CacheResult<Option<Vec<u8>>>;
    fn folder(&self, id: FolderId) -> CacheResult<Option<FolderRow>>;
    fn page(&self, mailbox_id: &str, offset: usize, limit: usize) -> CacheResult<Vec<EmailRow>>;
    fn folder_of(&self, account: &str, remote: &str) -> CacheResult<Option<FolderId>>;
    fn letter(&self, id: LetterId) -> CacheResult<Option<LetterRow>>;
    fn thread(&self, thread_id: &str) -> CacheResult<Vec<EmailRow>>;
}

/// Разбор письма воркером-песочницей (PLG-08): чужие данные — только в воркере. Воркер запускает ядро
/// ([`LocalWorker`]) или отвечает по обмену ([`RemoteWorker`]) — плагин процессов не запускает.
pub trait MailWorker: Send {
    fn mime(&self, raw: Vec<u8>) -> Result<ParsedMime, String>;
    fn mime_part(&self, raw: Vec<u8>, index: u32) -> Result<Vec<u8>, String>;
    fn mime_split(&self, raw: Vec<u8>) -> Result<(postman_worker::mime::Split, Vec<u8>), String>;
    fn threads(&self, heads: &[Vec<u8>]) -> Result<Vec<postman_worker::mime::Thread>, String>;
}

/// S/MIME исходящего (PLG-08): шифрование и подпись — у ядра, ключи пользователя плагину недоступны.
pub trait MailSeal: Send {
    fn seal_outgoing(&self, raw: Vec<u8>, to: Vec<postman_smime::seal::To>) -> Result<Vec<u8>, String>;
    fn sign_outgoing(&self, raw: Vec<u8>, from: &str, hwnd: Option<isize>, now: i64) -> Result<Vec<u8>, String>;
}

// ---------------------------------------------------------------------------------------------------------------
// Ядро в потоке: настоящий кэш, настоящий воркер, свои ключи. Тот же путь работает так до своей волны и в тестах.

/// Настоящий кэш ([`Cache`]) как [`MailCache`]: поток ядра и тесты путей.
pub struct LocalCache(pub Cache);

impl MailCache for LocalCache {
    fn replace_mailboxes(&self, account: Option<&str>, rows: &[MailboxRow]) -> CacheResult<()> {
        self.0.replace_mailboxes(account, rows)
    }

    fn replace_accounts_of(&self, profile: Option<&str>, rows: &[AccountRow]) -> CacheResult<()> {
        self.0.replace_accounts_of(profile, rows)
    }

    fn upsert_emails(&self, rows: &[EmailRow]) -> CacheResult<()> {
        self.0.upsert_emails(rows)
    }

    fn delete_emails(&self, ids: &[String]) -> CacheResult<()> {
        self.0.delete_emails(ids)
    }

    fn set_state(&self, kind: &str, value: &str) -> CacheResult<()> {
        self.0.set_state(kind, value)
    }

    fn put_body(&self, id: &str, b: &BodyRow) -> CacheResult<()> {
        self.0.put_body(id, b)
    }

    fn put_blob(&self, blob_id: &str, data: &[u8]) -> CacheResult<()> {
        self.0.put_blob(blob_id, data)
    }

    fn put_content(&self, letter: LetterId, raw: &[u8], split: &[SplitPart]) -> CacheResult<Hash> {
        self.0.put_content(letter, raw, split)
    }

    fn attach_remote(&self, letter: LetterId, link: RemoteRef) -> CacheResult<()> {
        self.0.attach_remote(letter, link)
    }

    fn upsert_remotes(&self, account: &str, rows: &[(String, LetterRow)]) -> CacheResult<Vec<LetterId>> {
        self.0.upsert_remotes(account, rows)
    }

    fn remember_senders(&self, senders: &[crate::sync::Sender]) -> CacheResult<()> {
        crate::channel::mail::remember_senders(&self.0, senders);
        Ok(())
    }

    fn state(&self, kind: &str) -> CacheResult<Option<String>> {
        self.0.state(kind)
    }

    fn body(&self, id: &str) -> CacheResult<Option<BodyRow>> {
        self.0.body(id)
    }

    fn rows_by_ids(&self, ids: &[String]) -> CacheResult<Vec<EmailRow>> {
        self.0.rows_by_ids(ids)
    }

    fn remotes(&self, letter: LetterId) -> CacheResult<Vec<RemoteRef>> {
        self.0.remotes(letter)
    }

    fn letter_content(&self, letter: LetterId) -> CacheResult<Option<Vec<u8>>> {
        self.0.letter_content(letter)
    }

    fn count(&self, mailbox_id: &str) -> CacheResult<usize> {
        self.0.count(mailbox_id)
    }

    fn mailboxes(&self) -> CacheResult<Vec<MailboxRow>> {
        self.0.mailboxes()
    }

    fn letter_of(&self, account: &str, id: &str) -> CacheResult<Option<LetterId>> {
        self.0.letter_of(account, id)
    }

    fn blob(&self, blob_id: &str) -> CacheResult<Option<Vec<u8>>> {
        self.0.blob(blob_id)
    }

    fn folder(&self, id: FolderId) -> CacheResult<Option<FolderRow>> {
        self.0.folder(id)
    }

    fn page(&self, mailbox_id: &str, offset: usize, limit: usize) -> CacheResult<Vec<EmailRow>> {
        self.0.page(mailbox_id, offset, limit)
    }

    fn folder_of(&self, account: &str, remote: &str) -> CacheResult<Option<FolderId>> {
        self.0.folder_of(account, remote)
    }

    fn letter(&self, id: LetterId) -> CacheResult<Option<LetterRow>> {
        self.0.letter(id)
    }

    fn thread(&self, thread_id: &str) -> CacheResult<Vec<EmailRow>> {
        self.0.thread(thread_id)
    }
}

/// Настоящий воркер-песочница ([`postman_worker::Client`]) как [`MailWorker`]: блокирует — путь зовёт его из своего
/// потока, ядро ждёт ответа как раньше.
pub struct LocalWorker(Mutex<postman_worker::Client>);

impl LocalWorker {
    pub fn new(exe: std::path::PathBuf, timeout: std::time::Duration) -> Self {
        Self(Mutex::new(postman_worker::Client::new(exe, timeout)))
    }

    fn with<T>(&self, f: impl FnOnce(&mut postman_worker::Client) -> Result<T, String>) -> Result<T, String> {
        f(&mut self.0.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

impl MailWorker for LocalWorker {
    fn mime(&self, raw: Vec<u8>) -> Result<ParsedMime, String> {
        self.with(|w| w.mime(&raw))
    }

    fn mime_part(&self, raw: Vec<u8>, index: u32) -> Result<Vec<u8>, String> {
        self.with(|w| w.mime_part(&raw, index))
    }

    fn mime_split(&self, raw: Vec<u8>) -> Result<(postman_worker::mime::Split, Vec<u8>), String> {
        self.with(|w| w.mime_split(&raw))
    }

    fn threads(&self, heads: &[Vec<u8>]) -> Result<Vec<postman_worker::mime::Thread>, String> {
        self.with(|w| w.threads(heads))
    }
}

/// Свои ключи ядра как [`MailSeal`]: шифрование и подпись исходящего (S5-12) — здесь же, у ядра.
pub struct LocalSeal;

impl MailSeal for LocalSeal {
    fn seal_outgoing(&self, raw: Vec<u8>, to: Vec<postman_smime::seal::To>) -> Result<Vec<u8>, String> {
        crate::smime::seal_outgoing(&raw, &to).map_err(|e| e.to_string())
    }

    fn sign_outgoing(&self, raw: Vec<u8>, from: &str, hwnd: Option<isize>, now: i64) -> Result<Vec<u8>, String> {
        crate::smime::sign_outgoing(&raw, from, hwnd, now).map_err(|e| e.to_string())
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Исполнители услуг на стороне ядра (хост): операции из [`wire`] — в кэш, воркер и S/MIME.

/// Операция хранилища — в кэш (хост). Ошибки — строкой: по обмену они всё равно текстом.
pub fn run_cache(cache: &Cache, op: &CacheOp) -> CacheResult<CacheDone> {
    let local = LocalCache(cache.clone());

    match op {
        CacheOp::ReplaceMailboxes(account, rows) => local.replace_mailboxes(account.as_deref(), rows).map(|_| CacheDone::Done),
        CacheOp::ReplaceAccountsOf(profile, rows) => local.replace_accounts_of(profile.as_deref(), rows).map(|_| CacheDone::Done),
        CacheOp::UpsertEmails(rows) => local.upsert_emails(rows).map(|_| CacheDone::Done),
        CacheOp::DeleteEmails(ids) => local.delete_emails(ids).map(|_| CacheDone::Done),
        CacheOp::SetState(kind, value) => local.set_state(kind, value).map(|_| CacheDone::Done),
        CacheOp::PutBody(id, b) => local.put_body(id, b).map(|_| CacheDone::Done),
        CacheOp::PutBlob(id, data) => local.put_blob(id, data).map(|_| CacheDone::Done),
        CacheOp::PutContent(letter, raw, split) => local.put_content(*letter, raw, split).map(|_| CacheDone::Done),
        CacheOp::AttachRemote(letter, link) => local.attach_remote(*letter, link.clone()).map(|_| CacheDone::Done),
        CacheOp::UpsertRemotes(account, rows) => local.upsert_remotes(account, rows).map(|_| CacheDone::Done),
        CacheOp::RememberSenders(senders) => local.remember_senders(senders).map(|_| CacheDone::Done),
        CacheOp::State(kind) => local.state(kind).map(CacheDone::State),
        CacheOp::Body(id) => local.body(id).map(CacheDone::Body),
        CacheOp::RowsByIds(ids) => local.rows_by_ids(ids).map(CacheDone::Rows),
        CacheOp::Remotes(letter) => local.remotes(*letter).map(CacheDone::Remotes),
        CacheOp::LetterContent(letter) => local.letter_content(*letter).map(CacheDone::Content),
        CacheOp::Count(mailbox) => local.count(mailbox).map(CacheDone::Count),
        CacheOp::Mailboxes => local.mailboxes().map(CacheDone::Mailboxes),
        CacheOp::LetterOf(account, id) => local.letter_of(account, id).map(CacheDone::LetterOf),
        CacheOp::Blob(blob_id) => local.blob(blob_id).map(CacheDone::Blob),
        CacheOp::Folder(id) => local.folder(*id).map(CacheDone::Folder),
        CacheOp::Page(mailbox, offset, limit) => local.page(mailbox, *offset, *limit).map(CacheDone::Rows),
        CacheOp::FolderOf(account, remote) => local.folder_of(account, remote).map(CacheDone::FolderOf),
        CacheOp::Letter(letter) => local.letter(*letter).map(CacheDone::Letter),
        CacheOp::Thread(thread) => local.thread(thread).map(CacheDone::Rows),
    }
}

/// Операция воркера — в настоящий воркер (хост): чужие письма разбирает только он.
pub fn run_worker(worker: &mut postman_worker::Client, op: WorkerOp) -> Result<WorkerDone, String> {
    match op {
        WorkerOp::Mime(raw) => worker.mime(&raw).map(WorkerDone::Mime),
        WorkerOp::Part { raw, index } => worker.mime_part(&raw, index).map(WorkerDone::Bytes),
        WorkerOp::Split(raw) => worker.mime_split(&raw).map(|(split, decoded)| WorkerDone::Split { split, decoded }),
        WorkerOp::Threads(heads) => worker.threads(&heads).map(WorkerDone::Threads),
    }
}

/// Операция S/MIME — своими ключами (хост).
pub fn run_smime(op: SmimeOp) -> Result<Vec<u8>, String> {
    match op {
        SmimeOp::Seal { raw, to } => LocalSeal.seal_outgoing(raw, to),
        SmimeOp::Sign { raw, from, hwnd, now } => LocalSeal.sign_outgoing(raw, &from, hwnd, now),
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Сторона пути-процесса: услуги ядра по обмену.

fn call(link: &PluginLink, msg: &FromPlugin, wait: std::time::Duration) -> Result<(ToPlugin, Vec<u8>), String> {
    link.call(msg, &[], wait).map_err(|e| e.to_string())
}

/// Хранилище учётки по обмену ([`MailCache`] в процессе, PLG-08): записи — без ответа ([`FromPlugin::CacheWrite`]),
/// подряд; чтения — вызовом ([`FromPlugin::Cache`]), ошибка прошлых записей — на ближайшем чтении.
pub struct RemoteCache {
    link: Arc<PluginLink>,
    session: u64,
}

impl RemoteCache {
    pub fn new(link: Arc<PluginLink>, session: u64) -> Self {
        Self { link, session }
    }

    /// Записать без ответа: ошибка связи — ошибкой (ответа не будет и на следующих).
    fn write(&self, op: CacheOp) -> CacheResult<()> {
        self.link.send(&FromPlugin::CacheWrite { session: self.session, op }, &[]).map_err(|e| postman_cache::Error::other(format!("store: {e}")))
    }

    fn read(&self, op: CacheOp) -> CacheResult<CacheDone> {
        match call(&self.link, &FromPlugin::Cache { session: self.session, op }, STORE_WAIT) {
            Ok((ToPlugin::Cached(r), _)) => r.map_err(postman_cache::Error::other),
            Ok(_) => Err(postman_cache::Error::other("store: unexpected reply")),
            Err(e) => Err(postman_cache::Error::other(format!("store: {e}"))),
        }
    }
}

impl MailCache for RemoteCache {
    fn replace_mailboxes(&self, account: Option<&str>, rows: &[MailboxRow]) -> CacheResult<()> {
        self.write(CacheOp::ReplaceMailboxes(account.map(str::to_owned), rows.to_vec()))
    }

    fn replace_accounts_of(&self, profile: Option<&str>, rows: &[AccountRow]) -> CacheResult<()> {
        self.write(CacheOp::ReplaceAccountsOf(profile.map(str::to_owned), rows.to_vec()))
    }

    fn upsert_emails(&self, rows: &[EmailRow]) -> CacheResult<()> {
        self.write(CacheOp::UpsertEmails(rows.to_vec()))
    }

    fn delete_emails(&self, ids: &[String]) -> CacheResult<()> {
        self.write(CacheOp::DeleteEmails(ids.to_vec()))
    }

    fn set_state(&self, kind: &str, value: &str) -> CacheResult<()> {
        self.write(CacheOp::SetState(kind.to_owned(), value.to_owned()))
    }

    fn put_body(&self, id: &str, b: &BodyRow) -> CacheResult<()> {
        self.write(CacheOp::PutBody(id.to_owned(), b.clone()))
    }

    fn put_blob(&self, blob_id: &str, data: &[u8]) -> CacheResult<()> {
        self.write(CacheOp::PutBlob(blob_id.to_owned(), data.to_vec()))
    }

    fn put_content(&self, letter: LetterId, raw: &[u8], split: &[SplitPart]) -> CacheResult<Hash> {
        // Ответ (хэш) путь не использует: запись как остальные — без ответа.
        self.write(CacheOp::PutContent(letter, raw.to_vec(), split.to_vec()))?;
        Ok(Hash([0; 32]))
    }

    fn attach_remote(&self, letter: LetterId, link: RemoteRef) -> CacheResult<()> {
        self.write(CacheOp::AttachRemote(letter, link))
    }

    fn upsert_remotes(&self, account: &str, rows: &[(String, LetterRow)]) -> CacheResult<Vec<LetterId>> {
        // Вернувшиеся связи путь не использует: запись как остальные — без ответа.
        self.write(CacheOp::UpsertRemotes(account.to_owned(), rows.to_vec()))?;
        Ok(Vec::new())
    }

    fn remember_senders(&self, senders: &[crate::sync::Sender]) -> CacheResult<()> {
        self.write(CacheOp::RememberSenders(senders.to_vec()))
    }

    fn state(&self, kind: &str) -> CacheResult<Option<String>> {
        match self.read(CacheOp::State(kind.to_owned()))? {
            CacheDone::State(s) => Ok(s),
            _ => Err(postman_cache::Error::other("store: unexpected reply")),
        }
    }

    fn body(&self, id: &str) -> CacheResult<Option<BodyRow>> {
        match self.read(CacheOp::Body(id.to_owned()))? {
            CacheDone::Body(b) => Ok(b),
            _ => Err(postman_cache::Error::other("store: unexpected reply")),
        }
    }

    fn rows_by_ids(&self, ids: &[String]) -> CacheResult<Vec<EmailRow>> {
        match self.read(CacheOp::RowsByIds(ids.to_vec()))? {
            CacheDone::Rows(rows) => Ok(rows),
            _ => Err(postman_cache::Error::other("store: unexpected reply")),
        }
    }

    fn remotes(&self, letter: LetterId) -> CacheResult<Vec<RemoteRef>> {
        match self.read(CacheOp::Remotes(letter))? {
            CacheDone::Remotes(r) => Ok(r),
            _ => Err(postman_cache::Error::other("store: unexpected reply")),
        }
    }

    fn letter_content(&self, letter: LetterId) -> CacheResult<Option<Vec<u8>>> {
        match self.read(CacheOp::LetterContent(letter))? {
            CacheDone::Content(c) => Ok(c),
            _ => Err(postman_cache::Error::other("store: unexpected reply")),
        }
    }

    fn count(&self, mailbox_id: &str) -> CacheResult<usize> {
        match self.read(CacheOp::Count(mailbox_id.to_owned()))? {
            CacheDone::Count(n) => Ok(n),
            _ => Err(postman_cache::Error::other("store: unexpected reply")),
        }
    }

    fn mailboxes(&self) -> CacheResult<Vec<MailboxRow>> {
        match self.read(CacheOp::Mailboxes)? {
            CacheDone::Mailboxes(rows) => Ok(rows),
            _ => Err(postman_cache::Error::other("store: unexpected reply")),
        }
    }

    fn letter_of(&self, account: &str, id: &str) -> CacheResult<Option<LetterId>> {
        match self.read(CacheOp::LetterOf(account.to_owned(), id.to_owned()))? {
            CacheDone::LetterOf(l) => Ok(l),
            _ => Err(postman_cache::Error::other("store: unexpected reply")),
        }
    }

    fn blob(&self, blob_id: &str) -> CacheResult<Option<Vec<u8>>> {
        match self.read(CacheOp::Blob(blob_id.to_owned()))? {
            CacheDone::Blob(b) => Ok(b),
            _ => Err(postman_cache::Error::other("store: unexpected reply")),
        }
    }

    fn folder(&self, id: FolderId) -> CacheResult<Option<FolderRow>> {
        match self.read(CacheOp::Folder(id))? {
            CacheDone::Folder(f) => Ok(f),
            _ => Err(postman_cache::Error::other("store: unexpected reply")),
        }
    }

    fn page(&self, mailbox_id: &str, offset: usize, limit: usize) -> CacheResult<Vec<EmailRow>> {
        match self.read(CacheOp::Page(mailbox_id.to_owned(), offset, limit))? {
            CacheDone::Rows(rows) => Ok(rows),
            _ => Err(postman_cache::Error::other("store: unexpected reply")),
        }
    }

    fn folder_of(&self, account: &str, remote: &str) -> CacheResult<Option<FolderId>> {
        match self.read(CacheOp::FolderOf(account.to_owned(), remote.to_owned()))? {
            CacheDone::FolderOf(f) => Ok(f),
            _ => Err(postman_cache::Error::other("store: unexpected reply")),
        }
    }

    fn letter(&self, id: LetterId) -> CacheResult<Option<LetterRow>> {
        match self.read(CacheOp::Letter(id))? {
            CacheDone::Letter(l) => Ok(l),
            _ => Err(postman_cache::Error::other("store: unexpected reply")),
        }
    }

    fn thread(&self, thread_id: &str) -> CacheResult<Vec<EmailRow>> {
        match self.read(CacheOp::Thread(thread_id.to_owned()))? {
            CacheDone::Rows(rows) => Ok(rows),
            _ => Err(postman_cache::Error::other("store: unexpected reply")),
        }
    }
}

/// Воркер по обмену ([`MailWorker`] в процессе, PLG-08): разбор делает ядро.
pub struct RemoteWorker {
    link: Arc<PluginLink>,
}

impl RemoteWorker {
    pub fn new(link: Arc<PluginLink>) -> Self {
        Self { link }
    }
}

impl MailWorker for RemoteWorker {
    fn mime(&self, raw: Vec<u8>) -> Result<ParsedMime, String> {
        match call(&self.link, &FromPlugin::Worker { op: WorkerOp::Mime(raw) }, WORKER_TIMEOUT * 2)? {
            (ToPlugin::Worked(Ok(WorkerDone::Mime(m))), _) => Ok(m),
            (ToPlugin::Worked(Err(e)), _) => Err(e),
            _ => Err("worker: unexpected reply".into()),
        }
    }

    fn mime_part(&self, raw: Vec<u8>, index: u32) -> Result<Vec<u8>, String> {
        match call(&self.link, &FromPlugin::Worker { op: WorkerOp::Part { raw, index } }, WORKER_TIMEOUT * 2)? {
            (ToPlugin::Worked(Ok(WorkerDone::Bytes(b))), _) => Ok(b),
            (ToPlugin::Worked(Err(e)), _) => Err(e),
            _ => Err("worker: unexpected reply".into()),
        }
    }

    fn mime_split(&self, raw: Vec<u8>) -> Result<(postman_worker::mime::Split, Vec<u8>), String> {
        match call(&self.link, &FromPlugin::Worker { op: WorkerOp::Split(raw) }, WORKER_TIMEOUT * 2)? {
            (ToPlugin::Worked(Ok(WorkerDone::Split { split, decoded })), _) => Ok((split, decoded)),
            (ToPlugin::Worked(Err(e)), _) => Err(e),
            _ => Err("worker: unexpected reply".into()),
        }
    }

    fn threads(&self, heads: &[Vec<u8>]) -> Result<Vec<postman_worker::mime::Thread>, String> {
        match call(&self.link, &FromPlugin::Worker { op: WorkerOp::Threads(heads.to_vec()) }, WORKER_TIMEOUT * 2)? {
            (ToPlugin::Worked(Ok(WorkerDone::Threads(t))), _) => Ok(t),
            (ToPlugin::Worked(Err(e)), _) => Err(e),
            _ => Err("worker: unexpected reply".into()),
        }
    }
}

/// S/MIME по обмену ([`MailSeal`] в процессе, PLG-08): ключи — у ядра.
pub struct WireSeal {
    link: Arc<PluginLink>,
}

impl WireSeal {
    pub fn new(link: Arc<PluginLink>) -> Self {
        Self { link }
    }
}

impl MailSeal for WireSeal {
    fn seal_outgoing(&self, raw: Vec<u8>, to: Vec<postman_smime::seal::To>) -> Result<Vec<u8>, String> {
        match call(&self.link, &FromPlugin::Smime { op: SmimeOp::Seal { raw, to } }, WORKER_TIMEOUT * 3)? {
            (ToPlugin::Sealed(r), _) => r,
            _ => Err("smime: unexpected reply".into()),
        }
    }

    fn sign_outgoing(&self, raw: Vec<u8>, from: &str, hwnd: Option<isize>, now: i64) -> Result<Vec<u8>, String> {
        match call(&self.link, &FromPlugin::Smime { op: SmimeOp::Sign { raw, from: from.to_owned(), hwnd, now } }, WORKER_TIMEOUT * 3)? {
            (ToPlugin::Sealed(r), _) => r,
            _ => Err("smime: unexpected reply".into()),
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Окно «Отменить» и очередь отправки почты — у ядра (PLG-08): путь получает письмо в момент отправки и подтверждает
// итог. Очередь — в базе профиля: закрыли Postman в окне — уходит при выходе ([`mail_leave`]); упал процесс плагина —
// письмо в очереди, уйдёт при следующем входе сессии; упала программа — при следующем запуске.

/// Префикс ключей очереди в таблице `local` базы профиля: `send.queue<аккаунт>/<id отправки>`.
const QUEUE_PREFIX: &str = "send.queue";

/// Билеты, которые очередь ядра даёт своим отправкам (интерфейсу их не показывают): ответы пути с такими билетами —
/// очереди, интерфейсу их не пересылают.
pub const INTERNAL_TICKET: u64 = 1 << 62;

/// Отложенная отправка в очереди ядра.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MailSend {
    /// Ключ отправки ([`Scheduled::submission`](crate::send_later::Scheduled)) — «Отменить» по нему ищет.
    pub submission: String,
    pub draft: Draft,
    /// Прежняя версия черновика на сервере (удалить после отправки).
    pub replace: Option<String>,
    /// Срок отправки, Unix-секунды.
    pub due: i64,
}

/// Сколько писем держит очередь ядра у всех учёток почтовых путей (выход их дожидается, [`mail_leave`]).
pub fn mail_holding() -> usize {
    MAIL_HOLDING.load(Ordering::Relaxed)
}

/// Выход: письма в окне «Отменить» уходят сейчас, сессии их дожидает ([`MAIL_WAKE`]).
pub fn mail_leave() {
    MAIL_LEAVING.store(true, Ordering::Relaxed);
    MAIL_WAKE.notify_waiters();
}

pub(crate) fn mail_leaving() -> bool {
    MAIL_LEAVING.load(Ordering::Relaxed)
}

static MAIL_HOLDING: AtomicUsize = AtomicUsize::new(0);
static MAIL_LEAVING: AtomicBool = AtomicBool::new(false);
static MAIL_WAKE: LazyLock<Notify> = LazyLock::new(Notify::new);

/// Очередь отправки одной учётки (PLG-08): письмо — в базу профиля, до срока — у ядра; общее с [`MailScope`] сессии.
pub(in crate::backend) struct MailQueue {
    cache: Cache,
    /// Префикс ключей учётки: `send.queue<аккаунт>/`.
    prefix: String,
    entries: Mutex<Vec<MailSend>>,
    /// Выданные пути и не подтверждённые ответом отправки (текущее окно, падение пути — [`MailQueue::postpone`]).
    inflight: AtomicUsize,
    next: AtomicU64,
}

impl MailQueue {
    /// Очередь учётки: несоответствующее в базе — мимо (писали другие сборки).
    pub(in crate::backend) fn new(cache: Cache, scope: &str) -> Self {
        let prefix = format!("{QUEUE_PREFIX}{scope}/");
        let entries = cache
            .local_prefix(&prefix)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(_, doc)| serde_json::from_slice::<MailSend>(&doc.value).ok())
            .collect::<Vec<_>>();
        let queued = entries.len();
        MAIL_HOLDING.fetch_add(queued, Ordering::Relaxed);
        if queued > 0 {
            tracing::info!("mail queue: {queued} held letter(s) from the previous run");
        }
        Self { cache, prefix, entries: Mutex::new(entries), inflight: AtomicUsize::new(0), next: AtomicU64::new(INTERNAL_TICKET + 1) }
    }

    /// Ключ записи в базе.
    fn key(&self, submission: &str) -> String {
        format!("{}{}", self.prefix, submission)
    }

    /// Записать письмо в очередь и в базу. Интерфейсу — «отправлено», с окном «Отменить».
    pub(in crate::backend) fn hold(&self, draft: Draft, replace: Option<String>, secs: u32) -> crate::send_later::Scheduled {
        let submission = format!("{}hold.{}", self.prefix, self.next.fetch_add(1, Ordering::Relaxed));
        let send_at = jiff::Timestamp::now().as_second() + i64::from(secs);
        let send = MailSend { submission: submission.clone(), draft, replace, due: send_at };
        self.persist(&send);
        self.entries.lock().unwrap_or_else(PoisonError::into_inner).push(send);
        MAIL_HOLDING.fetch_add(1, Ordering::Relaxed);
        MAIL_WAKE.notify_waiters();
        crate::send_later::Scheduled { submission, email: String::new(), send_at }
    }

    fn persist(&self, send: &MailSend) {
        let value = serde_json::to_vec(send).unwrap_or_default();
        let put = postman_cache::LocalPut { key: self.key(&send.submission), value, parts: Vec::new() };
        if let Err(e) = self.cache.put_local(&put) {
            tracing::error!("mail queue: the letter is not persisted: {e}");
        }
    }

    fn forget(&self, submission: &str) {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner).retain(|s| s.submission != submission);
        if let Err(e) = self.cache.remove_local(&[&self.key(submission)]) {
            tracing::error!("mail queue: the letter is not removed from the base: {e}");
        }
    }

    /// Что показать интерфейсу («Отправить позже» у Exchange нет — только окно «Отменить»).
    pub(in crate::backend) fn scheduled(&self) -> Vec<crate::send_later::Scheduled> {
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|s| crate::send_later::Scheduled { submission: s.submission.clone(), email: String::new(), send_at: s.due })
            .collect()
    }

    /// Ближайший срок (Unix-секунды); `None` — очередь пуста.
    pub(in crate::backend) fn next_due(&self) -> Option<i64> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner).iter().map(|s| s.due).min()
    }

    /// Отправки, чей срок вышел (`all` — все, интерфейс закрылся): из очереди и из базы, с билетами для ответов
    /// пути.
    pub(in crate::backend) fn release_due(&self, all: bool) -> Vec<(u64, MailSend)> {
        let leaving = mail_leaving();
        let now = jiff::Timestamp::now().as_second();
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        let (due, keep): (Vec<_>, Vec<_>) =
            std::mem::take(&mut *entries).into_iter().partition(|s| all || crate::send_later::release_now(s.due, now, leaving));
        *entries = keep;
        drop(entries);
        self.inflight.fetch_add(due.len(), Ordering::Relaxed);
        due.into_iter()
            .map(|s| {
                self.cache.remove_local(&[&self.key(&s.submission)]).ok();
                MAIL_HOLDING.fetch_sub(1, Ordering::Relaxed);
                (self.next.fetch_add(1, Ordering::Relaxed), s)
            })
            .collect()
    }

    /// Сколько очередь держит: в окне и на отправке у пути (выход дожидается).
    pub(in crate::backend) fn pending(&self) -> usize {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner).len() + self.inflight.load(Ordering::Relaxed)
    }

    /// Вернуть письмо в очередь: путь его не принял (процесс упал) — уйдёт при следующем входе сессии.
    pub(in crate::backend) fn postpone(&self, send: MailSend) {
        tracing::info!("mail queue: the path did not take the letter, it stays queued");
        self.persist(&send);
        self.entries.lock().unwrap_or_else(PoisonError::into_inner).push(send);
        self.inflight.fetch_sub(1, Ordering::Relaxed);
        MAIL_HOLDING.fetch_add(1, Ordering::Relaxed);
        MAIL_WAKE.notify_waiters();
    }

    /// «Отменить» из окна: письмо в очереди — убрано (`true`); уже у пути — нет.
    pub(in crate::backend) fn cancel(&self, submission: &str) -> bool {
        let had = self.entries.lock().unwrap_or_else(PoisonError::into_inner).iter().any(|s| s.submission == submission);
        if had {
            self.forget(submission);
            MAIL_HOLDING.fetch_sub(1, Ordering::Relaxed);
        }
        had
    }

    /// «Отменить» шага поручения: сколько нашлось в очереди.
    pub(in crate::backend) fn cancel_all(&self, submissions: &[String]) -> usize {
        submissions.iter().filter(|s| self.cancel(s)).count()
    }

    /// Ответ пути на свой билет: отправлено — ничего; отказ — ошибка интерфейсу (содержимое письма — не в журнал).
    pub(in crate::backend) fn confirm(&self, event: &crate::sync::Event) -> Option<crate::sync::Event> {
        self.inflight.fetch_sub(1, Ordering::Relaxed);
        match event {
            crate::sync::Event::Sent { .. } => None,
            crate::sync::Event::ComposeFailed { .. } => {
                tracing::error!("mail queue: held letter not sent by the path");
                Some(crate::sync::Event::Error(crate::sync::SyncError::internal("held letter not sent")))
            }
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Путь-процесс и его заместитель.

/// Почтовый путь на стороне процесса (PLG-08): вид сервера и запуск синхронизации учётки. События — `notify`
/// ([`FromPlugin::MailEvent`]), команды — канал `commands`, хранилище/воркер/S/MIME — услуги из [`mail`](self).
pub trait MailServer: Send + Sync {
    /// Есть ли путь по адресу (`MailPlugin::probe`); `Some` — адрес учётной записи для него.
    fn probe<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Option<String>>;

    /// Запустить синхронизацию учётной записи (`MailPlugin::spawn`): свой поток с рантаймом, команды — `commands`,
    /// события — `notify`. Кончается, когда канал команд закрылся (интерфейс закрылся) или синхронизация
    /// остановилась (отказ входа). `Err` — поток не завёлся.
    fn open(
        &self,
        url: String,
        profile: Option<String>,
        login: MailLogin,
        services: MailServices,
        commands: UnboundedReceiver<Command>,
        notify: Arc<dyn Fn(Event) + Send + Sync>,
    ) -> BoxFuture<'_, Result<(), String>>;
}

/// Услуги ядра почтовому пути по обмену: хранилище учётки, разбор письма в воркере, подпись и шифрование S/MIME
/// ([`MailServer::open`]).
pub struct MailServices {
    pub store: Box<dyn MailCache>,
    pub worker: Box<dyn MailWorker>,
    pub seal: Box<dyn MailSeal>,
}

/// Логин и пароль из учётной записи договора ([`Account::auth`]) для [`MailServer::open`]; токенов у путей с окном
/// «Отменить» в ядре нет (EWS — логин и пароль; токены — JMAP, он остаётся в потоке ядра до своей волны).
pub fn mail_login(a: &Account) -> Result<MailLogin, SyncError> {
    match &a.auth {
        Auth::Basic { user, password } => Ok(MailLogin { user: user.clone(), password: crate::channel::Secret::new(password.clone().into_bytes()) }),
        Auth::Bearer(_) => Err(SyncError {
            kind: crate::model::ErrorKind::Auth,
            stage: crate::sync::Stage::Start,
            detail: "the path signs in with a login and password only".to_owned(),
        }),
    }
}

/// Заместитель почтового пути ([`MailPlugin`] для ядра, PLG-08): внутри — процесс набора. Процесс стартует по первому
/// запросу (`spawn`, `probe`); сессия учётки живёт, пока жив канал команд интерфейса. Процесс упал — поток
/// синхронизации входит заново (`MailOpen`), учётка не пересоздаётся.
pub struct RemoteMail {
    host: Arc<super::remote::Inner>,
    plugin: Arc<super::remote::Plugin>,
}

impl super::remote::Host {
    /// Заместитель почтового пути по строке (PLG-08).
    pub fn mail(&self, row: super::remote::Row) -> RemoteMail {
        let plugin = self.0.plugin(row);
        RemoteMail { host: Arc::clone(&self.0), plugin }
    }
}

impl MailPlugin for RemoteMail {
    fn name(&self) -> &'static str {
        self.plugin.row.id
    }

    fn claims(&self, url: &str) -> bool {
        self.plugin.row.claims(url)
    }

    /// Вид сервера по адресу: процесс стартует по запросу — «Добавить учётную запись» поднимает набор.
    fn probe<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Option<String>> {
        Box::pin(async move {
            let conn = ready_async(&self.host, &self.plugin).await.ok()?;
            match conn.ask_sync(super::wire::Ask::Probe(url.to_owned()), PROBE_WAIT) {
                Some(super::wire::Reply::Url(found)) => found,
                _ => None,
            }
        })
    }

    fn set_tls13_only(&self, _on: bool) {
        // Политику процесс получает с `Hello` и сменой ([`super::wire::Settings`]), путь применяет её сам.
    }

    /// Синхронизация учётки — сессия процесса (PLG-08): команды интерфейса — по обмену, события — обратно. Ответ —
    /// канал команд сразу (процесс поднимается в фоне, как поток учётки в ядре).
    fn spawn(&self, s: Spawn) -> UnboundedSender<Command> {
        let login = match mail_login(&s.account) {
            Ok(l) => l,
            Err(e) => {
                let _ = s.events.send(Event::Error(e));
                (s.repaint)();
                let (tx, _rx) = unbounded_channel::<Command>();
                return tx;
            }
        };
        let (tx, mut rx) = unbounded_channel::<Command>();
        let Spawn { account, profile, cache, events, repaint, .. } = s;
        // Пробуждение — одно замыкание на все события сессии: под замком, чтобы делить между входами.
        let repaint: Arc<dyn Fn() + Send + Sync> = {
            let repaint = Arc::new(Mutex::new(repaint));
            Arc::new(move || (lock(&repaint))())
        };
        let (host, plugin) = (Arc::clone(&self.host), Arc::clone(&self.plugin));
        let spawn_errors = events.clone();
        let spawned = std::thread::Builder::new().name(format!("sync-{}", self.plugin.row.id)).spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = events.send(Event::Error(SyncError::internal(format!("tokio runtime: {e}"))));
                    return;
                }
            };
            rt.block_on(session(&host, &plugin, MailRun { url: account.url, profile, login, cache, events, repaint }, &mut rx));
        });
        if let Err(e) = spawned {
            let _ = tx_err(&spawn_errors, SyncError::internal(format!("sync thread: {e}")));
        }
        tx
    }
}

/// Ошибка в канал учётки с пробуждением интерфейса.
fn tx_err(events: &std::sync::mpsc::Sender<Event>, e: SyncError) -> Result<(), ()> {
    events.send(Event::Error(e)).map_err(|_| ())
}

/// Событие интерфейсу с пробуждением.
fn iface(events: &std::sync::mpsc::Sender<Event>, repaint: &Arc<dyn Fn() + Send + Sync>, event: Event) {
    let _ = events.send(event);
    repaint();
}

/// Что нужно сессии учётки ([`session`]) сверх процесса: учётка и её поток событий.
struct MailRun {
    url: String,
    profile: Option<String>,
    login: MailLogin,
    cache: Cache,
    events: std::sync::mpsc::Sender<Event>,
    repaint: Arc<dyn Fn() + Send + Sync>,
}

/// Сессия учётки в процессе (PLG-08): вход (`MailOpen`), команды до конца канала; окно «Отменить» и очередь
/// отправки — здесь, у ядра ([`MailQueue`]). Процесс упал — вход заново (письмо очереди дождётся новой сессии);
/// синхронизация кончилась (ответ `Unit(Err)`, отказ входа) — конец, как у пути в потоке ядра.
async fn session(host: &Arc<super::remote::Inner>, plugin: &Arc<super::remote::Plugin>, run: MailRun, rx: &mut UnboundedReceiver<Command>) {
    let MailRun { url, profile, login, cache, events, repaint } = run;
    let scope = postman_cache::in_profile(profile.as_deref(), None).unwrap_or_default();
    let queue = Arc::new(MailQueue::new(cache.clone(), &scope));
    loop {
        let conn = match ready_async(host, plugin).await {
            Ok(conn) => conn,
            Err(e) => {
                let _ =
                    tx_err(&events, SyncError { kind: crate::model::ErrorKind::Network, stage: crate::sync::Stage::Connect, detail: e.to_string() });
                repaint();
                return;
            }
        };
        let session = conn.id();
        let wake: Box<dyn Fn() + Send> = {
            let repaint = Arc::clone(&repaint);
            Box::new(move || repaint())
        };
        {
            let mut mails = lock(&conn.mails);
            mails.insert(
                session,
                MailScope {
                    events: events.clone(),
                    repaint: Mutex::new(wake),
                    cache: cache.clone(),
                    failed: Mutex::new(None),
                    queue: Arc::clone(&queue),
                },
            );
        }
        let ask = super::wire::Ask::MailOpen { session, url: url.clone(), profile: profile.clone(), login: login.clone() };
        match conn.ask(ask, &[]).await {
            Ok((super::wire::Reply::MailOpened(Ok(())), _)) => {}
            Ok((super::wire::Reply::MailOpened(Err(e)), _)) => {
                let _ = tx_err(&events, SyncError::internal(e));
                repaint();
                lock(&conn.mails).remove(&session);
                let _ = conn.link.send(&ToPlugin::Close { session }, &[]);
                return;
            }
            Ok(_) => {
                let _ = tx_err(&events, SyncError::internal("plugin: unexpected reply"));
                repaint();
                lock(&conn.mails).remove(&session);
                return;
            }
            // Процесс не донёс (упал под входом): вход заново, как у областей ([`RemoteAreasSession`]).
            Err(_) => continue,
        }
        // Письма прошлой сессии (срок вышел, процесс падал) — в путь сейчас.
        release(&conn, session, &queue, false).await;
        // Команды интерфейса и срок очереди; конец канала — очередь дослать, затем сессии конец.
        loop {
            let now = jiff::Timestamp::now().as_second();
            let due = queue.next_due().map_or(i64::MAX, |d| d.max(now));
            let wait = tokio::time::Instant::now() + Duration::from_secs(u64::try_from(due - now).unwrap_or(0));
            tokio::select! {
                cmd = rx.recv() => {
                    let Some(cmd) = cmd else {
                        // Интерфейс закрылся: письма в окне «Отменить» — отправить сейчас, а не потерять.
                        release(&conn, session, &queue, true).await;
                        break;
                    };
                    if !command(&conn, session, &queue, &events, &repaint, cmd).await {
                        break;
                    }
                }
                _ = tokio::time::sleep_until(wait) => release(&conn, session, &queue, false).await,
            }
        }
        lock(&conn.mails).remove(&session);
        let _ = conn.link.send(&ToPlugin::Close { session }, &[]);
        // Ответы пути на досланное — очереди; подождать их, пока сессия ещё принимает события.
        for _ in 0..200 {
            if queue.pending() == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        // Сессия кончилась (интерфейс закрылся или путь завершился) — поток тоже.
        return;
    }
}

/// Команда интерфейса: отправку с окном «Отменить» и отмены держит очередь ядра, остальное — в путь.
/// `false` — синхронизация кончилась.
async fn command(
    conn: &Arc<super::remote::Conn>,
    session: u64,
    queue: &Arc<MailQueue>,
    events: &std::sync::mpsc::Sender<Event>,
    repaint: &Arc<dyn Fn() + Send + Sync>,
    cmd: Command,
) -> bool {
    let reply = |event: Event| iface(events, repaint, event);
    match cmd {
        Command::Send { draft, replace, ticket, release: crate::model::Release::Now } => {
            forward(conn, session, Command::Send { draft, replace, ticket, release: crate::model::Release::Now }).await
        }
        Command::Send { draft, replace, ticket, release: crate::model::Release::After(secs) } => {
            let scheduled = queue.hold(*draft, replace, secs);
            reply(Event::Sent { ticket, scheduled: Some(scheduled) });
            true
        }
        Command::Send { draft, replace, ticket, release: crate::model::Release::At(at) } => {
            let secs = u32::try_from(at.saturating_sub(jiff::Timestamp::now().as_second())).unwrap_or(u32::MAX);
            let scheduled = queue.hold(*draft, replace, secs);
            reply(Event::Sent { ticket, scheduled: Some(scheduled) });
            true
        }
        Command::Unsend { scheduled, ticket } => {
            if queue.cancel(&scheduled.submission) {
                reply(Event::Unsent { ticket, email: scheduled.email });
                true
            } else {
                forward(conn, session, Command::Unsend { scheduled, ticket }).await
            }
        }
        Command::Withhold { scheduled, ticket } => {
            let found = queue.cancel_all(&scheduled.iter().map(|s| s.submission.clone()).collect::<Vec<_>>());
            if found == 0 {
                forward(conn, session, Command::Withhold { scheduled, ticket }).await
            } else {
                reply(Event::Withheld { ticket, too_late: found < scheduled.len(), error: None });
                true
            }
        }
        Command::Scheduled => {
            reply(Event::Scheduled { list: queue.scheduled(), max_delay: 0 });
            true
        }
        cmd => forward(conn, session, cmd).await,
    }
}

/// Команда — в путь; `false` — синхронизация кончилась (ответ пути) или связи нет (вход заново).
async fn forward(conn: &Arc<super::remote::Conn>, session: u64, cmd: Command) -> bool {
    match conn.ask(super::wire::Ask::MailCommand { session, cmd: Box::new(cmd) }, &[]).await {
        Ok((super::wire::Reply::Unit(Ok(())), _)) => true,
        Ok((super::wire::Reply::Unit(Err(_)), _)) => false,
        Ok(_) => true,
        Err(_) => false,
    }
}

/// Письма очереди с вышедшим сроком (`all` — все, интерфейс закрылся) — в путь своими билетами; путь не принял —
/// обратно в очередь, уйдёт при следующем входе сессии.
async fn release(conn: &Arc<super::remote::Conn>, session: u64, queue: &Arc<MailQueue>, all: bool) {
    for (ticket, send) in queue.release_due(all) {
        let again = MailSend { submission: send.submission.clone(), draft: send.draft.clone(), replace: send.replace.clone(), due: send.due };
        let cmd = Command::Send { draft: Box::new(send.draft), replace: send.replace, ticket, release: crate::model::Release::Now };
        if !forward(conn, session, cmd).await {
            queue.postpone(again);
            return;
        }
    }
}
