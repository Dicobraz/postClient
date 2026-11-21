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
    AccountRow, BodyRow, Cache, EmailRow, FolderId, FolderRow, Hash, LetterId, LetterRow, MailboxRow, Patch, Recipients, RemoteRef,
    Result as CacheResult, SplitPart, TaskRow,
};
use postman_compose::Draft;
use postman_worker::mime::Mime as ParsedMime;
use tokio::sync::Notify;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

use super::remote::{MailScope, PROBE_WAIT, PluginLink, STORE_WAIT, lock, ready_async};
use super::wire::{CacheDone, CacheOp, FromPlugin, IndexOp, MailLogin, SmimeOp, ToPlugin, WorkerDone, WorkerOp};
use super::{BoxFuture, MailPlugin, Spawn};
use crate::model::Auth;
use crate::sync::{Account, Command, Event, SyncError, UnboundedSender};

/// Сколько ждать воркер на разборе письма (у ядра, как у путей в их потоках).
pub const WORKER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Хранилище учётки для потока синхронизации (PLG-08): методы кэша из описи. Записи у пути-процесса уходят без
/// ответа (ошибка — на ближайшем чтении), у ядра — как были; остальное у обоих — с ответом.
pub trait MailCache: Send + Sync {
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
    fn upsert_folder(&self, row: &FolderRow) -> CacheResult<FolderId>;
    fn patch_emails(&self, patches: &[Patch]) -> CacheResult<Vec<EmailRow>>;
    fn missing_bodies(&self, limit: usize) -> CacheResult<Vec<String>>;
    fn oldest(&self, mailbox_id: &str) -> CacheResult<Option<String>>;
    fn set_keywords(&self, id: &str, add: &[String], remove: &[String]) -> CacheResult<Option<EmailRow>>;
    fn set_recipients(&self, rows: &[(String, Recipients)]) -> CacheResult<()>;
    fn remove_state(&self, kind: &str) -> CacheResult<()>;
    fn tasks(&self) -> CacheResult<Vec<TaskRow>>;
    fn dirty_tasks(&self) -> CacheResult<Vec<TaskRow>>;
    fn task(&self, uid: &str) -> CacheResult<Option<TaskRow>>;
    fn task_uid(&self, server_id: &str) -> CacheResult<Option<String>>;
    fn put_tasks(&self, rows: &[TaskRow]) -> CacheResult<()>;
    fn remove_tasks(&self, uids: &[String]) -> CacheResult<()>;
    fn replace_tasks(&self, rows: &[TaskRow]) -> CacheResult<()>;
    fn xref_set(&self, source: &str, targets: &[String]) -> CacheResult<()>;
    fn xref_drop(&self, sources: &[String]) -> CacheResult<()>;
    fn xref_sources(&self, target: &str) -> CacheResult<Vec<String>>;
    fn xref_targets(&self, source: &str) -> CacheResult<Vec<String>>;
    fn keyword_prefix(&self, prefix: &str, mailbox: &str, limit: usize) -> CacheResult<Vec<EmailRow>>;
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

/// Поисковый индекс (PLG-10): строки писем — в индекс ядра; текст письма и сам индекс (tantivy, файлы) — у ядра,
/// путь только называет строки. Ответа нет: очередью ведёт ядро (`Conn::serve`), ошибки — его журнал.
pub trait MailIndex: Send {
    fn rows(&self, rows: &[EmailRow]);
}

/// Путь без поиска по индексу (PLG-10: IMAP и EWS ищут на сервере): ядру делать нечего.
pub struct NopIndex;

impl MailIndex for NopIndex {
    fn rows(&self, _rows: &[EmailRow]) {}
}

/// Настоящий индекс ядра как [`MailIndex`] (PLG-10): поток ядра и тесты путей. Индекс может отсутствовать
/// (`SearchIndex` не завёлся) — тогда путь пишет, ядро не индексирует.
pub struct LocalIndex {
    index: crate::sync::Index,
    cache: Cache,
}

impl LocalIndex {
    pub fn new(index: crate::sync::Index, cache: Cache) -> Self {
        Self { index, cache }
    }
}

impl MailIndex for LocalIndex {
    fn rows(&self, rows: &[EmailRow]) {
        if let Some(i) = &self.index {
            crate::indexer::rows(i, &self.cache, rows);
        }
    }
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

    fn upsert_folder(&self, row: &FolderRow) -> CacheResult<FolderId> {
        self.0.upsert_folder(row)
    }

    fn patch_emails(&self, patches: &[Patch]) -> CacheResult<Vec<EmailRow>> {
        self.0.patch_emails(patches)
    }

    fn missing_bodies(&self, limit: usize) -> CacheResult<Vec<String>> {
        self.0.missing_bodies(limit)
    }

    fn oldest(&self, mailbox_id: &str) -> CacheResult<Option<String>> {
        self.0.oldest(mailbox_id)
    }

    fn set_keywords(&self, id: &str, add: &[String], remove: &[String]) -> CacheResult<Option<EmailRow>> {
        self.0.set_keywords(id, add, remove)
    }

    fn set_recipients(&self, rows: &[(String, Recipients)]) -> CacheResult<()> {
        self.0.set_recipients(rows)
    }

    fn remove_state(&self, kind: &str) -> CacheResult<()> {
        self.0.remove_state(kind)
    }

    fn tasks(&self) -> CacheResult<Vec<TaskRow>> {
        self.0.tasks()
    }

    fn dirty_tasks(&self) -> CacheResult<Vec<TaskRow>> {
        self.0.dirty_tasks()
    }

    fn task(&self, uid: &str) -> CacheResult<Option<TaskRow>> {
        self.0.task(uid)
    }

    fn task_uid(&self, server_id: &str) -> CacheResult<Option<String>> {
        self.0.task_uid(server_id)
    }

    fn put_tasks(&self, rows: &[TaskRow]) -> CacheResult<()> {
        self.0.put_tasks(rows)
    }

    fn remove_tasks(&self, uids: &[String]) -> CacheResult<()> {
        self.0.remove_tasks(uids)
    }

    fn replace_tasks(&self, rows: &[TaskRow]) -> CacheResult<()> {
        self.0.replace_tasks(rows)
    }

    fn xref_set(&self, source: &str, targets: &[String]) -> CacheResult<()> {
        self.0.xref_set(source, targets)
    }

    fn xref_drop(&self, sources: &[String]) -> CacheResult<()> {
        self.0.xref_drop(sources)
    }

    fn xref_sources(&self, target: &str) -> CacheResult<Vec<String>> {
        self.0.xref_sources(target)
    }

    fn xref_targets(&self, source: &str) -> CacheResult<Vec<String>> {
        self.0.xref_targets(source)
    }

    fn keyword_prefix(&self, prefix: &str, mailbox: &str, limit: usize) -> CacheResult<Vec<EmailRow>> {
        self.0.keyword_prefix(prefix, mailbox, limit)
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
        CacheOp::UpsertFolder(row) => local.upsert_folder(row).map(CacheDone::FolderUpserted),
        CacheOp::PatchEmails(patches) => local.patch_emails(patches).map(CacheDone::Emails),
        CacheOp::MissingBodies(limit) => local.missing_bodies(*limit).map(CacheDone::Ids),
        CacheOp::Oldest(mailbox) => local.oldest(mailbox).map(CacheDone::Oldest),
        CacheOp::SetKeywords(id, add, remove) => local.set_keywords(id, add, remove).map(CacheDone::Email),
        CacheOp::SetRecipients(rows) => local.set_recipients(rows).map(|_| CacheDone::Done),
        CacheOp::RemoveState(kind) => local.remove_state(kind).map(|_| CacheDone::Done),
        CacheOp::Tasks => local.tasks().map(CacheDone::Tasks),
        CacheOp::DirtyTasks => local.dirty_tasks().map(CacheDone::Tasks),
        CacheOp::Task(uid) => local.task(uid).map(CacheDone::Task),
        CacheOp::TaskUid(server_id) => local.task_uid(server_id).map(CacheDone::TaskUid),
        CacheOp::PutTasks(rows) => local.put_tasks(rows).map(|_| CacheDone::Done),
        CacheOp::RemoveTasks(uids) => local.remove_tasks(uids).map(|_| CacheDone::Done),
        CacheOp::ReplaceTasks(rows) => local.replace_tasks(rows).map(|_| CacheDone::Done),
        CacheOp::XrefSet(source, targets) => local.xref_set(source, targets).map(|_| CacheDone::Done),
        CacheOp::XrefDrop(sources) => local.xref_drop(sources).map(|_| CacheDone::Done),
        CacheOp::XrefSources(target) => local.xref_sources(target).map(CacheDone::Ids),
        CacheOp::XrefTargets(source) => local.xref_targets(source).map(CacheDone::Ids),
        CacheOp::KeywordPrefix(prefix, mailbox, limit) => local.keyword_prefix(prefix, mailbox, *limit).map(CacheDone::Rows),
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

    fn upsert_folder(&self, row: &FolderRow) -> CacheResult<FolderId> {
        // Ключ папки нужен ответом (RENAME у IMAP перелицовывает ключи писем): вызов с ответом.
        match self.read(CacheOp::UpsertFolder(row.clone()))? {
            CacheDone::FolderUpserted(id) => Ok(id),
            _ => Err(postman_cache::Error::other("store: unexpected reply")),
        }
    }

    fn patch_emails(&self, patches: &[Patch]) -> CacheResult<Vec<EmailRow>> {
        match self.read(CacheOp::PatchEmails(patches.to_vec()))? {
            CacheDone::Emails(rows) => Ok(rows),
            _ => Err(postman_cache::Error::other("store: unexpected reply")),
        }
    }

    fn missing_bodies(&self, limit: usize) -> CacheResult<Vec<String>> {
        match self.read(CacheOp::MissingBodies(limit))? {
            CacheDone::Ids(ids) => Ok(ids),
            _ => Err(postman_cache::Error::other("store: unexpected reply")),
        }
    }

    fn oldest(&self, mailbox_id: &str) -> CacheResult<Option<String>> {
        match self.read(CacheOp::Oldest(mailbox_id.to_owned()))? {
            CacheDone::Oldest(id) => Ok(id),
            _ => Err(postman_cache::Error::other("store: unexpected reply")),
        }
    }

    fn set_keywords(&self, id: &str, add: &[String], remove: &[String]) -> CacheResult<Option<EmailRow>> {
        match self.read(CacheOp::SetKeywords(id.to_owned(), add.to_vec(), remove.to_vec()))? {
            CacheDone::Email(row) => Ok(row),
            _ => Err(postman_cache::Error::other("store: unexpected reply")),
        }
    }

    fn set_recipients(&self, rows: &[(String, Recipients)]) -> CacheResult<()> {
        self.write(CacheOp::SetRecipients(rows.to_vec()))
    }

    fn remove_state(&self, kind: &str) -> CacheResult<()> {
        self.write(CacheOp::RemoveState(kind.to_owned()))
    }

    fn tasks(&self) -> CacheResult<Vec<TaskRow>> {
        match self.read(CacheOp::Tasks)? {
            CacheDone::Tasks(rows) => Ok(rows),
            _ => Err(postman_cache::Error::other("store: unexpected reply")),
        }
    }

    fn dirty_tasks(&self) -> CacheResult<Vec<TaskRow>> {
        match self.read(CacheOp::DirtyTasks)? {
            CacheDone::Tasks(rows) => Ok(rows),
            _ => Err(postman_cache::Error::other("store: unexpected reply")),
        }
    }

    fn task(&self, uid: &str) -> CacheResult<Option<TaskRow>> {
        match self.read(CacheOp::Task(uid.to_owned()))? {
            CacheDone::Task(row) => Ok(row),
            _ => Err(postman_cache::Error::other("store: unexpected reply")),
        }
    }

    fn task_uid(&self, server_id: &str) -> CacheResult<Option<String>> {
        match self.read(CacheOp::TaskUid(server_id.to_owned()))? {
            CacheDone::TaskUid(uid) => Ok(uid),
            _ => Err(postman_cache::Error::other("store: unexpected reply")),
        }
    }

    fn put_tasks(&self, rows: &[TaskRow]) -> CacheResult<()> {
        self.write(CacheOp::PutTasks(rows.to_vec()))
    }

    fn remove_tasks(&self, uids: &[String]) -> CacheResult<()> {
        self.write(CacheOp::RemoveTasks(uids.to_vec()))
    }

    fn replace_tasks(&self, rows: &[TaskRow]) -> CacheResult<()> {
        self.write(CacheOp::ReplaceTasks(rows.to_vec()))
    }

    fn xref_set(&self, source: &str, targets: &[String]) -> CacheResult<()> {
        self.write(CacheOp::XrefSet(source.to_owned(), targets.to_vec()))
    }

    fn xref_drop(&self, sources: &[String]) -> CacheResult<()> {
        self.write(CacheOp::XrefDrop(sources.to_vec()))
    }

    fn xref_sources(&self, target: &str) -> CacheResult<Vec<String>> {
        match self.read(CacheOp::XrefSources(target.to_owned()))? {
            CacheDone::Ids(ids) => Ok(ids),
            _ => Err(postman_cache::Error::other("store: unexpected reply")),
        }
    }

    fn xref_targets(&self, source: &str) -> CacheResult<Vec<String>> {
        match self.read(CacheOp::XrefTargets(source.to_owned()))? {
            CacheDone::Ids(ids) => Ok(ids),
            _ => Err(postman_cache::Error::other("store: unexpected reply")),
        }
    }

    fn keyword_prefix(&self, prefix: &str, mailbox: &str, limit: usize) -> CacheResult<Vec<EmailRow>> {
        match self.read(CacheOp::KeywordPrefix(prefix.to_owned(), mailbox.to_owned(), limit))? {
            CacheDone::Rows(rows) => Ok(rows),
            _ => Err(postman_cache::Error::other("store: unexpected reply")),
        }
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

/// Задачи и связи поверх услуги хранилища (PLG-10): [`postman_cache::TaskCache`] для пути в процессе —
/// `tasks::handle` у пути работает с тем же хранилищем по обмену, что и весь путь.
pub struct MailTasks<'a>(pub &'a dyn MailCache);

impl postman_cache::TaskCache for MailTasks<'_> {
    fn tasks(&self) -> postman_cache::Result<Vec<TaskRow>> {
        self.0.tasks()
    }

    fn dirty_tasks(&self) -> postman_cache::Result<Vec<TaskRow>> {
        self.0.dirty_tasks()
    }

    fn task(&self, uid: &str) -> postman_cache::Result<Option<TaskRow>> {
        self.0.task(uid)
    }

    fn task_uid(&self, server_id: &str) -> postman_cache::Result<Option<String>> {
        self.0.task_uid(server_id)
    }

    fn put_tasks(&self, rows: &[TaskRow]) -> postman_cache::Result<()> {
        self.0.put_tasks(rows)
    }

    fn remove_tasks(&self, uids: &[String]) -> postman_cache::Result<()> {
        self.0.remove_tasks(uids)
    }

    fn replace_tasks(&self, rows: &[TaskRow]) -> postman_cache::Result<()> {
        self.0.replace_tasks(rows)
    }

    fn set_state(&self, kind: &str, value: &str) -> postman_cache::Result<()> {
        self.0.set_state(kind, value)
    }

    fn state(&self, kind: &str) -> postman_cache::Result<Option<String>> {
        self.0.state(kind)
    }

    fn xref_set(&self, source: &str, targets: &[String]) -> postman_cache::Result<()> {
        self.0.xref_set(source, targets)
    }

    fn xref_drop(&self, sources: &[String]) -> postman_cache::Result<()> {
        self.0.xref_drop(sources)
    }

    fn xref_sources(&self, target: &str) -> postman_cache::Result<Vec<String>> {
        self.0.xref_sources(target)
    }

    fn xref_targets(&self, source: &str) -> postman_cache::Result<Vec<String>> {
        self.0.xref_targets(source)
    }

    fn keyword_prefix(&self, prefix: &str, mailbox: &str, limit: usize) -> postman_cache::Result<Vec<EmailRow>> {
        self.0.keyword_prefix(prefix, mailbox, limit)
    }

    fn letter(&self, id: LetterId) -> postman_cache::Result<Option<LetterRow>> {
        self.0.letter(id)
    }

    fn rows_by_ids(&self, ids: &[String]) -> postman_cache::Result<Vec<EmailRow>> {
        self.0.rows_by_ids(ids)
    }
}

/// Поисковый индекс по обмену ([`MailIndex`] в процессе, PLG-10): строки — ядру без ответа; текст и индекс — у него.
pub struct RemoteIndex {
    link: Arc<PluginLink>,
    session: u64,
}

impl RemoteIndex {
    pub fn new(link: Arc<PluginLink>, session: u64) -> Self {
        Self { link, session }
    }
}

impl MailIndex for RemoteIndex {
    fn rows(&self, rows: &[EmailRow]) {
        // Связи нет — путь в любом случае кончается; индексация догонит при следующем входе сессии.
        let _ = self.link.send(&FromPlugin::Index { session: self.session, op: IndexOp::Rows(rows.to_vec()) }, &[]);
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

/// Услуги ядра почтовому пути по обмену: хранилище учётки, разбор письма в воркере, подпись и шифрование S/MIME,
/// поисковый индекс ([`MailServer::open`]; индекс зовёт только JMAP, PLG-10, остальным — [`NopIndex`]).
pub struct MailServices {
    pub store: Box<dyn MailCache>,
    pub worker: Box<dyn MailWorker>,
    pub seal: Box<dyn MailSeal>,
    pub index: Box<dyn MailIndex>,
}

/// Вход из учётной записи договора ([`Account::auth`]) для [`MailServer::open`]: логин и пароль (EWS, IMAP) или
/// токен ([`MailLogin::bearer`], JMAP, PLG-10).
pub fn mail_login(a: &Account) -> Result<MailLogin, SyncError> {
    match &a.auth {
        Auth::Basic { user, password } => {
            Ok(MailLogin { user: user.clone(), password: crate::channel::Secret::new(password.clone().into_bytes()), bearer: None })
        }
        Auth::Bearer(token) => Ok(MailLogin {
            user: String::new(),
            password: crate::channel::Secret::new(Vec::new()),
            bearer: Some(crate::channel::Secret::new(token.clone().into_bytes())),
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

    /// Узел адреса учётки у пути со своей схемой (AUD-01, регресс PLG-09): такой путь (IMAP) своих областей не ведёт —
    /// календарь и контакты ищут пути областей на том же узле (RFC 6764). Путь без схемы (JMAP, EWS) ведёт области
    /// сам — искать нечего. Процесса не требует: детект работает и без поднятого набора.
    fn area_host(&self, url: &str) -> Option<String> {
        match self.plugin.row.claim {
            super::remote::Claim::Scheme(_) => host_of(url),
            _ => None,
        }
    }

    /// Адрес области с ключом `key` уже в адресе учётки (`?dav=…` — последнее значение).
    fn has_area(&self, url: &str, key: &str) -> bool {
        super::area_address(url, key).is_some()
    }

    /// Адрес учётки с дописанным адресом области `key`.
    fn with_area(&self, url: &str, key: &str, area: &str) -> String {
        format!("{url}{}{key}={area}", if url.contains('?') { '&' } else { '?' })
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
        let Spawn { account, profile, cache, index, events, repaint, .. } = s;
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
            rt.block_on(session(&host, &plugin, MailRun { url: account.url, profile, login, cache, index, events, repaint }, &mut rx));
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
    /// Поисковый индекс (PLG-10): услуга `FromPlugin::Index` индексирует его; пути без поиска идут мимо.
    index: crate::sync::Index,
    events: std::sync::mpsc::Sender<Event>,
    repaint: Arc<dyn Fn() + Send + Sync>,
}

/// Сессия учётки в процессе (PLG-08): вход (`MailOpen`), команды до конца канала; окно «Отменить» и очередь
/// отправки — здесь, у ядра ([`MailQueue`]). Процесс упал — вход заново (письмо очереди дождётся новой сессии);
/// синхронизация кончилась (ответ `Unit(Err)`, отказ входа) — конец, как у пути в потоке ядра.
async fn session(host: &Arc<super::remote::Inner>, plugin: &Arc<super::remote::Plugin>, run: MailRun, rx: &mut UnboundedReceiver<Command>) {
    let MailRun { url, profile, login, cache, index, events, repaint } = run;
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
                    index: index.clone(),
                    failed: Mutex::new(None),
                    features: Mutex::new(crate::model::Features::NONE),
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
        // Команды интерфейса и срок очереди; конец канала — очередь дослать, затем сессии конец. Новое письмо в
        // очереди и выход ([`mail_leave`]) будят ([`MAIL_WAKE`]): срок пересчитать, окно «Отменить» — уходит сейчас.
        loop {
            let now = jiff::Timestamp::now().as_second();
            let due = queue.next_due().map_or(i64::MAX, |d| d.max(now));
            let wait = tokio::time::Instant::now() + Duration::from_secs(u64::try_from(due - now).unwrap_or(0));
            let wake = MAIL_WAKE.notified();
            tokio::pin!(wake);
            wake.as_mut().enable();
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
                () = &mut wake => release(&conn, session, &queue, false).await,
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

/// Команда интерфейса: отправку и шаг поручения с окном «Отменить» держит очередь ядра — если путь не объявил
/// серверную отложку ([`server_schedule`], AUD-02); остальное — в путь. `false` — синхронизация кончилась.
async fn command(
    conn: &Arc<super::remote::Conn>,
    session: u64,
    queue: &Arc<MailQueue>,
    events: &std::sync::mpsc::Sender<Event>,
    repaint: &Arc<dyn Fn() + Send + Sync>,
    cmd: Command,
) -> bool {
    let reply = |event: Event| iface(events, repaint, event);
    // Фичу пути читаем один раз на команду: сессия у пути одна, `Event::Features` приходит сразу после входа.
    let server_holds = server_schedule(conn, session);
    match cmd {
        Command::Send { draft, replace, ticket, release: crate::model::Release::Now } => {
            forward(conn, session, Command::Send { draft, replace, ticket, release: crate::model::Release::Now }).await
        }
        // Сервер сам держит письмо до срока (FUTURERELEASE): команда уходит пути как есть, «Отменить» придёт
        // `Unsend`/`Withhold` мимо очереди — она пуста (AUD-02).
        Command::Send { draft, replace, ticket, release } if server_holds => {
            forward(conn, session, Command::Send { draft, replace, ticket, release }).await
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
        // Шаг поручения с окном «Отменить» (AUD-03): письма шага — в очередь ядра, интерфейсу — «шаг ушёл» со
        // списком отложенных («Отменить» — `Withhold`); по сроку уйдут обычной отправкой ([`release`]).
        Command::Delegate { uid, drafts, ticket, release } if server_holds => {
            forward(conn, session, Command::Delegate { uid, drafts, ticket, release }).await
        }
        Command::Delegate { uid, drafts, ticket, release: crate::model::Release::After(secs) } => {
            let scheduled = drafts.iter().map(|d| queue.hold(d.clone(), None, secs)).collect();
            reply(Event::Delegated { ticket, uid, error: None, scheduled });
            true
        }
        Command::Delegate { uid, drafts, ticket, release: crate::model::Release::At(at) } => {
            let secs = u32::try_from(at.saturating_sub(jiff::Timestamp::now().as_second())).unwrap_or(u32::MAX);
            let scheduled = drafts.iter().map(|d| queue.hold(d.clone(), None, secs)).collect();
            reply(Event::Delegated { ticket, uid, error: None, scheduled });
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
        // Список ждущих: у пути с серверной отложкой — его (сервер держит, AUD-02), иначе — очередь ядра.
        Command::Scheduled if server_holds => forward(conn, session, Command::Scheduled).await,
        Command::Scheduled => {
            reply(Event::Scheduled { list: queue.scheduled(), max_delay: 0 });
            true
        }
        cmd => forward(conn, session, cmd).await,
    }
}

/// Путь объявил `Feature::ServerSchedule` (`Event::Features`, AUD-02): «Отправить позже» и окно «Отменить» держит
/// сервер (FUTURERELEASE) — ядру в очереди держать нечего. Фичи ещё нет — ядро держит сам (как у IMAP и EWS).
fn server_schedule(conn: &Arc<super::remote::Conn>, session: u64) -> bool {
    lock(&conn.mails).get(&session).is_some_and(|s| lock(&s.features).has(crate::model::Feature::ServerSchedule))
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

/// Узел адреса учётки со своей схемой (`imaps://узел[:порт][?…]` → `узел`) — там ищут пути областей (AUD-01).
fn host_of(url: &str) -> Option<String> {
    let (_, rest) = url.split_once("://")?;
    let server = rest.split('?').next().unwrap_or_default().trim_end_matches('/');
    let host = server.rsplit_once(':').map_or(server, |(h, _)| h);
    (!host.is_empty()).then(|| host.to_owned())
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
#[cfg(test)]
mod tests {
    use std::io::pipe;
    use std::sync::atomic::AtomicUsize;
    use std::time::Instant;

    use super::*;
    use crate::backend::remote::{AreaServer, AreaSession, FetchedBody, Host, Launcher, Started, serve_mail_area};
    use crate::backend::wire::{Kind, Settings};
    use crate::backend::{AreaPlugin, Login, Registry};
    use crate::model::{Auth, Error as ModelError};
    use crate::sync::{Account, Command, Event};
    use postman_proc::link::Link;

    /// Выход ([`MAIL_LEAVING`]) и счётчик держащих — на всю программу: тесты очереди — по одному.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    const ROW: super::super::remote::Row = super::super::remote::Row {
        id: "ews",
        kind: Kind::Mail,
        claim: crate::backend::remote::Claim::Path("/ews/exchange.asmx"),
        set: "postman-plugins",
        profile: crate::backend::remote::Profile::Network { enterprise_auth: true },
    };

    /// Строка пути IMAP для детекта (AUD-01): своя схема — `imap` (обе, `imaps://` и `imap://`).
    const IMAP_ROW: super::super::remote::Row = super::super::remote::Row {
        id: "imap",
        kind: Kind::Mail,
        claim: crate::backend::remote::Claim::Scheme("imap"),
        set: "postman-plugins",
        profile: crate::backend::remote::Profile::Network { enterprise_auth: false },
    };

    /// Общее состояние подменного пути: команды собирает в поток своей «синхронизации».
    struct MockState {
        opens: AtomicUsize,
        sent: Mutex<Vec<Command>>,
        /// Возможности, которые путь объявляет при входе (`Event::Features`); по умолчанию — никаких (AUD-02).
        features: crate::model::Features,
    }

    /// Подменный путь: команды молча собирает, отправку подтверждает («отправлено» с тем же билетом).
    struct MockMail(Arc<MockState>);

    impl MockMail {
        fn new() -> Arc<Self> {
            Self::with_features(crate::model::Features::NONE)
        }

        /// Путь с объявленными возможностями (`Event::Features` при входе — до ответа на вход, AUD-02).
        fn with_features(features: crate::model::Features) -> Arc<Self> {
            Arc::new(Self(Arc::new(MockState { opens: AtomicUsize::new(0), sent: Mutex::new(Vec::new()), features })))
        }

        fn state(&self) -> Arc<MockState> {
            Arc::clone(&self.0)
        }

        fn sent(&self) -> Vec<Command> {
            lock(&self.0.sent).clone()
        }

        /// Ждать команду отправки у пути (сессия могла ещё подниматься).
        fn wait_sent(&self, n: usize) {
            let until = Instant::now() + Duration::from_secs(10);
            while self.sent().len() < n && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }

    impl MailServer for MockMail {
        fn probe<'a>(&'a self, _url: &'a str) -> BoxFuture<'a, Option<String>> {
            Box::pin(async { None })
        }

        fn open(
            &self,
            _url: String,
            _profile: Option<String>,
            _login: MailLogin,
            _services: MailServices,
            mut commands: UnboundedReceiver<Command>,
            notify: Arc<dyn Fn(Event) + Send + Sync>,
        ) -> BoxFuture<'_, Result<(), String>> {
            let state = self.state();
            state.opens.fetch_add(1, Ordering::Relaxed);
            // Возможности и первый пакет синхронизации — до ответа на вход: хост обработает их до команд (как у
            // JMAP фичи приходят сразу после входа, AUD-02; пакет — возврат приоритета, AUD-04).
            notify(Event::Features(state.features));
            notify(Event::MailboxesChanged);
            let spawned = std::thread::Builder::new().name("mock-mail-sync".into()).spawn(move || {
                let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return };
                rt.block_on(async move {
                    while let Some(cmd) = commands.recv().await {
                        lock(&state.sent).push(cmd.clone());
                        if let Command::Send { ticket, .. } = cmd {
                            notify(Event::Sent { ticket, scheduled: None });
                        }
                    }
                });
            });
            Box::pin(async move {
                spawned.map_err(|e| e.to_string())?;
                Ok(())
            })
        }
    }

    /// Области подменного пути: «не моя» на всё.
    struct MockAreas;

    impl AreaServer for MockAreas {
        fn probe<'a>(&'a self, _host: &'a str) -> BoxFuture<'a, Option<String>> {
            Box::pin(async { None })
        }

        fn connect<'a>(
            &'a self,
            _url: &'a str,
            _login: &'a Login,
            _profile: Option<&'a str>,
        ) -> BoxFuture<'a, Result<Box<dyn AreaSession>, ModelError>> {
            Box::pin(async { Err(ModelError::Protocol("mock: no areas".into())) })
        }

        fn fetch<'a>(&'a self, _url: &'a str, _max: u32, _etag: Option<&'a str>) -> BoxFuture<'a, Result<Option<FetchedBody>, String>> {
            Box::pin(async { Err("mock: no fetch".into()) })
        }
    }

    /// Сторона процесса — поток теста с точкой входа набора ([`serve_mail_area`]); второй элемент — журнал вызовов
    /// понижения/возврата приоритета процесса (`background`, AUD-04).
    struct InThread(Arc<MockMail>, Arc<Mutex<Vec<bool>>>);

    impl InThread {
        fn new(mail: Arc<MockMail>) -> (Arc<Self>, Arc<Mutex<Vec<bool>>>) {
            let priority = Arc::new(Mutex::new(Vec::new()));
            (Arc::new(Self(mail, Arc::clone(&priority))), priority)
        }
    }

    impl Launcher for InThread {
        fn launch(&self, _row: &crate::backend::remote::Row) -> std::io::Result<Started> {
            let (from_plugin, plugin_out) = pipe()?;
            let (plugin_in, to_plugin) = pipe()?;
            let mock = Arc::clone(&self.0);
            let priority = Arc::clone(&self.1);
            std::thread::spawn(move || {
                let link = Link::new(plugin_in, plugin_out, crate::backend::remote::MAX_BIN).unwrap();
                serve_mail_area(link, "ews", Some(mock), || Arc::new(MockAreas));
            });
            Ok(Started {
                link: Link::new(from_plugin, to_plugin, crate::backend::remote::MAX_BIN)?,
                kill: Box::new(|| {}),
                exit_code: Box::new(|| None),
                background: Box::new(move |on| {
                    lock(&priority).push(on);
                    Ok(())
                }),
            })
        }
    }

    fn draft() -> postman_compose::Draft {
        postman_compose::Draft {
            from: postman_compose::Address::new("Я", "me@example.test"),
            to: vec![postman_compose::Address::new("Анна", "anna@example.test")],
            subject: "Отчёт".into(),
            ..postman_compose::Draft::default()
        }
    }

    fn account() -> Account {
        Account { url: "https://mail.example.test/EWS/Exchange.asmx".into(), auth: Auth::Basic { user: "user".into(), password: "pw".into() } }
    }

    fn spawn_at(host: &Host, cache: postman_cache::Cache) -> (UnboundedSender<Command>, std::sync::mpsc::Receiver<Event>) {
        let (events, rx) = std::sync::mpsc::channel();
        let commands = host.mail(ROW).spawn(Spawn { account: account(), profile: None, cache, index: None, events, repaint: Box::new(|| {}) });
        (commands, rx)
    }

    /// Письмо в очереди — в базе профиля, ключ `send.queue/<id отправки>` (учётка основная — префикс пустой).
    fn queue_key(submission: &str) -> String {
        format!("{QUEUE_PREFIX}/{submission}")
    }

    /// Падение пути-процесса (или самой программы) при письме в окне «Отменить» письмо не теряет: очередь в базе
    /// профиля, следующий вход сессии — письмо в путь, ядро дожидается подтверждения.
    #[test]
    fn a_letter_from_a_crashed_run_leaves_at_the_next_sign_in() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        MAIL_LEAVING.store(false, Ordering::Relaxed);
        let mock = MockMail::new();
        let (launcher, _priority) = InThread::new(Arc::clone(&mock));
        let host = Host::new(launcher, Settings::default());
        host.first_frame();
        let cache = postman_cache::Cache::open_in_memory().unwrap();
        // Письмо, оставшееся от не дошедшей отправки (срок вышел: процесс упал в окне «Отменить»).
        let send =
            MailSend { submission: "hold.9".into(), draft: draft(), replace: Some("old".into()), due: jiff::Timestamp::now().as_second() - 60 };
        let put = postman_cache::LocalPut { key: queue_key(&send.submission), value: serde_json::to_vec(&send).unwrap(), parts: Vec::new() };
        cache.put_local(&put).unwrap();
        // Сессия открылась: письмо из базы — в путь, без «Отменить» (срок прошёл).
        let (_commands, rx) = spawn_at(&host, cache.clone());
        mock.wait_sent(1);
        let sent = mock.sent();
        assert_eq!(sent.len(), 1, "письмо прошлого запуска ушло при входе: {sent:?}");
        assert!(matches!(&sent[0], Command::Send { release: crate::model::Release::Now, replace: Some(r), .. } if r == "old"));
        let until = Instant::now() + Duration::from_secs(10);
        while mail_holding() > 0 && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(mail_holding(), 0, "подтверждение пути разметило очередь");
        assert!(cache.local_prefix(&queue_key("")).unwrap().is_empty(), "письмо из базы ушло");
        let _ = rx.recv_timeout(Duration::from_millis(200));
    }

    /// Очередь сама по себе: запись — перезагрузка — выдача (без пути и процесса); счётчик держащих — сходится.
    #[test]
    fn queue_round_trip() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        MAIL_LEAVING.store(false, Ordering::Relaxed);
        let cache = postman_cache::Cache::open_in_memory().unwrap();
        let q = MailQueue::new(cache.clone(), "");
        let scheduled = q.hold(draft(), Some("old".into()), 60);
        assert_eq!(q.pending(), 1);
        assert_eq!(cache.local_prefix("send.queue/").unwrap().len(), 1);
        // Срок не вышел: перезагрузка из базы письмо держит.
        let q2 = MailQueue::new(cache.clone(), "");
        assert_eq!(q2.pending(), 1, "перезагрузка из базы");
        assert_eq!(q2.release_due(false).len(), 0, "срок ещё не вышел");
        // Срок прошёл (процесс упал в окне): выдача — письмо из базы убрано.
        let past = MailSend {
            submission: scheduled.submission.clone(),
            draft: draft(),
            replace: Some("old".into()),
            due: jiff::Timestamp::now().as_second() - 1,
        };
        cache
            .put_local(&postman_cache::LocalPut {
                key: queue_key(&scheduled.submission),
                value: serde_json::to_vec(&past).unwrap(),
                parts: Vec::new(),
            })
            .unwrap();
        let q3 = MailQueue::new(cache.clone(), "");
        let due = q3.release_due(false);
        assert_eq!(due.len(), 1, "срок вышел — выдача");
        assert_eq!(due[0].1.replace.as_deref(), Some("old"));
        assert!(cache.local_prefix("send.queue/").unwrap().is_empty(), "выданное из базы убрано");
        // Выданные копии (своя, перезагрузок) — тоже отпустить: счётчик держащих сходится в ноль.
        q.release_due(true);
        q2.release_due(true);
        assert_eq!(mail_holding(), 0, "счётчик держащих сходится");
    }

    /// Отмена из окна: письмо в очереди — убрано, путь его не получает.
    #[test]
    fn unsend_cancels_a_queued_letter() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        MAIL_LEAVING.store(false, Ordering::Relaxed);
        let mock = MockMail::new();
        let (launcher, _priority) = InThread::new(Arc::clone(&mock));
        let host = Host::new(launcher, Settings::default());
        host.first_frame();
        let (commands, rx) = spawn_at(&host, postman_cache::Cache::open_in_memory().unwrap());
        commands.send(Command::Send { draft: Box::new(draft()), replace: None, ticket: 1, release: crate::model::Release::After(60) }).unwrap();
        let scheduled = loop {
            match rx.recv_timeout(Duration::from_secs(10)) {
                Ok(Event::Sent { ticket: 1, scheduled }) => {
                    assert!(scheduled.is_some(), "окно «Отменить» предложено");
                    break scheduled.unwrap();
                }
                Ok(_) => {}
                Err(e) => panic!("нет ответа на отправку: {e}"),
            }
        };
        assert_eq!(mail_holding(), 1, "письмо держит очередь ядра");
        assert!(mock.sent().is_empty(), "путь письма ещё не получил");
        commands.send(Command::Unsend { scheduled, ticket: 2 }).unwrap();
        loop {
            match rx.recv_timeout(Duration::from_secs(10)) {
                Ok(Event::Unsent { ticket: 2, .. }) => break,
                Ok(_) => {}
                Err(e) => panic!("нет ответа на отмену: {e}"),
            }
        }
        assert_eq!(mail_holding(), 0, "письмо убрано из очереди");
        assert!(mock.sent().is_empty(), "путь письмо не получал");
    }

    /// Выход при письме в окне «Отменить»: уходит сейчас ([`mail_leave`]), выход дожидается подтверждения пути.
    #[test]
    fn leave_flushes_the_undo_window_and_waits_for_the_path() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        MAIL_LEAVING.store(false, Ordering::Relaxed);
        let mock = MockMail::new();
        let (launcher, _priority) = InThread::new(Arc::clone(&mock));
        let host = Host::new(launcher, Settings::default());
        host.first_frame();
        let cache = postman_cache::Cache::open_in_memory().unwrap();
        let (commands, _rx) = spawn_at(&host, cache.clone());
        commands.send(Command::Send { draft: Box::new(draft()), replace: None, ticket: 1, release: crate::model::Release::After(60) }).unwrap();
        let until = Instant::now() + Duration::from_secs(10);
        while mail_holding() < 1 && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(mail_holding(), 1);
        mail_leave();
        mock.wait_sent(1);
        assert_eq!(mock.sent().len(), 1, "письмо ушло при выходе, не по сроку");
        let until = Instant::now() + Duration::from_secs(10);
        while mail_holding() > 0 && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(mail_holding(), 0, "выход дождался подтверждения");
        assert!(cache.local_prefix(&queue_key("")).unwrap().is_empty(), "письмо из базы ушло");
    }

    /// Область для детекта: «есть» на любом узле; сеть не проверяется — путь со своей схемой отвечает claims без
    /// процесса (AUD-01).
    struct MockArea;

    impl AreaPlugin for MockArea {
        fn key(&self) -> &'static str {
            "dav"
        }

        fn probe<'a>(&'a self, host: &'a str) -> BoxFuture<'a, Option<String>> {
            Box::pin(async move { Some(format!("https://{host}/dav")) })
        }

        fn connect<'a>(
            &'a self,
            _url: &'a str,
            _login: &'a Login,
            _profile: Option<&'a str>,
        ) -> BoxFuture<'a, Result<Box<dyn crate::backend::Areas>, ModelError>> {
            Box::pin(async { Err(ModelError::Protocol("mock: no areas".into())) })
        }
    }

    /// Детект имап-учётки (AUD-01, регресс PLG-09): у `imaps://` области DAV ищутся на узле адреса и дописываются в
    /// него; у пути со своими областями (EWS — адрес путём) адрес не меняется.
    #[test]
    fn detect_of_an_imap_account_finds_dav_areas() {
        let mock = MockMail::new();
        let (launcher, _priority) = InThread::new(Arc::clone(&mock));
        let host = Host::new(launcher, Settings::default());
        // Оба пути реестра отвечают по claims без процесса: детект не поднимает набор и ничего не ждёт.
        let registry = Registry::default().with_mail(host.mail(IMAP_ROW)).with_mail(host.mail(ROW)).with_area(MockArea);
        assert_eq!(
            registry.detect_blocking("imaps://mail.example.test"),
            "imaps://mail.example.test?dav=https://mail.example.test/dav",
            "области DAV — на узле адреса учётки"
        );
        assert_eq!(
            registry.detect_blocking("imaps://mail.example.test:993"),
            "imaps://mail.example.test:993?dav=https://mail.example.test/dav",
            "порт в адрес области не попадает"
        );
        assert_eq!(
            registry.detect_blocking("imaps://mail.example.test?dav=https://other.test/dav"),
            "imaps://mail.example.test?dav=https://other.test/dav",
            "адрес области уже есть — не трогаем"
        );
        assert_eq!(
            registry.detect_blocking("https://mail.example.test/ews/exchange.asmx"),
            "https://mail.example.test/ews/exchange.asmx",
            "путь со своими областями — адрес как был"
        );
    }

    /// Путь объявил серверную отложку (`Feature::ServerSchedule`, AUD-02): «Отправить позже» и список отложенных
    /// уходят пути как есть — ядро ничего не держит.
    #[test]
    fn server_schedule_sends_go_to_the_path() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        MAIL_LEAVING.store(false, Ordering::Relaxed);
        let features = crate::model::Features::NONE.with(crate::model::Feature::ServerSchedule, true);
        let mock = MockMail::with_features(features);
        let (launcher, _priority) = InThread::new(Arc::clone(&mock));
        let host = Host::new(launcher, Settings::default());
        host.first_frame();
        let (commands, rx) = spawn_at(&host, postman_cache::Cache::open_in_memory().unwrap());
        commands
            .send(Command::Send { draft: Box::new(draft()), replace: Some("old".into()), ticket: 1, release: crate::model::Release::After(3600) })
            .unwrap();
        mock.wait_sent(1);
        let sent = mock.sent();
        assert!(
            matches!(&sent[0], Command::Send { ticket: 1, replace, release: crate::model::Release::After(3600), .. } if replace.as_deref() == Some("old")),
            "команда ушла пути как есть: {sent:?}"
        );
        assert_eq!(mail_holding(), 0, "очередь ядра письмо не держит");
        // Ответ пути интерфейсу проходит («отправлено», окно — забота сервера).
        loop {
            match rx.recv_timeout(Duration::from_secs(10)) {
                Ok(Event::Sent { ticket: 1, .. }) => break,
                Ok(_) => {}
                Err(e) => panic!("нет ответа на отправку: {e}"),
            }
        }
        // Список ждущих — тоже путь (сервер держит их, не очередь ядра).
        commands.send(Command::Scheduled).unwrap();
        mock.wait_sent(2);
        assert!(mock.sent().iter().any(|c| matches!(c, Command::Scheduled)), "запрос списка ушёл пути: {:?}", mock.sent());
    }

    /// Фичи нет (как у IMAP и EWS): письмо до срока держит очередь ядра, список отложенных — свой, предел — 0 (AUD-02).
    #[test]
    fn without_server_schedule_the_letter_is_queued_locally() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        MAIL_LEAVING.store(false, Ordering::Relaxed);
        let mock = MockMail::new();
        let (launcher, _priority) = InThread::new(Arc::clone(&mock));
        let host = Host::new(launcher, Settings::default());
        host.first_frame();
        let (commands, rx) = spawn_at(&host, postman_cache::Cache::open_in_memory().unwrap());
        commands.send(Command::Send { draft: Box::new(draft()), replace: None, ticket: 1, release: crate::model::Release::After(60) }).unwrap();
        let scheduled = loop {
            match rx.recv_timeout(Duration::from_secs(10)) {
                Ok(Event::Sent { ticket: 1, scheduled: Some(s) }) => break s,
                Ok(_) => {}
                Err(e) => panic!("нет ответа на отправку: {e}"),
            }
        };
        assert_eq!(mail_holding(), 1, "письмо держит очередь ядра");
        assert!(mock.sent().is_empty(), "путь письма не получил");
        commands.send(Command::Scheduled).unwrap();
        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Event::Scheduled { list, max_delay: 0 }) => assert_eq!(list.len(), 1, "список — из очереди ядра"),
            Ok(e) => panic!("не тот ответ на список: {e:?}"),
            Err(e) => panic!("нет ответа на список: {e}"),
        }
        // Убрать за собой: счётчик держащих общий на программу.
        commands.send(Command::Unsend { scheduled, ticket: 2 }).unwrap();
        loop {
            match rx.recv_timeout(Duration::from_secs(10)) {
                Ok(Event::Unsent { ticket: 2, .. }) => break,
                Ok(_) => {}
                Err(e) => panic!("нет ответа на отмену: {e}"),
            }
        }
        assert_eq!(mail_holding(), 0);
    }

    /// Окно «Отменить» у писем шага поручения (AUD-03): письма шага держит очередь ядра, «Отменить» убирает их до
    /// срока — путь их не получает.
    #[test]
    fn delegate_steps_wait_in_the_undo_window() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        MAIL_LEAVING.store(false, Ordering::Relaxed);
        let mock = MockMail::new();
        let (launcher, _priority) = InThread::new(Arc::clone(&mock));
        let host = Host::new(launcher, Settings::default());
        host.first_frame();
        let cache = postman_cache::Cache::open_in_memory().unwrap();
        let (commands, rx) = spawn_at(&host, cache.clone());
        commands
            .send(Command::Delegate { uid: "t-1@postman".into(), drafts: vec![draft()], ticket: 1, release: crate::model::Release::After(60) })
            .unwrap();
        let scheduled = loop {
            match rx.recv_timeout(Duration::from_secs(10)) {
                Ok(Event::Delegated { ticket: 1, uid, error: None, scheduled }) => {
                    assert_eq!(uid, "t-1@postman");
                    assert_eq!(scheduled.len(), 1, "окно «Отменить» предложено");
                    break scheduled;
                }
                Ok(_) => {}
                Err(e) => panic!("нет ответа на шаг поручения: {e}"),
            }
        };
        assert_eq!(mail_holding(), 1, "письмо шага держит очередь ядра");
        assert!(mock.sent().is_empty(), "путь письмо шага ещё не получил");
        // «Отменить» шага: письма убраны из очереди и из базы, путь не звали.
        commands.send(Command::Withhold { scheduled, ticket: 2 }).unwrap();
        loop {
            match rx.recv_timeout(Duration::from_secs(10)) {
                Ok(Event::Withheld { ticket: 2, too_late: false, error: None }) => break,
                Ok(_) => {}
                Err(e) => panic!("нет ответа на отмену шага: {e}"),
            }
        }
        assert_eq!(mail_holding(), 0, "письма шага убраны из очереди");
        assert!(mock.sent().is_empty(), "отменённые письма путь не получил");
        assert!(cache.local_prefix(&queue_key("")).unwrap().is_empty(), "письма шага из базы убраны");
    }

    /// Срок окна вышел: письма шага уходят пути обычной отправкой (по тику), подтверждение разметило очередь (AUD-03).
    #[test]
    fn delegate_steps_leave_when_the_window_is_over() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        MAIL_LEAVING.store(false, Ordering::Relaxed);
        let mock = MockMail::new();
        let (launcher, _priority) = InThread::new(Arc::clone(&mock));
        let host = Host::new(launcher, Settings::default());
        host.first_frame();
        let cache = postman_cache::Cache::open_in_memory().unwrap();
        let (commands, _rx) = spawn_at(&host, cache.clone());
        // Срок в ноль: очередь выпускает письмо на ближайшем тике.
        commands
            .send(Command::Delegate { uid: "t-1@postman".into(), drafts: vec![draft()], ticket: 1, release: crate::model::Release::After(0) })
            .unwrap();
        mock.wait_sent(1);
        let sent = mock.sent();
        assert!(
            matches!(&sent[0], Command::Send { release: crate::model::Release::Now, replace: None, .. }),
            "по сроку шаг ушёл обычной отправкой: {sent:?}"
        );
        let until = Instant::now() + Duration::from_secs(10);
        while mail_holding() > 0 && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(mail_holding(), 0, "подтверждение пути разметило очередь");
        assert!(cache.local_prefix(&queue_key("")).unwrap().is_empty(), "письмо из базы ушло");
    }

    /// Первый пакет событий синхронизации почты возвращает приоритет процесса (AUD-04, как курсор у каналов):
    /// запуск — понижен, папки дошли до ядра — обратно.
    #[test]
    fn first_mail_sync_packet_restores_the_process_priority() {
        let mock = MockMail::new();
        let (launcher, priority) = InThread::new(Arc::clone(&mock));
        let host = Host::new(launcher, Settings::default());
        host.first_frame();
        let (_commands, _rx) = spawn_at(&host, postman_cache::Cache::open_in_memory().unwrap());
        let until = Instant::now() + Duration::from_secs(10);
        while (*lock(&priority) != vec![true, false]) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(*lock(&priority), vec![true, false], "запуск понизил приоритет, первый пакет событий синка вернул");
    }
}
