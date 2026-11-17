//! Синхронизация: договор команд и событий между интерфейсом и путями к серверам, и общий цикл (ADR 0004).
//! UI шлёт команды, в ответ получает события и перечитывает кэш. Каждая учётная запись — свой поток с tokio и
//! своим соединением с кэшем; что внутри потока (JMAP, IMAP…) — дело плагина пути (`postman-backend-*`), `core`
//! даёт ему поток ([`spawn_thread`]), цикл «без связи» ([`supervise`]) и этот договор.
//!
//! Команды и события сериализуются (serde; кодек — postcard, как у обмена каналов `backend::wire`): тест
//! «туда и обратно» проходит каждый вариант. Атрибуты переименования не вводятся: имена полей и вариантов — и есть
//! формат обмена.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use postman_calendar::{Busy, BusyNote, Invitation, Partstat};
use postman_compose::{Address, Draft};

use crate::backend::BoxFuture;
use crate::categories;
use crate::export;
use crate::invite::Delivery;
use crate::lifecycle;
use crate::link::{Connection, Link};
use crate::model::{
    Auth, ContactCard, Filter, FolderId, ImportError, JobId, LetterId, MigrationRequest, MigrationState, Principal, Remote, Selector, SweepId,
};
pub use crate::model::{Error, ErrorKind, Release};
use crate::schedule::Meeting;
use crate::send_later::Scheduled;
use crate::settings;
use crate::sweep;
use crate::tasks;
use postman_cache::MailboxRow;
use postman_search::SearchIndex;
use tokio::sync::mpsc::UnboundedReceiver;
/// Канал команд синхронизации (UI держит отправителя).
pub use tokio::sync::mpsc::UnboundedSender;

#[derive(Clone)]
pub struct Account {
    pub url: String,
    pub auth: Auth,
}

impl Account {
    pub fn from_env() -> Option<Self> {
        let url = std::env::var("POSTMAN_URL").ok()?;
        let auth = match std::env::var("POSTMAN_TOKEN") {
            Ok(t) => Auth::Bearer(t),
            Err(_) => Auth::Basic { user: std::env::var("POSTMAN_USER").ok()?, password: std::env::var("POSTMAN_PASSWORD").ok()? },
        };
        Some(Self { url, auth })
    }
}

/// Кому открыт мой календарь (S8-43): адрес, имя и видит ли подробности событий (иначе — только занятость).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Access {
    pub email: String,
    pub name: String,
    pub items: bool,
}

/// Ссылка публикации моего календаря (S8-45): адрес виден только при создании.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Publication {
    pub id: String,
    /// С подробностями событий, иначе — только занятость.
    pub items: bool,
}

/// Адресная книга сервера (T2-48): JMAP `AddressBook/get`, CardDAV — коллекции адресных книг, Exchange — папки
/// контактов. Куда сохранить новый контакт; переноса между книгами нет.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactBook {
    /// У пути: id книги JMAP, путь коллекции CardDAV, id папки Exchange.
    pub id: String,
    pub name: String,
    /// Книга по умолчанию: в неё пишет Postman без выбора (у CardDAV — первая доступная для записи, у Exchange —
    /// сама папка «Контакты»).
    pub default: bool,
}

/// Ключевое слово команды [`Command::Keyword`] — статика ядра ([`crate::labels`], [`crate::marks`]). Псевдоним, а
/// не `&'static str` в поле: serde неявно «заимствует» поля-ссылки (`&str`, `&[u8]`), и у типа с такой ссылкой
/// derive даёт только `Deserialize<'static>`; псевдоним прячет ссылку от макроса, договор не меняется.
pub type Keyword = &'static str;

/// Команда интерфейса синхронизации учётной записи: сериализуется (postcard), смысл договора — как в вариантах.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Command {
    Body(String),
    /// Беседа (ключ цепочки, `EmailRow::thread_id`): письма из всех папок и их тела — в кэш → `ThreadReady`.
    Thread(String),
    Keyword {
        id: String,
        /// По обмену — строкой, на приёме — та же статика (модуль [`keyword`]).
        #[serde(with = "keyword")]
        keyword: Keyword,
        on: bool,
    },
    /// Поставить `add` и снять `remove` (ключевые слова — категории, метки, флаг) у писем `ids` (ключи кэша) — по
    /// одному `Email/set` на аккаунт (S5-02). UI уже поменял кэш; письма, которые сервер не изменил, — с сервера
    /// заново.
    SetKeywords {
        ids: Vec<String>,
        add: Vec<String>,
        remove: Vec<String>,
    },
    /// Письма по фильтру на сервере (`hasKeyword`, `notKeyword`…) → `SearchResults`. `mailbox` — ключ папки кэша;
    /// `None` — все папки своего аккаунта («Где искать: Все папки»).
    Query {
        filter: Filter,
        mailbox: Option<String>,
        ticket: u64,
    },
    /// Число писем по каждому фильтру на сервере (в папке `mailbox` или во всех) → `Counts`.
    Counts {
        filters: Vec<Filter>,
        mailbox: Option<String>,
        ticket: u64,
    },
    /// Перенести письмо (удаление в «Удалённые»). UI уже перенёс его в кэше; сбой — письмо с сервера заново.
    Move {
        id: String,
        from: String,
        to: String,
    },
    /// Удалить письмо насовсем (из «Удалённых»). UI уже убрал его из кэша; сбой — письмо с сервера заново.
    Destroy(String),
    /// Новая папка (T1-04б) → дерево перечитано, `MailboxesChanged`; отказ — `FolderFailed`. `account` — аккаунт
    /// ключей кэша (`postman_cache::account_of`; `None` — основной), `parent` — ключ папки-родителя, `None` — верхний
    /// уровень ящика.
    CreateFolder {
        account: Option<String>,
        parent: Option<String>,
        name: String,
    },
    /// Переименовать папку (ключ кэша) → `MailboxesChanged` или `FolderFailed`.
    RenameFolder {
        mailbox: String,
        name: String,
    },
    /// Перенести папку под `parent` (`None` — верхний уровень ящика) → `MailboxesChanged` или `FolderFailed`. В саму
    /// себя, в свою подпапку и в другой аккаунт — отказ ядра до плагина ([`admit`]).
    MoveFolder {
        mailbox: String,
        parent: Option<String>,
    },
    /// `replace` — id прежней версии черновика на сервере. `release` — когда сервер выпустит письмо (S5-06):
    /// `After` — окно «Отменить» (без FUTURERELEASE у сервера — сразу), `At` — «Отправить позже» → `Sent`.
    Send {
        draft: Box<Draft>,
        replace: Option<String>,
        ticket: u64,
        release: Release,
    },
    /// Отменить отправку, которую сервер ещё держит, и вернуть письмо в «Черновики» → `Unsent` или `UnsendFailed`,
    /// затем `Scheduled`.
    Unsend {
        scheduled: Scheduled,
        ticket: u64,
    },
    /// Перечитать отложенные отправки → `Scheduled`.
    Scheduled,
    SaveDraft {
        draft: Box<Draft>,
        replace: Option<String>,
        ticket: u64,
    },
    /// Догрузить следующую страницу ящика (после самого старого письма в кэше).
    LoadMore {
        mailbox: String,
    },
    /// Серверный поиск; `mailbox: None` — во всех папках.
    Search {
        query: String,
        mailbox: Option<String>,
        ticket: u64,
    },
    /// Письмо по `Message-ID` на сервере (ADR 0012 п. 3: адрес `mid:` или `postman://mail/…`, письма нет в базе) —
    /// во всех учётках профиля и их общих ящиках: найденные строки — в кэш → `SearchResults` (ключи кэша, тело —
    /// `Body` по ключу). JMAP — `Email/query` с условием `header`, IMAP — `SEARCH HEADER Message-ID`, EWS —
    /// `FindItem` по `InternetMessageId`. `message_id` — без угловых скобок. Путь без поиска — пустой ответ
    /// ([`Command::unsupported`]).
    FindMessage {
        message_id: String,
        ticket: u64,
    },
    /// Скачать вложение/картинку.
    Blob {
        blob_id: String,
        name: String,
        mime: String,
    },
    /// Ответить на приглашение из письма; `in_reply_to` — Message-ID письма с приглашением; `answer` — с комментарием
    /// или без ответа организатору (T2-15).
    RespondInvite {
        invite: Box<Invitation>,
        status: Partstat,
        in_reply_to: Option<String>,
        answer: crate::invite::Answer,
    },
    /// Приглашение, пришедшее только письмом (iMIP, RFC 6047; T2-08в): положить в мой календарь как «не ответил»
    /// (`NEEDS-ACTION`), без ответа организатору → `CalendarChanged`. Сервер, который разбирает приглашения сам
    /// (Exchange), команду пропускает.
    ImportInvite {
        invite: Box<Invitation>,
    },
    /// «Удалить из календаря» у отменённой встречи (T2-19, решение владельца 2026-11-10): моя копия встречи с UID
    /// приглашения уходит из календаря без писем организатору → `InviteRemoved` (`found: false` — копии нет) или
    /// `InviteFailed`; удалённая — ещё и `CalendarChanged`.
    RemoveInvite {
        invite: Box<Invitation>,
    },
    /// События календаря в `[from, to)` (Unix-секунды), повторения разворачивает сервер → `EventsLoaded`.
    Events {
        from: i64,
        to: i64,
    },
    /// Поиск в календаре по тексту в `[from, to)` → `EventsFound` (S5-21).
    SearchEvents {
        text: String,
        from: i64,
        to: i64,
    },
    /// Занятость адресов и моя в `[from, to)` → `Availability`. `sender` — адрес «От» встречи (S7-03б): занятость
    /// — путём его учётной записи профиля; `None` — основной.
    Availability {
        ticket: u64,
        emails: Vec<String>,
        from: i64,
        to: i64,
        sender: Option<String>,
    },
    /// Создать встречу и разослать приглашения → `MeetingCreated` или `MeetingFailed`.
    CreateMeeting {
        meeting: Box<Meeting>,
        ticket: u64,
    },
    /// Изменить свою встречу (я — организатор) и разослать изменения → `MeetingUpdated` или `MeetingFailed` (S7-07б,
    /// решение владельца 2026-10-23). `event` — событие недели (id и UID — как их дал путь); `meeting` — поля вкладки
    /// встречи (`uid` — UID события, `from` — организатор). `series` — у экземпляра повтора изменить всю серию (время
    /// серии сдвигается на ту же разницу), иначе — только этот экземпляр; у одиночной не важно.
    UpdateMeeting {
        event: Box<postman_calendar::Event>,
        meeting: Box<Meeting>,
        series: bool,
        ticket: u64,
    },
    /// Отменить свою встречу: участникам — отмена, у меня встречи нет → `MeetingCancelled` или `MeetingFailed`.
    /// `series` — как у `UpdateMeeting` (иначе — только этот экземпляр); `from` — адрес организатора (учётная запись).
    CancelMeeting {
        event: Box<postman_calendar::Event>,
        series: bool,
        from: Option<String>,
        ticket: u64,
    },
    /// Открыть коллеге (адрес) мою занятость, `items` — ещё и подробности событий; уровень у открытого заменяется →
    /// `FreeBusyShared` или `CalendarFailed`.
    ShareFreeBusy {
        email: String,
        items: bool,
    },
    /// Кому открыт мой календарь (S8-43) → `CalendarAccess` или `CalendarFailed`.
    CalendarAccess,
    /// Ссылки публикации (S8-45) → `Publications`.
    Publications,
    /// Создать ссылку публикации → `Published`, затем `Publications`, или `CalendarFailed`.
    Publish {
        items: bool,
    },
    /// Отозвать ссылку → `Publications` или `CalendarFailed`.
    Unpublish {
        id: String,
    },
    /// Закрыть мой календарь коллеге → `CalendarAccess` или `CalendarFailed`.
    UnshareCalendar {
        email: String,
    },
    /// Настройки ящика на сервере: автоответ, квота (`settings.rs`) → `Event::Settings`.
    Settings(settings::Request),
    /// Перечитать личные контакты и справочник → `ContactsLoaded`, `DirectoryLoaded` (и `Contacts` для подсказок).
    Contacts,
    /// Страница справочника (T1-25б): до `max` людей с `offset` по порядку пути → `Event::DirectoryPage`. `account` —
    /// учётная запись профиля (`None` — основная).
    DirectoryPage {
        account: Option<String>,
        offset: u32,
        max: u32,
    },
    /// Люди справочника по набранной строке → `PeopleFound` (S7-08): поиск людей — функция ядра, каждый путь отвечает
    /// своим поиском (JMAP — `Principal/query`, EWS — `FindPeople`, запасной — `ResolveNames`); справочника нет — пусто.
    /// `offset` — с какого найденного (подгрузка по прокрутке, решение владельца 2026-10-28): `0` — первая страница;
    /// страница — по пути (20 человек).
    FindPeople {
        text: String,
        offset: u32,
    },
    /// Подробности человека справочника по адресу — при открытии его карточки → `PersonDetails` (S7-08б): у EWS
    /// `FindPeople` телефона не отдаёт, его даёт `ResolveNames`. У кого подробностей нет — `person: None`.
    PersonDetails {
        email: String,
    },
    /// Состав списка рассылки справочника по его адресу → `ListMembers` (T3-09): EWS — `ExpandDL`, JMAP — `members`
    /// группы `Principal`, если сервер их даёт. Один уровень: вложенный список — участник с видом `group`.
    ExpandList {
        ticket: u64,
        address: String,
    },
    /// Создать (`old: None`) или изменить карточку → `ContactSaved` или `ContactFailed`, затем `ContactsLoaded`.
    /// `book` — адресная книга новой карточки из [`Event::ContactBooks`] (T2-48; `None` — книга по умолчанию);
    /// у правки не используется, переноса между книгами нет.
    SaveContact {
        card: Box<ContactCard>,
        old: Option<Box<ContactCard>>,
        book: Option<String>,
        ticket: u64,
    },
    /// Удалить карточку → `ContactDeleted` или `ContactFailed`, затем `ContactsLoaded`.
    DeleteContact {
        id: String,
        ticket: u64,
    },
    /// Таблица категорий аккаунта на сервере (`categories.rs`, S5-02б) → `Event::Categories`.
    Categories(categories::Request),
    /// Жизненный цикл (S5-04, `lifecycle.rs`): правки писем (ключи кэша; папки — ключи кэша или
    /// [`lifecycle::ARCHIVE`] — «Архив» аккаунта, нет — создаётся с именем `archive_name`) — один `Email/set` на
    /// аккаунт. UI уже поменял кэш; письма, которые сервер не изменил, — с сервера заново.
    Patch {
        patches: Vec<lifecycle::Patch>,
        archive_name: String,
    },
    /// Рассылки «Входящих» своего аккаунта: прочитанные раньше `cutoff` (Unix-секунды), кроме последней от каждого
    /// отправителя, — в «Архив» → `Event::Newsletters`.
    Newsletters {
        cutoff: i64,
        archive_name: String,
    },
    /// «Сообщить о фишинге» (S5-08): письма `ids` (ключи кэша) — вложениями `message/rfc822` одним письмом на адрес
    /// `to` от своего адреса → `Event::Reported`. Без связи — в очередь, как правки писем.
    ReportPhishing {
        ids: Vec<String>,
        to: Address,
        subject: String,
        text: String,
        ticket: u64,
    },
    /// «Повторить сейчас» (S5-08): без связи — попытка сразу, не дожидаясь паузы; со связью — ничего.
    Reconnect,
    /// «Синхронизировать» у учётной записи (T2-51, класс B, ОК владельца 2026-11-10) и «Отправить и получить — все
    /// папки» (T3-02): сверить с сервером сейчас, не дожидаясь push и опроса; без связи — как [`Command::Reconnect`].
    /// Ход — [`Event::SendReceive`]: начало и конец сверки этой учётной записи.
    SyncNow,
    /// Задачи (S6-02, `tasks.rs`): загрузка, сверка, правки → `Event::Tasks`. Правка без связи — в кэше, уходит
    /// по возвращении связи (`tasks::Request::Flush`).
    Tasks(tasks::Request),
    /// Шаг поручения (S6-09, `assign.rs`): своя копия задачи уже в кэше (`tasks::save_local`) — сначала она на
    /// сервер (без рассылки), затем письма iTIP шага → `Event::Delegated`. Без связи — в очередь, как правки.
    /// `release` — как у `Send`: «Забрать» и «Удалить поручение» (S6-11) держатся сервером окно «Отменить».
    Delegate {
        uid: String,
        drafts: Vec<Draft>,
        ticket: u64,
        release: Release,
    },
    /// «Отменить» шага поручения (S6-11): письма, которые сервер ещё держит, не отправлять и удалить из
    /// «Отправленных» → `Event::Withheld`.
    Withhold {
        scheduled: Vec<Scheduled>,
        ticket: u64,
    },
    /// Перенести письма в папку (ADR 0006 п. 3–4) — в поток ядра: перенос в базе одной транзакцией, затем ядро решает
    /// само: письма своей учётки цели — перенос на её сервере, прочие — задание переноса (`Event::Migration`).
    MoveLetters {
        ids: Vec<LetterId>,
        to: FolderId,
    },
    /// Письмо целиком (как его отдаёт сервер) — у учётки письма → `Event::Raw`. Письмо «На этом компьютере» — из
    /// базы, без сети.
    FetchRaw {
        letter: LetterId,
    },
    /// Выгрузить письмо (содержимое — в базе) в папку `folder` учётки цели с ключевыми словами (и `$seen`,
    /// `$flagged`) и датой получения (Unix-секунды) → `Event::Imported` или `Event::ImportFailed`. Путь без выгрузки
    /// ([`Features::import`](crate::model::Features::import)) отвечает отказом. `retry` — повтор после сбоя (выгрузка
    /// могла дойти, ответ — нет): путь, у которого сервер не отвечает `alreadyExists`, сначала ищет письмо по
    /// `message_id` в папке цели и, если нашёл, отвечает `Imported` с ним — без дубля. `message_id` — с угловыми
    /// скобками, из ответа воркера (`MimeSplit`): письмо в процессе приложения путь не разбирает; `None` — у письма его
    /// нет (или воркер не ответил), искать нечем.
    Import {
        job: JobId,
        letter: LetterId,
        folder: FolderId,
        keywords: Vec<String>,
        received_at: i64,
        message_id: Option<String>,
        retry: bool,
    },
    /// Сервер сам переносит письма в папку `to` другого аккаунта той же сессии, без выгрузки
    /// ([`Features::server_copy`](crate::model::Features::server_copy)) → по письму `Event::Imported` или
    /// `Event::ImportFailed`.
    ServerCopy {
        job: JobId,
        letters: Vec<LetterId>,
        to: FolderId,
    },
    /// Удалить письма задания у источника — учётки `account` (аккаунт ключей кэша, основной — пустая строка) по их
    /// `detached`-связям → по письму `Event::Destroyed` или `Event::DestroyFailed` (решение владельца 2026-10-29:
    /// письмо у источника удалено только по ответу сервера). Письма уже нет на сервере — тоже `Destroyed`.
    DestroyLetters {
        job: JobId,
        account: String,
        letters: Vec<LetterId>,
    },
    /// Ответ, пауза, продолжение, отмена задания переноса — в поток ядра → `Event::Migration`.
    Migration(MigrationRequest),
    /// Действие над всей выборкой на сервере («выбрано всё») — в поток ядра: письма ядро получает с сервера
    /// постранично, задание переживает перезапуск (`sweep.rs`) → `Event::Sweep`.
    Sweep {
        selector: Selector,
        action: sweep::Action,
    },
    /// «Остановить» действие над выборкой — в поток ядра: уже сделанное остаётся, остальное письма не трогает.
    StopSweep {
        sweep: SweepId,
    },
    /// Страница ключей выборки: до `limit` писем с `position` по порядку сервера. Только ключи, строк писем не надо →
    /// `Event::LettersListed` или `Event::ListFailed`.
    ListLetters {
        sweep: SweepId,
        selector: Selector,
        position: u64,
        limit: u32,
    },
    /// Строки писем выборки (ключи кэша) папки `mailbox` — в кэш → `Event::LettersStored` или `Event::ListFailed`.
    StoreLetters {
        sweep: SweepId,
        mailbox: String,
        ids: Vec<String>,
    },
    /// Экспорт писем в файлы в каталог `dir` (S8-31) — в поток ядра: новое задание ([`export::Exporter::start`]) →
    /// `Event::Export`; письма без содержимого — `FetchRaw` у их учёток.
    Export {
        scope: export::Scope,
        format: export::Format,
        dir: std::path::PathBuf,
    },
    /// «Остановить» экспорт — в поток ядра: готовые файлы остаются, недописанный mbox удаляется → `Event::Export`.
    StopExport {
        job: export::ExportId,
    },
    /// Запрос интерфейса учётке канала бесед (ADR 0012 п. 6) — в её поток ([`Route::Channel`]); ответ —
    /// [`Event::Channel`]. Почта (`mail`) — адаптером: `channel::mail::MailSource::plan` переводит запрос в команды
    /// выше.
    Channel {
        account: crate::channel::ChanAccount,
        /// В коробке: исходящее с телом в IR не раздувает каждую команду.
        request: Box<crate::channel::Request>,
    },
}

impl Command {
    /// Копия команды, которую можно повторить, когда связь вернётся (`link`): правки писем и сообщение о фишинге.
    /// Остальное (тела, поиск, отправка с окном письма) без связи не ждёт — интерфейс показывает ошибку сам.
    pub fn replayable(&self) -> Option<Command> {
        Some(match self {
            Command::Keyword { id, keyword, on } => Command::Keyword { id: id.clone(), keyword, on: *on },
            Command::SetKeywords { ids, add, remove } => Command::SetKeywords { ids: ids.clone(), add: add.clone(), remove: remove.clone() },
            Command::Move { id, from, to } => Command::Move { id: id.clone(), from: from.clone(), to: to.clone() },
            Command::Destroy(id) => Command::Destroy(id.clone()),
            Command::Patch { patches, archive_name } => Command::Patch { patches: patches.clone(), archive_name: archive_name.clone() },
            Command::Tasks(r) => Command::Tasks(r.replayable()?),
            Command::Delegate { uid, drafts, ticket, release } => {
                Command::Delegate { uid: uid.clone(), drafts: drafts.clone(), ticket: *ticket, release: *release }
            }
            Command::ReportPhishing { ids, to, subject, text, ticket } => {
                Command::ReportPhishing { ids: ids.clone(), to: to.clone(), subject: subject.clone(), text: text.clone(), ticket: *ticket }
            }
            _ => return None,
        })
    }
}

/// Ключевое слово команды [`Command::Keyword`] — статика ядра (`&'static str`, константы [`crate::labels`] и
/// [`crate::marks`]): десериализация `&'static str` в serde невозможна (данные обмена живут меньше), поэтому по
/// обмену слово идёт строкой, а на приёме возвращается та же статика. Слово вне таблицы (по договору их не бывает)
/// становится новой статикой.
mod keyword {
    use serde::{Deserialize, Deserializer, Serializer};

    /// Слова, которыми интерфейс помечает письмо: флаги сервера и метки жизни письма.
    const KNOWN: &[&str] = &[
        crate::labels::SEEN,
        crate::labels::FLAGGED,
        crate::labels::ANSWERED,
        crate::labels::FORWARDED,
        crate::labels::DRAFT,
        crate::marks::CONFIDENTIAL,
        crate::marks::DUE,
        crate::marks::WAIT,
        crate::marks::REMIND,
        crate::marks::EXPIRE,
        crate::marks::TAKEN,
    ];

    pub fn serialize<S: Serializer>(v: &&'static str, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(v)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<&'static str, D::Error> {
        let word = String::deserialize(d)?;
        Ok(KNOWN.iter().copied().find(|k| *k == word).unwrap_or_else(|| Box::leak(word.into_boxed_str())))
    }
}

/// Куда идёт команда, когда учётных записей в профиле несколько (S7-03б, `docs/specs/S7.md`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// В синхронизацию учётной записи профиля: `None` — основной, `@<n>` — дополнительной.
    Profile(Option<String>),
    /// Во все: поиск и фильтры «Все папки», неделя календаря, отложенные отправки, «Повторить сейчас».
    All,
    /// Туда, чей адрес отправителя (`Send`, `SaveDraft`): учётную запись знает список «От» интерфейса.
    Sender(String),
}

/// Учётная запись профиля ключа кэша.
fn profile_of_key(key: &str) -> Option<String> {
    postman_cache::profile_of(postman_cache::account_of(key)).map(str::to_owned)
}

/// Чьё письмо или папка формата 6 (ADR 0006 п. 1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Home {
    /// Учётки: ключ — аккаунт ключей кэша (основной — пустая строка), как у `postman_cache::Owner::Account`.
    Account(String),
    /// «На этом компьютере»: без сервера.
    Local,
}

impl Home {
    /// Поток, который работает с письмами и папками этой учётки: синхронизация её учётной записи профиля; «На этом
    /// компьютере» — ядро.
    pub fn route(&self) -> Route {
        match self {
            Home::Local => Route::Core,
            Home::Account(a) => {
                Route::Sync(Target::Profile(postman_cache::profile_of(Some(a.as_str()).filter(|a| !a.is_empty())).map(str::to_owned)))
            }
        }
    }
}

/// Откуда маршрут знает, чьи письма и папки: письмо — по действующей связи с сервером (`letter_remote`), без неё —
/// [`Home::Local`]; папка — по владельцу. Даёт хранилище формата 6.
pub trait Homes {
    fn letter(&self, id: LetterId) -> Home;
    fn folder(&self, id: FolderId) -> Home;
}

/// Куда идёт команда в профиле с письмами формата 6 ([`Command::routes_by`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// Поток ядра, без сети: письма и папки «На этом компьютере», перенос между учётками и его задания.
    Core,
    /// Поток синхронизации учётной записи.
    Sync(Target),
    /// Поток учётки канала бесед (ADR 0012 п. 6): `channel::spawn`.
    Channel(crate::channel::ChanAccount),
}

impl Command {
    /// Маршрут с письмами и папками формата 6: команда над [`LetterId`] — по учётке письма (`FetchRaw`), выгрузка —
    /// по учётке папки цели (`Import`, `ServerCopy`), удаление у источника — по учётке источника (`DestroyLetters`);
    /// «На этом компьютере», перенос (`MoveLetters`), задания (`Migration`) и экспорт (`Export`, `StopExport`) — в
    /// ядро. Прочие команды — по ключам
    /// кэша, как [`Command::routes`].
    pub fn routes_by(self, homes: &dyn Homes) -> Vec<(Route, Command)> {
        let route = match &self {
            Command::Channel { account, .. } => Route::Channel(account.clone()),
            Command::FetchRaw { letter } => homes.letter(*letter).route(),
            Command::Import { folder, .. } => homes.folder(*folder).route(),
            Command::ServerCopy { to, .. } => homes.folder(*to).route(),
            Command::DestroyLetters { account, .. } => Home::Account(account.clone()).route(),
            Command::MoveLetters { .. }
            | Command::Migration(_)
            | Command::Sweep { .. }
            | Command::StopSweep { .. }
            | Command::Export { .. }
            | Command::StopExport { .. } => Route::Core,
            _ => return self.routes().into_iter().map(|(t, c)| (Route::Sync(t), c)).collect(),
        };
        vec![(route, self)]
    }

    /// Куда идёт команда и в каком виде: правки писем разных учётных записей — по команде на учётную запись. По
    /// ключам кэша (префикс учётной записи); команды над [`LetterId`] здесь — в основную, их маршрут —
    /// [`Command::routes_by`].
    pub fn routes(self) -> Vec<(Target, Command)> {
        // Учётке канала — не синхронизация почты: маршрут — [`Route::Channel`] ([`Command::routes_by`]).
        if matches!(self, Command::Channel { .. }) {
            return Vec::new();
        }
        let one = |key: &str| Target::Profile(profile_of_key(key));
        let scope = |mailbox: &Option<String>| mailbox.as_deref().map_or(Target::All, one);
        let target = match &self {
            Command::Body(k) | Command::Thread(k) | Command::Destroy(k) => one(k),
            Command::Keyword { id, .. } | Command::Move { id, .. } => one(id),
            Command::RenameFolder { mailbox, .. } | Command::MoveFolder { mailbox, .. } => one(mailbox),
            Command::CreateFolder { account, .. } => Target::Profile(postman_cache::profile_of(account.as_deref()).map(str::to_owned)),
            Command::DirectoryPage { account, .. } => Target::Profile(account.clone()),
            Command::Blob { blob_id, .. } => one(blob_id),
            Command::LoadMore { mailbox } => one(mailbox),
            Command::ListLetters { selector, .. } => one(&selector.mailbox),
            Command::StoreLetters { mailbox, .. } => one(mailbox),
            Command::Unsend { scheduled, .. } => one(&scheduled.submission),
            Command::Withhold { scheduled, .. } => scheduled.first().map_or(Target::Profile(None), |s| one(&s.submission)),
            Command::Query { mailbox, .. } | Command::Counts { mailbox, .. } | Command::Search { mailbox, .. } => scope(mailbox),
            Command::Send { draft, .. } | Command::SaveDraft { draft, .. } => Target::Sender(draft.from.email.clone()),
            Command::CreateMeeting { meeting, .. } | Command::UpdateMeeting { meeting, .. } => {
                meeting.from.clone().map_or(Target::Profile(None), Target::Sender)
            }
            Command::CancelMeeting { from, .. } => from.clone().map_or(Target::Profile(None), Target::Sender),
            Command::Availability { sender, .. } => sender.clone().map_or(Target::Profile(None), Target::Sender),
            Command::Events { .. }
            | Command::Scheduled
            | Command::Reconnect
            | Command::SyncNow
            | Command::Newsletters { .. }
            | Command::FindMessage { .. } => Target::All,
            Command::SetKeywords { .. } | Command::Patch { .. } | Command::ReportPhishing { .. } => return self.split(),
            _ => Target::Profile(None),
        };
        vec![(target, self)]
    }

    /// Команда над письмами — по учётным записям их ключей (порядок — первой встречи).
    fn split(self) -> Vec<(Target, Command)> {
        fn groups<T>(items: Vec<T>, key: impl Fn(&T) -> &str) -> Vec<(Option<String>, Vec<T>)> {
            let mut out: Vec<(Option<String>, Vec<T>)> = Vec::new();
            for item in items {
                let p = profile_of_key(key(&item));
                match out.iter_mut().find(|(q, _)| *q == p) {
                    Some((_, v)) => v.push(item),
                    None => out.push((p, vec![item])),
                }
            }
            out
        }
        match self {
            Command::SetKeywords { ids, add, remove } => groups(ids, |s| s.as_str())
                .into_iter()
                .map(|(p, ids)| (Target::Profile(p), Command::SetKeywords { ids, add: add.clone(), remove: remove.clone() }))
                .collect(),
            Command::Patch { patches, archive_name } => groups(patches, |p| p.id.as_str())
                .into_iter()
                .map(|(p, patches)| (Target::Profile(p), Command::Patch { patches, archive_name: archive_name.clone() }))
                .collect(),
            Command::ReportPhishing { ids, to, subject, text, ticket } => groups(ids, |s| s.as_str())
                .into_iter()
                .map(|(p, ids)| {
                    let c = Command::ReportPhishing { ids, to: to.clone(), subject: subject.clone(), text: text.clone(), ticket };
                    (Target::Profile(p), c)
                })
                .collect(),
            other => vec![(Target::Profile(None), other)],
        }
    }

    /// Копия команды для каждой учётной записи ([`Target::All`]); у прочих — `None`.
    pub fn for_each_account(&self) -> Option<Command> {
        Some(match self {
            Command::Query { filter, mailbox, ticket } => Command::Query { filter: filter.clone(), mailbox: mailbox.clone(), ticket: *ticket },
            Command::Counts { filters, mailbox, ticket } => Command::Counts { filters: filters.clone(), mailbox: mailbox.clone(), ticket: *ticket },
            Command::Search { query, mailbox, ticket } => Command::Search { query: query.clone(), mailbox: mailbox.clone(), ticket: *ticket },
            Command::FindMessage { message_id, ticket } => Command::FindMessage { message_id: message_id.clone(), ticket: *ticket },
            Command::Events { from, to } => Command::Events { from: *from, to: *to },
            Command::Scheduled => Command::Scheduled,
            Command::Reconnect => Command::Reconnect,
            Command::SyncNow => Command::SyncNow,
            Command::Newsletters { cutoff, archive_name } => Command::Newsletters { cutoff: *cutoff, archive_name: archive_name.clone() },
            _ => return None,
        })
    }
}

/// Что делает синхронизация. Текст — у интерфейса (`sync.ftl`), здесь только код и данные.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Status {
    Connecting {
        url: String,
    },
    SignedIn {
        user: String,
    },
    /// Первая загрузка: папка `mailbox` (имя с сервера).
    Loading {
        mailbox: String,
    },
    Synced,
}

impl ErrorKind {
    pub fn of(e: &Error) -> Self {
        e.kind()
    }
}

/// На каком шаге синхронизации случилась ошибка (для журнала).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Stage {
    Start,
    Connect,
    Load,
    Refresh,
    Prefetch,
    Command,
    Restore,
}

/// Ошибка синхронизации: род, шаг и подробность. `detail` — текст ошибки как есть (может быть ответом сервера),
/// интерфейс показывает его только как подробность после своего текста.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncError {
    pub kind: ErrorKind,
    pub stage: Stage,
    pub detail: String,
}

impl SyncError {
    pub fn new(stage: Stage, e: &Error) -> Self {
        Self { kind: e.kind(), stage, detail: e.to_string() }
    }

    /// Синхронизация не запустилась (поток, рантайм, нет пути к серверу).
    pub fn internal(detail: impl std::fmt::Display) -> Self {
        Self { kind: ErrorKind::Internal, stage: Stage::Start, detail: detail.to_string() }
    }
}

/// Для журнала (по-английски); интерфейсу — `kind` и `detail`.
impl std::fmt::Display for SyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?} failed ({:?}): {}", self.stage, self.kind, self.detail)
    }
}

/// Для журнала и запасного интерфейса (по-английски).
impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Status::Connecting { url } => write!(f, "connecting to {url}"),
            Status::SignedIn { user } => write!(f, "signed in: {user}"),
            Status::Loading { mailbox } => write!(f, "loading {mailbox}"),
            Status::Synced => write!(f, "synced"),
        }
    }
}

/// Ход «Отправить и получить» ([`Command::SyncNow`], T3-02, класс B, решение владельца 2026-11-11): сверка учётной
/// записи началась и закончилась. Конец — всегда, в том числе когда на сервере ничего не изменилось; `error` —
/// почему не вышло (нет связи, сервер ответил ошибкой). Без связи до входа начало — у повторного входа, конец — когда
/// вход и первая загрузка прошли или не удались.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SendReceive {
    Started,
    Finished { error: Option<String> },
}

/// Отправитель для окна письма: свой адрес или общий ящик (S4-06).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sender {
    /// Общий ящик, из которого уходит письмо (id аккаунта, он же префикс ключей кэша); `None` — свой.
    pub account: Option<String>,
    pub address: Address,
    pub signature: String,
}

/// Почему состава списка рассылки нет (`Event::ListMembers`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ListError {
    /// Путь или сервер состав списков не отдаёт — показывать список без состава.
    Unsupported,
    /// Запрос не удался: текст — для журнала, по-английски.
    Failed(String),
}

/// Событие синхронизации интерфейсу: сериализуется (postcard), смысл договора — как в вариантах. Байты
/// (`BlobReady`, `Raw`) идут в `Arc` как байты; ключевые слова-статики — строкой (модуль [`keyword`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Event {
    Status(Status),
    MailboxesChanged,
    EmailsChanged,
    BodyReady(String),
    /// Беседа загружена: ключ цепочки и ключи её писем по времени (старые первыми), тела — в кэше.
    ThreadReady {
        thread: String,
        ids: Vec<String>,
    },
    BlobReady {
        blob_id: String,
        data: Arc<Vec<u8>>,
    },
    /// Тела загружены в фоне — их текст можно добавить в поиск.
    BodiesStored(Vec<String>),
    /// Догрузка ящика (`LoadMore`) пришла: `added` писем; 0 — на сервере старше нет.
    LoadedMore {
        mailbox: String,
        added: usize,
    },
    /// В календаре на сервере что-то изменилось (push, с задержкой на пачку) — открытую неделю перечитать.
    CalendarChanged,
    SearchResults {
        ticket: u64,
        ids: Vec<String>,
        total: u64,
    },
    /// Ответ на `Counts`: по числу на фильтр; `None` — запрос не удался.
    Counts {
        ticket: u64,
        counts: Option<Vec<u64>>,
    },
    BlobFailed {
        blob_id: String,
        error: String,
    },
    /// Учётная запись отправителя и подпись (из Identity).
    Me {
        address: Address,
        signature: String,
    },
    /// Что умеет сервер (ADR 0003, S7-03): интерфейс прячет то, чего нет. Приходит сразу после входа.
    Features(crate::model::Features),
    /// Адреса, от которых можно отправить письмо (S4-06): свои Identity и Identity общих ящиков.
    Senders(Vec<Sender>),
    /// Личные контакты (имя, адрес) — для автодополнения.
    Contacts(Vec<(String, String)>),
    /// Письмо отправлено; `scheduled` — сервер держит его до срока (окно «Отменить» или «Отправить позже»).
    Sent {
        ticket: u64,
        scheduled: Option<Scheduled>,
    },
    /// Отправка отменена, письмо — снова черновик в «Черновиках» (`email` — ключ кэша).
    Unsent {
        ticket: u64,
        email: String,
    },
    /// Отмена не удалась: `too_late` — письмо уже ушло.
    UnsendFailed {
        ticket: u64,
        too_late: bool,
        error: String,
    },
    /// Отправки, которые сервер держит, и сколько он позволяет задержать письмо (секунды, `send_later::max_delay`;
    /// 0 — отложенной отправки нет).
    Scheduled {
        list: Vec<Scheduled>,
        max_delay: u64,
    },
    DraftSaved {
        ticket: u64,
        id: String,
    },
    /// Ошибка конкретной отправки/сохранения — показывается в окне письма.
    ComposeFailed {
        ticket: u64,
        error: String,
    },
    InviteAnswered {
        uid: String,
        status: Partstat,
        delivery: Delivery,
    },
    InviteFailed {
        uid: String,
        error: String,
    },
    /// Ответ на `RemoveInvite`: `found` — копия была и удалена.
    InviteRemoved {
        uid: String,
        found: bool,
    },
    /// События календаря в `[from, to)` — без отменённых, по времени начала.
    /// События недели: мои и календарей коллег (`Event::owner`, S5-22); коллеги — все, кто открыл мне календарь.
    /// `account` — учётная запись профиля, чьи это события (S7-03б): у дополнительной её события — с владельцем, как
    /// календарь коллеги; ответы учётных записей за тот же период складываются.
    EventsLoaded {
        from: i64,
        to: i64,
        events: Vec<postman_calendar::Event>,
        colleagues: Vec<postman_calendar::Owner>,
        account: Option<String>,
    },
    /// Найдено в календаре по тексту `text` (S5-21); `error` — запрос не удался.
    EventsFound {
        text: String,
        events: Vec<postman_calendar::Event>,
        error: Option<String>,
    },
    /// Занятость для помощника: моя и по адресам; `None` — нет данных (календарь не открыт мне, сотрудник
    /// не найден): это не «свободен». `error` — запрос не удался целиком.
    Availability {
        ticket: u64,
        me: Vec<Busy>,
        people: Vec<(String, Option<Vec<Busy>>)>,
        /// Тема и место интервалов по адресам, где сервер их дал (T2-02б; сейчас — только Exchange).
        notes: Vec<(String, Vec<BusyNote>)>,
        error: Option<String>,
    },
    MeetingCreated {
        ticket: u64,
        uid: String,
    },
    /// Встреча изменена, изменения разосланы (`UpdateMeeting`).
    MeetingUpdated {
        ticket: u64,
        uid: String,
    },
    /// Встреча отменена, участникам ушла отмена (`CancelMeeting`).
    MeetingCancelled {
        ticket: u64,
        uid: String,
    },
    /// Ошибка создания, правки или отмены встречи с этим тикетом.
    MeetingFailed {
        ticket: u64,
        error: String,
    },
    /// Кому открыт мой календарь (S8-43).
    CalendarAccess(Vec<Access>),
    /// Ссылки публикации (S8-45); `None` — сервер публикацию не умеет.
    Publications(Option<Vec<Publication>>),
    /// Создана ссылка публикации: адрес показывается один раз.
    Published {
        url: String,
    },
    FreeBusyShared {
        email: String,
    },
    /// Ошибка календаря (неделя, открытие занятости) — тостом.
    CalendarFailed(String),
    /// Ответ по настройкам ящика (`settings.rs`).
    Settings(settings::Reply),
    /// Личные контакты целиком (раздел «Контакты»).
    ContactsLoaded(Vec<ContactCard>),
    /// Адресные книги сервера (T2-48, ответ на `Command::Contacts`): выбор книги в форме нового контакта
    /// (`Command::SaveContact`). Пустая — книг нет или путь их не даёт: пишет в книгу по умолчанию.
    ContactBooks(Vec<ContactBook>),
    /// Справочник: сотрудники, переговорные и ресурсы с адресами (раздел «Контакты», подсказки адресов).
    DirectoryLoaded(Vec<Principal>),
    /// Страница справочника (`Command::DirectoryPage`, T1-25б): `people` с `offset`; `total` — всего людей, если путь
    /// знает; `search_only` — справочник у сервера только поиском (`FindPeople`), страниц не будет. Пути без
    /// справочника — пустая страница ([`Event::no_directory`]).
    DirectoryPage {
        account: Option<String>,
        offset: u32,
        people: Vec<Principal>,
        total: Option<u32>,
        search_only: bool,
    },
    /// Найдено в справочнике по строке `FindPeople` — к уже загруженному (подсказки адресов, поиск в «Контактах»).
    /// `offset` — как в команде; `more` — у сервера есть ещё найденные (следующая страница — `offset` + сколько пришло).
    PeopleFound {
        text: String,
        offset: u32,
        people: Vec<Principal>,
        more: bool,
    },
    /// Подробности человека справочника (`PersonDetails`): `None` — путь их не даёт или человек не найден.
    PersonDetails {
        email: String,
        person: Option<Principal>,
    },
    /// Состав списка рассылки (`ExpandList`, T3-09), в порядке сервера: число участников — `members.len()`, вложенный
    /// список — участник с `kind == "group"` (не раскрыт). `error` — состава нет, `members` пуст.
    ListMembers {
        ticket: u64,
        address: String,
        members: Vec<Principal>,
        error: Option<ListError>,
    },
    ContactSaved {
        ticket: u64,
        id: String,
    },
    ContactDeleted {
        ticket: u64,
        id: String,
    },
    /// Ошибка контактов: `ticket` — сохранения или удаления, `0` — загрузки.
    ContactFailed {
        ticket: u64,
        error: String,
    },
    /// Таблица категорий: прочитана при входе или по push, записана, снята с писем (`categories.rs`).
    Categories(categories::Reply),
    /// Задачи: из кэша и с сервера, правки (`tasks.rs`, S6-02).
    Tasks(tasks::Reply),
    /// Рассылки перенесены в «Архив» (`Command::Newsletters`): сколько.
    Newsletters {
        archived: usize,
    },
    /// Связь с сервером (S5-08, `link`): пропала (повтор через N с, правок в очереди) или вернулась.
    Connection(Connection),
    /// «Отправить и получить» (`Command::SyncNow`, T3-02): сверка учётной записи началась или закончилась.
    SendReceive(SendReceive),
    /// Сообщение о фишинге (`Command::ReportPhishing`) отправлено (`error: None`) или не отправлено. Без связи
    /// события нет — команда ждёт в очереди.
    Reported {
        ticket: u64,
        error: Option<String>,
    },
    /// Шаг поручения (`Command::Delegate`) ушёл (`error: None`) или нет. Без связи события нет — ждёт в очереди.
    /// `scheduled` — письма, которые сервер держит (окно «Отменить»).
    Delegated {
        ticket: u64,
        uid: String,
        error: Option<String>,
        scheduled: Vec<Scheduled>,
    },
    /// Письма шага не ушли (`Command::Withhold`); `too_late` — хотя бы одно уже отправлено.
    Withheld {
        ticket: u64,
        too_late: bool,
        error: Option<String>,
    },
    /// Письмо целиком (`Command::FetchRaw`); ошибка — ответ сервера или сети.
    Raw {
        letter: LetterId,
        data: std::result::Result<Arc<Vec<u8>>, String>,
    },
    /// Сервер цели принял письмо задания (`Command::Import`, `Command::ServerCopy`): `remote` — где оно у него.
    Imported {
        job: JobId,
        letter: LetterId,
        remote: Remote,
    },
    /// Сервер цели письмо не принял.
    ImportFailed {
        job: JobId,
        letter: LetterId,
        error: ImportError,
    },
    /// Сервер источника удалил письмо задания (`Command::DestroyLetters`) или его там уже нет.
    Destroyed {
        job: JobId,
        letter: LetterId,
    },
    /// Сервер источника письмо не удалил: ответ сервера или сети как есть. Задание повторит (не больше
    /// [`crate::migrate::DESTROY_ATTEMPTS`] раз).
    DestroyFailed {
        job: JobId,
        letter: LetterId,
        error: String,
    },
    /// Итог задания переноса: `letters` писем источник так и не удалил (все попытки — отказ). Они у цели (или в базе)
    /// с пометкой «не удалено у источника» — его `detached`-связью; интерфейс показывает тост с числом.
    SourceKept {
        job: JobId,
        letters: u64,
    },
    /// Обновление хранилища (перенос формата 5 в 6, ADR 0006 п. 6): писем перенесено из `total` — строка
    /// «Обновление хранилища: N %» вместо списка; первый кадр её не ждёт.
    StoreUpgrade {
        done: u64,
        total: u64,
    },
    /// Страница ключей выборки (`Command::ListLetters`): `total` — всего писем по условию у сервера, `end` — страниц
    /// больше нет.
    LettersListed {
        sweep: SweepId,
        ids: Vec<String>,
        total: u64,
        end: bool,
    },
    /// Строки писем в кэше (`Command::StoreLetters`); `gone` — ключи писем, которых на сервере уже нет.
    LettersStored {
        sweep: SweepId,
        gone: Vec<String>,
    },
    /// Страница не получена: ответ сервера или сети как есть. [`FOLDER_UNSUPPORTED`] — путь не умеет, повторять незачем.
    ListFailed {
        sweep: SweepId,
        error: String,
    },
    /// Ход действия над выборкой — не чаще 4 раз в секунду, смена состояния — сразу.
    Sweep(sweep::Progress),
    /// Ход задания переноса: писем готово из `total`, байт выгружено, состояние.
    Migration {
        job: JobId,
        done: u64,
        total: u64,
        bytes: u64,
        state: MigrationState,
    },
    /// Ход экспорта (`Command::Export`) — не чаще 4 раз в секунду, смена состояния — сразу: писем записано из
    /// `total`, байт записано, состояние (конец — последнее событие задания).
    Export {
        job: export::ExportId,
        done: u64,
        total: u64,
        bytes: u64,
        state: export::State,
    },
    /// Папку не создать, не переименовать или не перенести (T1-04б). `mailbox` — папка команды, у `CreateFolder` —
    /// родитель (`None` — верхний уровень); `error` — ответ сервера или отказ ядра ([`FOLDER_UNSUPPORTED`],
    /// [`FOLDER_INTO_ITSELF`], [`FOLDER_OTHER_ACCOUNT`]: текст у интерфейса).
    FolderFailed {
        mailbox: Option<String>,
        error: String,
    },
    Error(SyncError),
    /// Ответ потока учётки канала бесед (ADR 0012 п. 6): чей и что.
    Channel(crate::channel::ChanAccount, crate::channel::Update),
}

/// Путь не умеет команду ([`Command::unsupported`]): над папками, письмо целиком, выгрузку.
pub const FOLDER_UNSUPPORTED: &str = "not supported";
/// Перенос папки в саму себя или в свою подпапку ([`admit`]).
pub const FOLDER_INTO_ITSELF: &str = "folder cannot be moved into itself or its subfolder";
/// Перенос папки в другой аккаунт ([`admit`]): у сервера это не перенос, а копия.
pub const FOLDER_OTHER_ACCOUNT: &str = "folder cannot be moved to another account";

impl Command {
    /// Ответ пути, который команду не умеет, — чтобы интерфейс и задание переноса не ждали: у команд над папками —
    /// `FolderFailed` «not supported», у страницы справочника — пустая страница, у письма целиком, выгрузки и
    /// удаления у источника — отказ по письму. У прочих — пусто (путь пишет в журнал).
    pub fn unsupported(&self) -> Vec<Event> {
        let not = || FOLDER_UNSUPPORTED.to_owned();
        let failed = |job: JobId, letter: LetterId| Event::ImportFailed { job, letter, error: ImportError::Rejected(not()) };
        let mailbox = match self {
            Command::CreateFolder { parent, .. } => parent.clone(),
            Command::RenameFolder { mailbox, .. } | Command::MoveFolder { mailbox, .. } => Some(mailbox.clone()),
            Command::DirectoryPage { account, offset, .. } => return vec![Event::no_directory(account.clone(), *offset)],
            Command::FetchRaw { letter } => return vec![Event::Raw { letter: *letter, data: Err(not()) }],
            Command::Import { job, letter, .. } => return vec![failed(*job, *letter)],
            Command::ServerCopy { job, letters, .. } => return letters.iter().map(|l| failed(*job, *l)).collect(),
            Command::DestroyLetters { job, letters, .. } => {
                return letters.iter().map(|l| Event::DestroyFailed { job: *job, letter: *l, error: not() }).collect();
            }
            Command::ListLetters { sweep, .. } | Command::StoreLetters { sweep, .. } => {
                return vec![Event::ListFailed { sweep: *sweep, error: not() }];
            }
            Command::FindMessage { ticket, .. } => return vec![Event::SearchResults { ticket: *ticket, ids: Vec::new(), total: 0 }],
            _ => return Vec::new(),
        };
        vec![Event::FolderFailed { mailbox, error: not() }]
    }
}

impl Event {
    /// Ответ на `DirectoryPage` срезом справочника, загруженного целиком (JMAP: тот же список, что в
    /// `DirectoryLoaded`): `max` людей с `offset`, `total` — длина списка.
    pub fn directory_slice(account: Option<String>, all: &[Principal], offset: u32, max: u32) -> Event {
        let people = all.iter().skip(offset as usize).take(max as usize).cloned().collect();
        let total = Some(u32::try_from(all.len()).unwrap_or(u32::MAX));
        Event::DirectoryPage { account, offset, people, total, search_only: false }
    }

    /// Ответ пути без справочника: пустая страница, больше нет.
    pub fn no_directory(account: Option<String>, offset: u32) -> Event {
        Event::DirectoryPage { account, offset, people: Vec::new(), total: Some(0), search_only: false }
    }
}

/// Пропустить ли команду в плагин (T1-04б): перенос папки в саму себя, в свою подпапку или в другой аккаунт ядро
/// отклоняет само: `FolderFailed` — в `notify`, плагину — `None`. `folders` — дерево папок кэша (`Cache::mailboxes`),
/// читается только для переноса папки. Плагин: `let Some(cmd) = admit(cmd, || …, notify) else { continue };`.
pub fn admit(cmd: Command, folders: impl FnOnce() -> Vec<MailboxRow>, notify: &dyn Fn(Event)) -> Option<Command> {
    let Command::MoveFolder { mailbox, parent: Some(parent) } = &cmd else { return Some(cmd) };
    let error = if postman_cache::account_of(mailbox) != postman_cache::account_of(parent) {
        FOLDER_OTHER_ACCOUNT
    } else if is_within(parent, mailbox, &folders()) {
        FOLDER_INTO_ITSELF
    } else {
        return Some(cmd);
    };
    notify(Event::FolderFailed { mailbox: Some(mailbox.clone()), error: error.into() });
    None
}

/// Папка `key` — это `root` или её подпапка (вверх по `parent_id`; цикл в дереве не зацикливает).
fn is_within(key: &str, root: &str, folders: &[MailboxRow]) -> bool {
    let parent: HashMap<&str, &str> = folders.iter().filter_map(|m| Some((m.id.as_str(), m.parent_id.as_deref()?))).collect();
    let mut at = key;
    for _ in 0..=parent.len() {
        if at == root {
            return true;
        }
        match parent.get(at) {
            Some(up) => at = up,
            None => return false,
        }
    }
    false
}

/// Поисковый индекс (если открылся); пишут в него и синхронизация, и UI.
pub type Index = Option<Arc<SearchIndex>>;

// ---------------------------------------------------------------------------------------------------------------
// Общий цикл: поток учётной записи и «без связи» (ADR 0004). Плагин пути даёт свою сессию с сервером.

/// Куда поток синхронизации отдаёт события: канал интерфейса и пробуждение его цикла.
pub struct Notifier {
    events: mpsc::Sender<Event>,
    repaint: Box<dyn Fn() + Send>,
}

impl Notifier {
    pub fn new(events: mpsc::Sender<Event>, repaint: Box<dyn Fn() + Send>) -> Self {
        Self { events, repaint }
    }

    pub fn send(&self, e: Event) {
        let _ = self.events.send(e);
        (self.repaint)();
    }
}

/// Поток учётки канала бесед (ADR 0012 п. 2) с ответами в общий канал событий: `channel::Update` —
/// [`Event::Channel`] этой учётки, затем пробуждение интерфейса. Команды — `Command::Channel` с той же учёткой.
pub fn spawn_channel(
    plugin: Arc<dyn crate::channel::ChannelPlugin>,
    start: crate::channel::Start,
    events: mpsc::Sender<Event>,
    repaint: Box<dyn Fn() + Send>,
) -> UnboundedSender<crate::channel::Request> {
    let account = crate::channel::ChanAccount::new(plugin.id(), start.account.0.clone());
    let notify = Notifier::new(events, repaint);
    crate::channel::spawn(plugin, start, Box::new(move |u| notify.send(Event::Channel(account.clone(), u))))
}

/// Поток синхронизации учётной записи: свой рантайм tokio (один поток) и `body` в нём. Поток или рантайм не
/// запустились — ошибка видна в окне (как любая ошибка синхронизации); команды уходят в никуда, клиент работает с
/// кэшем.
pub fn spawn_thread<F, Fut>(name: &str, events: mpsc::Sender<Event>, repaint: Box<dyn Fn() + Send>, body: F)
where
    F: FnOnce(Notifier) -> Fut + Send + 'static,
    Fut: Future<Output = ()>,
{
    let failed = events.clone();
    let started = std::thread::Builder::new().name(name.into()).spawn(move || {
        let notify = Notifier::new(events, repaint);
        let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(rt) => rt,
            Err(e) => return notify.send(Event::Error(SyncError::internal(format!("tokio runtime: {e}")))),
        };
        rt.block_on(body(notify));
    });
    if let Err(e) = started {
        let _ = failed.send(Event::Error(SyncError::internal(format!("sync thread: {e}"))));
    }
}

/// Сессия с сервером одной учётной записи (плагин пути): вход и работа до конца канала команд или до ошибки.
/// Правки без связи — в `link` (их повторит сама сессия, когда связь вернётся).
pub trait Session {
    fn run<'a>(
        &'a mut self,
        rx: &'a mut UnboundedReceiver<Command>,
        link: &'a mut Link<Command>,
        notify: &'a dyn Fn(Event),
    ) -> BoxFuture<'a, Result<(), SyncError>>;
}

/// Общий цикл учётной записи: сессия; сервер не ответил при входе или первой загрузке — без связи, повтор с
/// паузой (S5-08), правки писем — в очередь (уйдут после входа). Отказ во входе и прочие ошибки останавливают
/// синхронизацию.
pub async fn supervise(session: &mut impl Session, mut rx: UnboundedReceiver<Command>, notify: &dyn Fn(Event)) {
    let mut link = Link::default();
    // «Отправить и получить» без связи (T3-02): повторный вход идёт по `SyncNow` — конец хода — первая загрузка
    // (`Status::Synced`) или новая ошибка входа.
    let asked = std::cell::Cell::new(false);
    let finish = |error: Option<String>| {
        if asked.replace(false) {
            notify(Event::SendReceive(SendReceive::Finished { error }));
        }
    };
    let watch = |e: Event| {
        let synced = matches!(e, Event::Status(Status::Synced));
        notify(e);
        if synced {
            finish(None);
        }
    };
    loop {
        match session.run(&mut rx, &mut link, &watch).await {
            Ok(()) => return finish(None),
            Err(e) if e.kind == ErrorKind::Network => {
                finish(Some(e.to_string()));
                notify(Event::Error(e));
                let wait = link.failed();
                notify(Event::Connection(link.connection(wait)));
                match wait_offline(&mut rx, &mut link, wait, notify).await {
                    Woke::Closed => return,
                    Woke::Timer => {}
                    Woke::SendReceive => {
                        notify(Event::SendReceive(SendReceive::Started));
                        asked.set(true);
                    }
                }
            }
            Err(e) => {
                finish(Some(e.to_string()));
                return notify(Event::Error(e));
            }
        }
    }
}

/// Чем кончилось ожидание без связи.
enum Woke {
    /// Интерфейс закрыл канал команд: синхронизации конец.
    Closed,
    /// Пауза прошла или «Повторить сейчас».
    Timer,
    /// «Отправить и получить» (`Command::SyncNow`): повтор сейчас, с ходом [`Event::SendReceive`].
    SendReceive,
}

/// Без связи до входа: ждать паузу `wait` (или «Повторить сейчас»); правки писем — в очередь (уйдут после входа),
/// прочие команды без сервера выполнить нельзя.
async fn wait_offline(rx: &mut UnboundedReceiver<Command>, link: &mut Link<Command>, wait: Duration, notify: &dyn Fn(Event)) -> Woke {
    let until = tokio::time::Instant::now() + wait;
    loop {
        tokio::select! {
            () = tokio::time::sleep_until(until) => return Woke::Timer,
            cmd = rx.recv() => match cmd {
                None => return Woke::Closed,
                Some(Command::Reconnect) => return Woke::Timer,
                Some(Command::SyncNow) => return Woke::SendReceive,
                Some(cmd) => {
                    if let Some(c) = cmd.replayable() {
                        link.queue(c);
                        let left = until.saturating_duration_since(tokio::time::Instant::now());
                        notify(Event::Connection(link.connection(left)));
                    }
                    // Страница выборки ждёт ответа: без него выборка встала бы молча.
                    if let Command::ListLetters { sweep, .. } | Command::StoreLetters { sweep, .. } = cmd {
                        notify(Event::ListFailed { sweep, error: "no connection".into() });
                    }
                }
            },
        }
    }
}

/// Связь пропала (или повтор не удался): пауза до следующей попытки — интерфейсу и циклу.
pub fn went_down(link: &mut Link<Command>, notify: &impl Fn(Event)) -> tokio::time::Instant {
    let wait = link.failed();
    notify(Event::Connection(link.connection(wait)));
    tokio::time::Instant::now() + wait
}

/// Ждать срока повтора; срока нет — никогда.
pub async fn retry_at(at: Option<tokio::time::Instant>) {
    match at {
        Some(t) => tokio::time::sleep_until(t).await,
        None => std::future::pending().await,
    }
}

/// Ошибку — в интерфейс; `true` — остановить синхронизацию: сервер отклонил вход — повторять запросы с тем же
/// паролем бессмысленно (и сервер может заблокировать адрес за перебор). Интерфейс показывает строку входа.
pub fn report(e: SyncError, notify: &impl Fn(Event)) -> bool {
    let stop = e.kind == ErrorKind::Auth;
    notify(Event::Error(e));
    stop
}

/// Задержка перед перечитыванием календаря: первое изменение пачки ставит срок, следующие до срока — к нему же.
#[derive(Debug, Default)]
pub struct Debounce {
    due: Option<tokio::time::Instant>,
}

impl Debounce {
    pub fn poke(&mut self, now: tokio::time::Instant, delay: Duration) {
        self.due.get_or_insert(now + delay);
    }

    /// Срок наступил: сбросить и вернуть `true`.
    pub fn fire(&mut self, now: tokio::time::Instant) -> bool {
        let due = self.due.is_some_and(|d| d <= now);
        if due {
            self.due = None;
        }
        due
    }

    pub async fn wait(&self) {
        match self.due {
            Some(d) => tokio::time::sleep_until(d).await,
            None => std::future::pending().await,
        }
    }
}

/// События дополнительной учётной записи (S7-03б) — с владельцем: в общем календаре они рисуются цветом и подписью
/// владельца, как календари коллег (S5-22). Основная (`me: None`) — как есть.
pub fn owned_by(
    me: &Option<postman_calendar::Owner>,
    mut events: Vec<postman_calendar::Event>,
    mut colleagues: Vec<postman_calendar::Owner>,
) -> (Vec<postman_calendar::Event>, Vec<postman_calendar::Owner>) {
    if let Some(me) = me {
        for e in events.iter_mut().filter(|e| e.owner.is_none()) {
            e.owner = Some(me.clone());
        }
        if !colleagues.iter().any(|c| c.key() == me.key()) {
            colleagues.insert(0, me.clone());
        }
    }
    (events, colleagues)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Несколько учётных записей (S7-03б): команда идёт в синхронизацию учётной записи своих ключей, правки писем
    /// разных учётных записей делятся, поиск «Все папки» и неделя календаря — во все.
    #[test]
    fn commands_route_by_profile() {
        let target = |c: Command| c.routes().into_iter().map(|(t, _)| t).collect::<Vec<_>>();
        let p = |s: Option<&str>| Target::Profile(s.map(str::to_owned));
        assert_eq!(target(Command::Body("e1".into())), [p(None)]);
        assert_eq!(target(Command::Body("shared/e1".into())), [p(None)]);
        assert_eq!(target(Command::Body("@2/e1".into())), [p(Some("@2"))]);
        assert_eq!(target(Command::Body("@2.shared/e1".into())), [p(Some("@2"))]);
        assert_eq!(target(Command::Move { id: "@3/e".into(), from: "@3/a".into(), to: "@3/b".into() }), [p(Some("@3"))]);
        assert_eq!(target(Command::Search { query: "q".into(), mailbox: None, ticket: 1 }), [Target::All]);
        let find = Command::FindMessage { message_id: "m1@example.test".into(), ticket: 2 };
        assert_eq!(target(Command::FindMessage { message_id: "m1@example.test".into(), ticket: 2 }), [Target::All]);
        assert!(matches!(find.for_each_account(), Some(Command::FindMessage { ticket: 2, .. })), "во все учётки профиля");
        assert_eq!(target(Command::Search { query: "q".into(), mailbox: Some("@2/inbox".into()), ticket: 1 }), [p(Some("@2"))]);
        assert_eq!(target(Command::Events { from: 0, to: 1 }), [Target::All]);
        assert_eq!(target(Command::Contacts), [p(None)]);
        let draft = Draft { from: Address::new("", "me@b.test"), ..Default::default() };
        let send = Command::Send { draft: Box::new(draft), replace: None, ticket: 1, release: Release::Now };
        assert_eq!(target(send), [Target::Sender("me@b.test".into())]);
        let split = Command::SetKeywords { ids: vec!["a".into(), "@2/b".into(), "s/c".into()], add: vec!["k".into()], remove: vec![] }.routes();
        let got: Vec<(Target, Vec<String>)> = split
            .into_iter()
            .map(|(t, c)| match c {
                Command::SetKeywords { ids, .. } => (t, ids),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(got, [(p(None), vec!["a".to_string(), "s/c".to_string()]), (p(Some("@2")), vec!["@2/b".to_string()])]);
        assert!(Command::Events { from: 0, to: 1 }.for_each_account().is_some());
        assert!(Command::Contacts.for_each_account().is_none());
    }

    /// Команды над папками (T1-04б) — в учётную запись папки (у новой — её аккаунта), основную и дополнительную.
    #[test]
    fn folder_commands_route_by_folder() {
        let target = |c: Command| c.routes().into_iter().map(|(t, _)| t).collect::<Vec<_>>();
        let p = |s: Option<&str>| Target::Profile(s.map(str::to_owned));
        let create = |account: Option<&str>, parent: Option<&str>| Command::CreateFolder {
            account: account.map(str::to_owned),
            parent: parent.map(str::to_owned),
            name: "Новая".into(),
        };
        assert_eq!(target(create(None, None)), [p(None)]);
        assert_eq!(target(create(Some("shared"), Some("shared/f"))), [p(None)], "общий ящик основной учётной записи");
        assert_eq!(target(create(Some("@2"), None)), [p(Some("@2"))]);
        assert_eq!(target(create(Some("@2.shared"), Some("@2.shared/f"))), [p(Some("@2"))]);
        assert_eq!(target(Command::RenameFolder { mailbox: "f".into(), name: "x".into() }), [p(None)]);
        assert_eq!(target(Command::RenameFolder { mailbox: "@3/f".into(), name: "x".into() }), [p(Some("@3"))]);
        assert_eq!(target(Command::MoveFolder { mailbox: "f".into(), parent: None }), [p(None)]);
        assert_eq!(target(Command::MoveFolder { mailbox: "@2/f".into(), parent: Some("@2/g".into()) }), [p(Some("@2"))]);
        assert!(Command::MoveFolder { mailbox: "f".into(), parent: None }.for_each_account().is_none());
        // Путь без папок отвечает отказом, команда не теряется.
        let no = Command::RenameFolder { mailbox: "@2/f".into(), name: "x".into() }.unsupported();
        assert!(matches!(&no[..], [Event::FolderFailed { mailbox: Some(m), error }] if m == "@2/f" && error == FOLDER_UNSUPPORTED));
        assert!(matches!(&create(None, None).unsupported()[..], [Event::FolderFailed { mailbox: None, .. }]));
        assert!(Command::Contacts.unsupported().is_empty());
        let find = Command::FindMessage { message_id: "m1@example.test".into(), ticket: 7 }.unsupported();
        assert!(matches!(&find[..], [Event::SearchResults { ticket: 7, ids, total: 0 }] if ids.is_empty()), "путь без поиска — пустой ответ");
    }

    /// Перенос папки в саму себя, в подпапку или в другой аккаунт — отказ ядра, плагин команды не получает; прочее —
    /// в плагин (дерево папок для прочих команд не читается).
    #[test]
    fn folder_into_its_subfolder_is_refused_before_the_plugin() {
        let row = |id: &str, parent: Option<&str>| MailboxRow {
            id: id.into(),
            parent_id: parent.map(str::to_owned),
            name: id.into(),
            role: None,
            sort_order: 0,
            total: 0,
            unread: 0,
            rights: Default::default(),
        };
        // a → b → c; d — сосед; x ↔ y — цикл в дереве.
        let tree = vec![row("a", None), row("b", Some("a")), row("c", Some("b")), row("d", None), row("x", Some("y")), row("y", Some("x"))];
        let mv = |m: &str, to: Option<&str>| Command::MoveFolder { mailbox: m.into(), parent: to.map(str::to_owned) };
        let mut plugin = Vec::new();
        let refused = std::cell::RefCell::new(Vec::new());
        let notify = |e: Event| match e {
            Event::FolderFailed { mailbox, error } => refused.borrow_mut().push((mailbox.unwrap_or_default(), error)),
            _ => unreachable!(),
        };
        for cmd in
            [mv("a", Some("c")), mv("a", Some("a")), mv("b", Some("c")), mv("a", Some("@2/z")), mv("c", Some("d")), mv("c", None), mv("x", Some("d"))]
        {
            match admit(cmd, || tree.clone(), &notify) {
                Some(Command::MoveFolder { mailbox, parent }) => plugin.push((mailbox, parent)),
                Some(_) => unreachable!(),
                None => {}
            }
        }
        let into = FOLDER_INTO_ITSELF.to_string();
        assert_eq!(
            refused.take(),
            [("a".into(), into.clone()), ("a".into(), into.clone()), ("b".into(), into), ("a".into(), FOLDER_OTHER_ACCOUNT.to_string())]
        );
        assert_eq!(plugin, [("c".to_string(), Some("d".to_string())), ("c".into(), None), ("x".into(), Some("d".into()))]);
        assert!(admit(mv("y", Some("x")), || tree.clone(), &notify).is_none(), "цикл: x — подпапка y");
        assert_eq!(refused.take().len(), 1);
        let read = std::cell::Cell::new(false);
        let body = admit(
            Command::Body("e".into()),
            || {
                read.set(true);
                Vec::new()
            },
            &notify,
        );
        assert!(matches!(body, Some(Command::Body(_))) && !read.get());
    }

    /// Письма формата 6 (S8-08): письмо — по учётке его связи с сервером, «На этом компьютере» — в ядро; выгрузка — в
    /// поток учётки цели; перенос и задания — в ядро; прочие команды — по ключам, как раньше.
    #[test]
    fn letters_route_by_their_account() {
        struct Table;
        impl Homes for Table {
            fn letter(&self, id: LetterId) -> Home {
                match id.0 {
                    1 => Home::Local,
                    2 => Home::Account("@2".into()),
                    3 => Home::Account(String::new()),
                    _ => Home::Account("@2.shared".into()),
                }
            }
            fn folder(&self, id: FolderId) -> Home {
                match id.0 {
                    10 => Home::Local,
                    _ => Home::Account("@3".into()),
                }
            }
        }
        let route = |c: Command| c.routes_by(&Table).into_iter().map(|(r, _)| r).collect::<Vec<_>>();
        let sync = |s: Option<&str>| Route::Sync(Target::Profile(s.map(str::to_owned)));
        let raw = |n: u64| Command::FetchRaw { letter: LetterId(n) };
        assert_eq!(route(raw(1)), [Route::Core], "письмо «На этом компьютере» — без сети");
        // Учётка канала бесед — своим потоком (ADR 0012 п. 6), не синхронизацией почты.
        let chan = crate::channel::ChanAccount::new("demo", "me@example.test");
        let to_chan = || Command::Channel { account: chan.clone(), request: Box::new(crate::channel::Request::Reconnect) };
        assert_eq!(route(to_chan()), [Route::Channel(chan.clone())]);
        assert!(to_chan().routes().is_empty());
        assert_eq!(route(raw(2)), [sync(Some("@2"))]);
        assert_eq!(route(raw(3)), [sync(None)], "основная учётка");
        assert_eq!(route(raw(4)), [sync(Some("@2"))], "общий ящик учётки @2");
        let import = |folder: u64| Command::Import {
            job: JobId(1),
            letter: LetterId(2),
            folder: FolderId(folder),
            keywords: vec![],
            received_at: 0,
            message_id: None,
            retry: false,
        };
        assert_eq!(route(import(11)), [sync(Some("@3"))], "выгрузка — в поток цели, а не источника");
        let copy = Command::ServerCopy { job: JobId(1), letters: vec![LetterId(2), LetterId(3)], to: FolderId(11) };
        assert_eq!(route(copy), [sync(Some("@3"))]);
        assert_eq!(route(Command::MoveLetters { ids: vec![LetterId(2)], to: FolderId(11) }), [Route::Core]);
        assert_eq!(route(Command::MoveLetters { ids: vec![LetterId(1)], to: FolderId(10) }), [Route::Core]);
        assert_eq!(route(Command::Migration(MigrationRequest::Pause { job: JobId(1) })), [Route::Core]);
        let export =
            Command::Export { scope: export::Scope::Folder { id: FolderId(11), subfolders: true }, format: export::Format::Mbox, dir: "x".into() };
        assert_eq!(route(export), [Route::Core], "экспорт — в поток ядра, и папки учётки тоже");
        assert_eq!(route(Command::StopExport { job: export::ExportId(1) }), [Route::Core]);
        let destroy = |account: &str| Command::DestroyLetters { job: JobId(1), account: account.into(), letters: vec![LetterId(1)] };
        assert_eq!(route(destroy("@2.shared")), [sync(Some("@2"))], "удаление — у источника, письмо уже в другой учётке");
        assert_eq!(route(destroy("")), [sync(None)]);
        assert_eq!(route(Command::Body("@2/e1".into())), [sync(Some("@2"))], "прежние команды — по ключам");
        assert_eq!(route(Command::Search { query: "q".into(), mailbox: None, ticket: 1 }), [Route::Sync(Target::All)]);
        let split = Command::SetKeywords { ids: vec!["a".into(), "@2/b".into()], add: vec![], remove: vec![] };
        assert_eq!(route(split), [sync(None), sync(Some("@2"))]);
        for c in [raw(2), import(11), destroy("@2"), Command::Migration(MigrationRequest::Resume { job: JobId(1) })] {
            assert!(c.replayable().is_none() && c.for_each_account().is_none(), "задание переноса повторяет само");
        }
    }

    /// Путь без выгрузки отвечает отказом по каждому письму: задание не ждёт.
    #[test]
    fn unsupported_transfer_answers_per_letter() {
        let failed = |events: Vec<Event>| {
            events
                .into_iter()
                .map(|e| match e {
                    Event::ImportFailed { job, letter, error: ImportError::Rejected(r) } if r == FOLDER_UNSUPPORTED => (job.0, letter.0),
                    _ => unreachable!(),
                })
                .collect::<Vec<_>>()
        };
        let import = Command::Import {
            job: JobId(5),
            letter: LetterId(2),
            folder: FolderId(9),
            keywords: vec![],
            received_at: 0,
            message_id: None,
            retry: false,
        };
        assert_eq!(failed(import.unsupported()), [(5, 2)]);
        let copy = Command::ServerCopy { job: JobId(5), letters: vec![LetterId(2), LetterId(3)], to: FolderId(9) };
        assert_eq!(failed(copy.unsupported()), [(5, 2), (5, 3)]);
        let raw = Command::FetchRaw { letter: LetterId(4) }.unsupported();
        assert!(matches!(&raw[..], [Event::Raw { letter: LetterId(4), data: Err(_) }]));
        let destroy = Command::DestroyLetters { job: JobId(5), account: "@2".into(), letters: vec![LetterId(6), LetterId(7)] }.unsupported();
        assert!(matches!(&destroy[..], [Event::DestroyFailed { letter: LetterId(6), .. }, Event::DestroyFailed { letter: LetterId(7), .. }]));
        assert!(Command::MoveLetters { ids: vec![LetterId(1)], to: FolderId(1) }.unsupported().is_empty());
    }

    /// Справочник по страницам (T1-25б): команда — в свою учётную запись; JMAP отвечает срезом загруженного, путь без
    /// справочника — пустой страницей.
    #[test]
    fn directory_pages_route_and_slice() {
        let target = |c: Command| c.routes().into_iter().map(|(t, _)| t).collect::<Vec<_>>();
        let page = |account: Option<&str>| Command::DirectoryPage { account: account.map(str::to_owned), offset: 0, max: 100 };
        assert_eq!(target(page(None)), [Target::Profile(None)]);
        assert_eq!(target(page(Some("@2"))), [Target::Profile(Some("@2".into()))]);
        assert!(page(None).for_each_account().is_none());
        let all: Vec<Principal> = (0..5).map(|n| Principal { id: n.to_string(), email: Some(format!("p{n}@x.ru")), ..Default::default() }).collect();
        let ids = |e: Event| match e {
            Event::DirectoryPage { offset, people, total, search_only, .. } => {
                (offset, people.into_iter().map(|p| p.id).collect::<Vec<_>>(), total, search_only)
            }
            _ => unreachable!(),
        };
        assert_eq!(ids(Event::directory_slice(None, &all, 0, 2)), (0, vec!["0".into(), "1".into()], Some(5), false));
        assert_eq!(ids(Event::directory_slice(None, &all, 4, 2)), (4, vec!["4".into()], Some(5), false));
        assert_eq!(ids(Event::directory_slice(None, &all, 9, 2)), (9, vec![], Some(5), false));
        let none = page(Some("@2")).unsupported().pop().unwrap();
        assert!(matches!(&none, Event::DirectoryPage { account: Some(a), .. } if a == "@2"));
        assert_eq!(ids(none), (0, vec![], Some(0), false));
    }

    /// События дополнительной учётной записи — с её владельцем, как календарь коллеги; основной — как есть.
    #[test]
    fn second_account_events_have_an_owner() {
        let me = postman_calendar::Owner { email: "me@b.test".into(), name: "Я".into() };
        let raw = serde_json::json!({ "id": "e", "uid": "u", "title": "t", "start": "2026-10-23T10:00:00", "duration": "PT1H" });
        let ev = postman_calendar::Event::from_jmap(&raw, &jiff::tz::TimeZone::UTC).unwrap();
        let (events, colleagues) = owned_by(&Some(me.clone()), vec![ev.clone()], Vec::new());
        assert_eq!(events[0].owner.as_ref().map(|o| o.email.as_str()), Some("me@b.test"));
        assert_eq!(colleagues.len(), 1);
        let (events, colleagues) = owned_by(&None, vec![ev], Vec::new());
        assert!(events[0].owner.is_none() && colleagues.is_empty());
    }

    #[test]
    fn calendar_changes_are_debounced() {
        let t0 = tokio::time::Instant::now();
        let d = Duration::from_millis(1500);
        let mut db = Debounce::default();
        assert!(!db.fire(t0));
        db.poke(t0, d);
        // Следующие изменения пачки срок не сдвигают и второго события не дают.
        db.poke(t0 + Duration::from_millis(500), d);
        assert!(!db.fire(t0 + Duration::from_millis(1000)));
        assert!(db.fire(t0 + d));
        assert!(!db.fire(t0 + d * 2));
    }

    #[test]
    fn errors_have_kinds() {
        assert_eq!(ErrorKind::of(&Error::storage("table")), ErrorKind::Storage);
        assert_eq!(ErrorKind::of(&Error::Protocol("x".into())), ErrorKind::Protocol);
        let m = Error::Method { method: "JMAP Email/get".into(), kind: "serverFail".into(), description: String::new() };
        let e = SyncError::new(Stage::Refresh, &m);
        assert_eq!(e.kind, ErrorKind::Server);
        assert!(e.to_string().starts_with("Refresh failed (Server)"), "{e}");
    }

    /// Общий цикл: сессия не вошла по сети — «без связи» со сроком, правка — в очередь; «Повторить сейчас» —
    /// новая попытка; канал закрыт — конец.
    #[test]
    fn supervise_goes_offline_and_retries() {
        struct Down(u32);
        impl Session for Down {
            fn run<'a>(
                &'a mut self,
                _rx: &'a mut UnboundedReceiver<Command>,
                link: &'a mut Link<Command>,
                _notify: &'a dyn Fn(Event),
            ) -> BoxFuture<'a, Result<(), SyncError>> {
                self.0 += 1;
                let queued = link.queued();
                Box::pin(async move { Err(SyncError { kind: ErrorKind::Network, stage: Stage::Connect, detail: format!("down, queued {queued}") }) })
            }
        }
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tx.send(Command::Destroy("e1".into())).unwrap();
        tx.send(Command::Reconnect).unwrap();
        drop(tx);
        let events = std::sync::Mutex::new(Vec::new());
        let mut down = Down(0);
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(supervise(&mut down, rx, &|e| events.lock().unwrap().push(e)));
        assert_eq!(down.0, 2, "вторая попытка — по «Повторить сейчас»");
        let events = events.into_inner().unwrap();
        let offline: Vec<usize> = events
            .iter()
            .filter_map(|e| match e {
                Event::Connection(Connection::Offline { queued, .. }) => Some(*queued),
                _ => None,
            })
            .collect();
        assert_eq!(offline, [0, 1, 1]);
    }

    /// «Отправить и получить» без связи (T3-02): `SyncNow` — повторный вход сразу, ход — начало и конец; вход не
    /// удался — конец с ошибкой, удался (первая загрузка) — без ошибки.
    #[test]
    fn send_receive_offline_reports_start_and_end() {
        /// Первая попытка — сети нет, следующие — вход и первая загрузка, затем работа до конца канала.
        struct Flaky(u32);
        impl Session for Flaky {
            fn run<'a>(
                &'a mut self,
                rx: &'a mut UnboundedReceiver<Command>,
                _link: &'a mut Link<Command>,
                notify: &'a dyn Fn(Event),
            ) -> BoxFuture<'a, Result<(), SyncError>> {
                self.0 += 1;
                let attempt = self.0;
                Box::pin(async move {
                    if attempt < 3 {
                        return Err(SyncError { kind: ErrorKind::Network, stage: Stage::Connect, detail: "down".into() });
                    }
                    notify(Event::Status(Status::Synced));
                    while rx.recv().await.is_some() {}
                    Ok(())
                })
            }
        }
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tx.send(Command::SyncNow).unwrap();
        tx.send(Command::SyncNow).unwrap();
        drop(tx);
        let events = std::sync::Mutex::new(Vec::new());
        let mut flaky = Flaky(0);
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(supervise(&mut flaky, rx, &|e| events.lock().unwrap().push(e)));
        assert_eq!(flaky.0, 3);
        let progress: Vec<SendReceive> = events
            .into_inner()
            .unwrap()
            .into_iter()
            .filter_map(|e| match e {
                Event::SendReceive(p) => Some(p),
                _ => None,
            })
            .collect();
        assert_eq!(
            progress,
            [
                SendReceive::Started,
                SendReceive::Finished { error: Some("Connect failed (Network): down".into()) },
                SendReceive::Started,
                SendReceive::Finished { error: None },
            ]
        );
    }

    /// Каждый вариант договора проходит «туда и обратно» (postcard) без потерь. Списки образцов сверяются с `match`
    /// без `_`: новый вариант без образца здесь не собирается (как у обмена каналов, `backend::wire`).
    mod wire {
        use super::*;

        fn round<T: Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug>(v: &T) {
            let bytes = postcard::to_stdvec(v).unwrap();
            assert_eq!(&postcard::from_bytes::<T>(&bytes).unwrap(), v);
        }

        fn draft() -> Draft {
            Draft {
                from: Address::new("Я", "me@example.test"),
                to: vec![Address::new("Анна", "anna@example.test")],
                subject: "Отчёт".into(),
                markdown: "текст письма".into(),
                attachments: vec![postman_compose::Attachment {
                    name: "a.txt".into(),
                    mime: "text/plain".into(),
                    data: vec![1],
                    inline_cid: None,
                    blob_id: Some("b1".into()),
                }],
                calendar: Some(postman_compose::CalendarPart { method: "REPLY".into(), ics: "BEGIN:VCALENDAR".into() }),
                keywords: vec!["pm.cat.work".into()],
                sign: true,
                share: Some(postman_compose::Share {
                    known: vec!["anna@example.test".into()],
                    attachments: Vec::new(),
                    note: "Файлы беседы".into(),
                    off: false,
                }),
                ..Draft::default()
            }
        }

        fn cal_event() -> postman_calendar::Event {
            let raw = serde_json::json!({ "id": "e-1", "uid": "u-1", "title": "Планёрка", "start": "2026-11-20T10:00:00", "duration": "PT1H" });
            postman_calendar::Event::from_jmap(&raw, &jiff::tz::TimeZone::UTC).unwrap()
        }

        fn invitation() -> Invitation {
            Invitation {
                method: "REQUEST".into(), event: cal_event(), sequence: 2, raw: serde_json::json!({ "uid": "u-1", "title": "Планёрка" })
            }
        }

        fn meeting() -> Meeting {
            Meeting {
                uid: "u-1".into(),
                title: "Планёрка".into(),
                location: "Переговорная 1".into(),
                description: "Еженедельная".into(),
                start: jiff::civil::date(2026, 11, 20).at(10, 0, 0, 0),
                time_zone: "Europe/Moscow".into(),
                duration_secs: 3600,
                attendees: vec![("Анна".into(), "anna@example.test".into())],
                optional: Vec::new(),
                from: Some("me@example.test".into()),
                reminder_secs: Some(600),
                recurrence: Some("FREQ=WEEKLY".into()),
                all_day: false,
                private: false,
            }
        }

        fn task() -> postman_calendar::Task {
            let mut t = postman_calendar::Task::new("t-1@postman", "cal-1", "Купить кофе");
            t.due = Some(postman_calendar::Moment::new(jiff::civil::date(2026, 11, 20).at(9, 0, 0, 0), Some("Europe/Moscow")));
            t.important = true;
            t.progress = postman_calendar::Progress::InProcess;
            t.keywords = vec!["pm.cat.work".into()];
            t
        }

        fn scheduled() -> Scheduled {
            Scheduled { submission: "s-1".into(), email: "e-1".into(), send_at: 1_768_467_600 }
        }

        fn selector() -> Selector {
            Selector { mailbox: "inbox".into(), filter: Some(Filter::NotKeyword(crate::labels::SEEN.into())), except: vec!["e-9".into()] }
        }

        /// Номер варианта `Command` в порядке объявления.
        fn command_index(c: &Command) -> usize {
            match c {
                Command::Body(_) => 0,
                Command::Thread(_) => 1,
                Command::Keyword { .. } => 2,
                Command::SetKeywords { .. } => 3,
                Command::Query { .. } => 4,
                Command::Counts { .. } => 5,
                Command::Move { .. } => 6,
                Command::Destroy(_) => 7,
                Command::CreateFolder { .. } => 8,
                Command::RenameFolder { .. } => 9,
                Command::MoveFolder { .. } => 10,
                Command::Send { .. } => 11,
                Command::Unsend { .. } => 12,
                Command::Scheduled => 13,
                Command::SaveDraft { .. } => 14,
                Command::LoadMore { .. } => 15,
                Command::Search { .. } => 16,
                Command::FindMessage { .. } => 17,
                Command::Blob { .. } => 18,
                Command::RespondInvite { .. } => 19,
                Command::ImportInvite { .. } => 20,
                Command::RemoveInvite { .. } => 21,
                Command::Events { .. } => 22,
                Command::SearchEvents { .. } => 23,
                Command::Availability { .. } => 24,
                Command::CreateMeeting { .. } => 25,
                Command::UpdateMeeting { .. } => 26,
                Command::CancelMeeting { .. } => 27,
                Command::ShareFreeBusy { .. } => 28,
                Command::CalendarAccess => 29,
                Command::Publications => 30,
                Command::Publish { .. } => 31,
                Command::Unpublish { .. } => 32,
                Command::UnshareCalendar { .. } => 33,
                Command::Settings(_) => 34,
                Command::Contacts => 35,
                Command::DirectoryPage { .. } => 36,
                Command::FindPeople { .. } => 37,
                Command::PersonDetails { .. } => 38,
                Command::ExpandList { .. } => 39,
                Command::SaveContact { .. } => 40,
                Command::DeleteContact { .. } => 41,
                Command::Categories(_) => 42,
                Command::Patch { .. } => 43,
                Command::Newsletters { .. } => 44,
                Command::ReportPhishing { .. } => 45,
                Command::Reconnect => 46,
                Command::SyncNow => 47,
                Command::Tasks(_) => 48,
                Command::Delegate { .. } => 49,
                Command::Withhold { .. } => 50,
                Command::MoveLetters { .. } => 51,
                Command::FetchRaw { .. } => 52,
                Command::Import { .. } => 53,
                Command::ServerCopy { .. } => 54,
                Command::DestroyLetters { .. } => 55,
                Command::Migration(_) => 56,
                Command::Sweep { .. } => 57,
                Command::StopSweep { .. } => 58,
                Command::ListLetters { .. } => 59,
                Command::StoreLetters { .. } => 60,
                Command::Export { .. } => 61,
                Command::StopExport { .. } => 62,
                Command::Channel { .. } => 63,
            }
        }

        fn commands() -> Vec<Command> {
            let all = vec![
                Command::Body("e-1".into()),
                Command::Thread("t-1".into()),
                Command::Keyword { id: "e-1".into(), keyword: crate::labels::FLAGGED, on: true },
                Command::SetKeywords { ids: vec!["e-1".into()], add: vec!["pm.cat.work".into()], remove: Vec::new() },
                Command::Query { filter: Filter::Text("отчёт".into()), mailbox: Some("inbox".into()), ticket: 1 },
                Command::Counts { filters: vec![Filter::HasKeyword(crate::labels::FLAGGED.into())], mailbox: None, ticket: 2 },
                Command::Move { id: "e-1".into(), from: "inbox".into(), to: "archive".into() },
                Command::Destroy("e-1".into()),
                Command::CreateFolder { account: None, parent: Some("inbox".into()), name: "Новая".into() },
                Command::RenameFolder { mailbox: "f-1".into(), name: "Новое имя".into() },
                Command::MoveFolder { mailbox: "f-1".into(), parent: None },
                Command::Send { draft: Box::new(draft()), replace: Some("d-1".into()), ticket: 3, release: Release::After(30) },
                Command::Unsend { scheduled: scheduled(), ticket: 4 },
                Command::Scheduled,
                Command::SaveDraft { draft: Box::new(Draft::default()), replace: None, ticket: 5 },
                Command::LoadMore { mailbox: "inbox".into() },
                Command::Search { query: "отчёт".into(), mailbox: None, ticket: 6 },
                Command::FindMessage { message_id: "m-1@example.test".into(), ticket: 7 },
                Command::Blob { blob_id: "b-1".into(), name: "photo.png".into(), mime: "image/png".into() },
                Command::RespondInvite {
                    invite: Box::new(invitation()),
                    status: Partstat::Accepted,
                    in_reply_to: Some("<m-1@example.test>".into()),
                    answer: crate::invite::Answer { comment: Some("Приду".into()), silent: false },
                },
                Command::ImportInvite { invite: Box::new(invitation()) },
                Command::RemoveInvite { invite: Box::new(invitation()) },
                Command::Events { from: 1_768_400_000, to: 1_768_900_000 },
                Command::SearchEvents { text: "планёрка".into(), from: 1_768_400_000, to: 1_768_900_000 },
                Command::Availability {
                    ticket: 8,
                    emails: vec!["anna@example.test".into()],
                    from: 0,
                    to: 3600,
                    sender: Some("me@example.test".into()),
                },
                Command::CreateMeeting { meeting: Box::new(meeting()), ticket: 9 },
                Command::UpdateMeeting { event: Box::new(cal_event()), meeting: Box::new(meeting()), series: true, ticket: 10 },
                Command::CancelMeeting { event: Box::new(cal_event()), series: false, from: Some("me@example.test".into()), ticket: 11 },
                Command::ShareFreeBusy { email: "anna@example.test".into(), items: true },
                Command::CalendarAccess,
                Command::Publications,
                Command::Publish { items: false },
                Command::Unpublish { id: "pub-1".into() },
                Command::UnshareCalendar { email: "anna@example.test".into() },
                Command::Settings(settings::Request::SetVacation(crate::model::VacationResponse::default())),
                Command::Contacts,
                Command::DirectoryPage { account: Some("@2".into()), offset: 20, max: 100 },
                Command::FindPeople { text: "анна".into(), offset: 0 },
                Command::PersonDetails { email: "anna@example.test".into() },
                Command::ExpandList { ticket: 12, address: "all@example.test".into() },
                Command::SaveContact {
                    card: Box::new(ContactCard {
                        id: String::new(),
                        name: "Анна".into(),
                        emails: vec!["anna@example.test".into()],
                        ..ContactCard::default()
                    }),
                    old: None,
                    book: Some("book-1".into()),
                    ticket: 13,
                },
                Command::DeleteContact { id: "c-1".into(), ticket: 14 },
                Command::Categories(categories::Request::Load { account: None }),
                Command::Patch {
                    patches: vec![lifecycle::Patch { id: "e-1".into(), keywords: vec![(crate::labels::SEEN.into(), true)], mailboxes: Vec::new() }],
                    archive_name: "Архив".into(),
                },
                Command::Newsletters { cutoff: 1_768_000_000, archive_name: "Архив".into() },
                Command::ReportPhishing {
                    ids: vec!["e-1".into()],
                    to: Address::new("Абуз", "abuse@example.test"),
                    subject: "Фишинг".into(),
                    text: "Письмо".into(),
                    ticket: 15,
                },
                Command::Reconnect,
                Command::SyncNow,
                Command::Tasks(tasks::Request::Save(Box::new(task()))),
                Command::Delegate { uid: "t-1@postman".into(), drafts: vec![draft()], ticket: 16, release: Release::At(1_768_467_600) },
                Command::Withhold { scheduled: vec![scheduled()], ticket: 17 },
                Command::MoveLetters { ids: vec![LetterId(1), LetterId(2)], to: FolderId(3) },
                Command::FetchRaw { letter: LetterId(4) },
                Command::Import {
                    job: JobId(1),
                    letter: LetterId(2),
                    folder: FolderId(9),
                    keywords: vec![crate::labels::SEEN.into()],
                    received_at: 1_768_000_000,
                    message_id: Some("<m-1@example.test>".into()),
                    retry: true,
                },
                Command::ServerCopy { job: JobId(1), letters: vec![LetterId(2)], to: FolderId(9) },
                Command::DestroyLetters { job: JobId(1), account: "@2".into(), letters: vec![LetterId(1)] },
                Command::Migration(MigrationRequest::Pause { job: JobId(1) }),
                Command::Sweep { selector: selector(), action: sweep::Action::Purge },
                Command::StopSweep { sweep: SweepId(7) },
                Command::ListLetters {
                    sweep: SweepId(7),
                    selector: Selector { mailbox: "inbox".into(), filter: None, except: Vec::new() },
                    position: 0,
                    limit: 100,
                },
                Command::StoreLetters { sweep: SweepId(7), mailbox: "inbox".into(), ids: vec!["e-1".into()] },
                Command::Export {
                    scope: export::Scope::Folder { id: FolderId(11), subfolders: true },
                    format: export::Format::Mbox,
                    dir: "out".into(),
                },
                Command::StopExport { job: export::ExportId(1) },
                Command::Channel {
                    account: crate::channel::ChanAccount::new("demo", "me@example.test"),
                    request: Box::new(crate::channel::Request::Reconnect),
                },
            ];
            assert_eq!(all.iter().map(command_index).collect::<Vec<_>>(), (0..all.len()).collect::<Vec<_>>(), "у каждой команды — образец");
            all
        }

        /// Номер варианта `Event` в порядке объявления.
        fn event_index(e: &Event) -> usize {
            match e {
                Event::Status(_) => 0,
                Event::MailboxesChanged => 1,
                Event::EmailsChanged => 2,
                Event::BodyReady(_) => 3,
                Event::ThreadReady { .. } => 4,
                Event::BlobReady { .. } => 5,
                Event::BodiesStored(_) => 6,
                Event::LoadedMore { .. } => 7,
                Event::CalendarChanged => 8,
                Event::SearchResults { .. } => 9,
                Event::Counts { .. } => 10,
                Event::BlobFailed { .. } => 11,
                Event::Me { .. } => 12,
                Event::Features(_) => 13,
                Event::Senders(_) => 14,
                Event::Contacts(_) => 15,
                Event::Sent { .. } => 16,
                Event::Unsent { .. } => 17,
                Event::UnsendFailed { .. } => 18,
                Event::Scheduled { .. } => 19,
                Event::DraftSaved { .. } => 20,
                Event::ComposeFailed { .. } => 21,
                Event::InviteAnswered { .. } => 22,
                Event::InviteFailed { .. } => 23,
                Event::InviteRemoved { .. } => 24,
                Event::EventsLoaded { .. } => 25,
                Event::EventsFound { .. } => 26,
                Event::Availability { .. } => 27,
                Event::MeetingCreated { .. } => 28,
                Event::MeetingUpdated { .. } => 29,
                Event::MeetingCancelled { .. } => 30,
                Event::MeetingFailed { .. } => 31,
                Event::CalendarAccess(_) => 32,
                Event::Publications(_) => 33,
                Event::Published { .. } => 34,
                Event::FreeBusyShared { .. } => 35,
                Event::CalendarFailed(_) => 36,
                Event::Settings(_) => 37,
                Event::ContactsLoaded(_) => 38,
                Event::ContactBooks(_) => 39,
                Event::DirectoryLoaded(_) => 40,
                Event::DirectoryPage { .. } => 41,
                Event::PeopleFound { .. } => 42,
                Event::PersonDetails { .. } => 43,
                Event::ListMembers { .. } => 44,
                Event::ContactSaved { .. } => 45,
                Event::ContactDeleted { .. } => 46,
                Event::ContactFailed { .. } => 47,
                Event::Categories(_) => 48,
                Event::Tasks(_) => 49,
                Event::Newsletters { .. } => 50,
                Event::Connection(_) => 51,
                Event::SendReceive(_) => 52,
                Event::Reported { .. } => 53,
                Event::Delegated { .. } => 54,
                Event::Withheld { .. } => 55,
                Event::Raw { .. } => 56,
                Event::Imported { .. } => 57,
                Event::ImportFailed { .. } => 58,
                Event::Destroyed { .. } => 59,
                Event::DestroyFailed { .. } => 60,
                Event::SourceKept { .. } => 61,
                Event::StoreUpgrade { .. } => 62,
                Event::LettersListed { .. } => 63,
                Event::LettersStored { .. } => 64,
                Event::ListFailed { .. } => 65,
                Event::Sweep(_) => 66,
                Event::Migration { .. } => 67,
                Event::Export { .. } => 68,
                Event::FolderFailed { .. } => 69,
                Event::Error(_) => 70,
                Event::Channel(..) => 71,
            }
        }

        fn events() -> Vec<Event> {
            let all = vec![
                Event::Status(Status::Loading { mailbox: "Входящие".into() }),
                Event::MailboxesChanged,
                Event::EmailsChanged,
                Event::BodyReady("e-1".into()),
                Event::ThreadReady { thread: "t-1".into(), ids: vec!["e-1".into(), "e-2".into()] },
                Event::BlobReady { blob_id: "b-1".into(), data: Arc::new(vec![1, 2, 3]) },
                Event::BodiesStored(vec!["e-1".into()]),
                Event::LoadedMore { mailbox: "inbox".into(), added: 25 },
                Event::CalendarChanged,
                Event::SearchResults { ticket: 1, ids: vec!["e-1".into()], total: 42 },
                Event::Counts { ticket: 2, counts: Some(vec![1, 2]) },
                Event::BlobFailed { blob_id: "b-1".into(), error: "gone".into() },
                Event::Me { address: Address::new("Я", "me@example.test"), signature: "С уважением".into() },
                Event::Features(crate::model::Features::ALL.with_import(true)),
                Event::Senders(vec![Sender {
                    account: Some("shared".into()),
                    address: Address::new("Я", "me@example.test"),
                    signature: String::new(),
                }]),
                Event::Contacts(vec![("Анна".into(), "anna@example.test".into())]),
                Event::Sent { ticket: 3, scheduled: Some(scheduled()) },
                Event::Unsent { ticket: 4, email: "e-1".into() },
                Event::UnsendFailed { ticket: 5, too_late: true, error: "уже ушло".into() },
                Event::Scheduled { list: vec![scheduled()], max_delay: 120 },
                Event::DraftSaved { ticket: 6, id: "d-1".into() },
                Event::ComposeFailed { ticket: 7, error: "нет темы".into() },
                Event::InviteAnswered { uid: "u-1".into(), status: Partstat::Tentative, delivery: crate::invite::Delivery::Mail { imported: true } },
                Event::InviteFailed { uid: "u-1".into(), error: "no calendar".into() },
                Event::InviteRemoved { uid: "u-1".into(), found: true },
                Event::EventsLoaded {
                    from: 1_768_400_000,
                    to: 1_768_900_000,
                    events: vec![cal_event()],
                    colleagues: vec![postman_calendar::Owner { email: "anna@example.test".into(), name: "Анна".into() }],
                    account: Some("@2".into()),
                },
                Event::EventsFound { text: "план".into(), events: Vec::new(), error: Some("down".into()) },
                Event::Availability {
                    ticket: 8,
                    me: vec![Busy { span: postman_calendar::Span::new(0, 3600), kind: postman_calendar::BusyKind::Busy }],
                    people: vec![("anna@example.test".into(), None)],
                    notes: vec![(
                        "anna@example.test".into(),
                        vec![BusyNote { span: postman_calendar::Span::new(0, 3600), subject: "Занят".into(), location: "каб. 1".into() }],
                    )],
                    error: None,
                },
                Event::MeetingCreated { ticket: 9, uid: "u-1".into() },
                Event::MeetingUpdated { ticket: 10, uid: "u-1".into() },
                Event::MeetingCancelled { ticket: 11, uid: "u-1".into() },
                Event::MeetingFailed { ticket: 12, error: "no calendar".into() },
                Event::CalendarAccess(vec![Access { email: "anna@example.test".into(), name: "Анна".into(), items: true }]),
                Event::Publications(Some(vec![Publication { id: "pub-1".into(), items: false }])),
                Event::Published { url: "https://example.test/cal/abc".into() },
                Event::FreeBusyShared { email: "anna@example.test".into() },
                Event::CalendarFailed("нет календаря".into()),
                Event::Settings(settings::Reply::Quota(Some(settings::Usage { used: 10, limit: 100 }))),
                Event::ContactsLoaded(vec![ContactCard { id: "c-1".into(), name: "Анна".into(), ..ContactCard::default() }]),
                Event::ContactBooks(vec![ContactBook { id: "book-1".into(), name: "Контакты".into(), default: true }]),
                Event::DirectoryLoaded(vec![Principal { id: "p-1".into(), email: Some("p-1@example.test".into()), ..Principal::default() }]),
                Event::DirectoryPage { account: None, offset: 20, people: vec![Principal::default()], total: Some(100), search_only: true },
                Event::PeopleFound { text: "ан".into(), offset: 0, people: Vec::new(), more: true },
                Event::PersonDetails { email: "p-1@example.test".into(), person: Some(Principal::default()) },
                Event::ListMembers { ticket: 13, address: "all@example.test".into(), members: Vec::new(), error: Some(ListError::Unsupported) },
                Event::ContactSaved { ticket: 14, id: "c-1".into() },
                Event::ContactDeleted { ticket: 15, id: "c-1".into() },
                Event::ContactFailed { ticket: 0, error: "409".into() },
                Event::Categories(categories::Reply::Loaded { account: None, table: None }),
                Event::Tasks(tasks::Reply::Changed(vec![task()])),
                Event::Newsletters { archived: 12 },
                Event::Connection(Connection::Offline { retry_in: 5, queued: 1 }),
                Event::SendReceive(SendReceive::Finished { error: Some("down".into()) }),
                Event::Reported { ticket: 16, error: None },
                Event::Delegated { ticket: 17, uid: "t-1@postman".into(), error: None, scheduled: vec![scheduled()] },
                Event::Withheld { ticket: 18, too_late: false, error: Some("поздно".into()) },
                Event::Raw { letter: LetterId(4), data: Ok(Arc::new(b"RFC 5322".to_vec())) },
                Event::Imported { job: JobId(1), letter: LetterId(2), remote: Remote { account: "@2".into(), id: "m-1".into(), data: vec![9] } },
                Event::ImportFailed { job: JobId(1), letter: LetterId(2), error: ImportError::Throttled { retry_after: 30 } },
                Event::Destroyed { job: JobId(1), letter: LetterId(1) },
                Event::DestroyFailed { job: JobId(1), letter: LetterId(1), error: "500".into() },
                Event::SourceKept { job: JobId(1), letters: 3 },
                Event::StoreUpgrade { done: 40, total: 50 },
                Event::LettersListed { sweep: SweepId(7), ids: vec!["e-1".into()], total: 100, end: false },
                Event::LettersStored { sweep: SweepId(7), gone: vec!["e-9".into()] },
                Event::ListFailed { sweep: SweepId(7), error: "not supported".into() },
                Event::Sweep(sweep::Progress {
                    sweep: SweepId(7),
                    action: sweep::Action::Purge,
                    job: None,
                    done: 10,
                    total: 100,
                    state: sweep::State::Working,
                }),
                Event::Migration { job: JobId(1), done: 5, total: 10, bytes: 2048, state: MigrationState::Running },
                Event::Export { job: export::ExportId(1), done: 3, total: 9, bytes: 1024, state: export::State::Running { fetching: 2 } },
                Event::FolderFailed { mailbox: Some("f-1".into()), error: FOLDER_UNSUPPORTED.into() },
                Event::Error(SyncError { kind: ErrorKind::Network, stage: Stage::Connect, detail: "down".into() }),
                Event::Channel(crate::channel::ChanAccount::new("demo", "me@example.test"), crate::channel::Update::SignedOut),
            ];
            assert_eq!(all.iter().map(event_index).collect::<Vec<_>>(), (0..all.len()).collect::<Vec<_>>(), "у каждого события — образец");
            all
        }

        #[test]
        fn every_command_and_event_round_trips() {
            commands().iter().for_each(round);
            events().iter().for_each(round);
            // Внутренние формы вариантов: отказ письма целиком, пустые ответы.
            let more = [
                Event::Raw { letter: LetterId(5), data: Err("not supported".into()) },
                Event::Publications(None),
                Event::Counts { ticket: 9, counts: None },
                Event::Sent { ticket: 10, scheduled: None },
                Event::EventsFound { text: String::new(), events: Vec::new(), error: None },
                Event::ListMembers { ticket: 11, address: String::new(), members: Vec::new(), error: Some(ListError::Failed("500".into())) },
            ];
            more.iter().for_each(round);
            round(&Command::Keyword { id: "e-2".into(), keyword: crate::labels::SEEN, on: false });
            // Ключевое слово — значением то же слово таблицы ядра (какой копией статической строки его дал
            // компоновщик — дело платформы, утечки для известных слов нет).
            let back = postcard::from_bytes::<Command>(
                &postcard::to_stdvec(&Command::Keyword { id: "e-1".into(), keyword: crate::labels::FLAGGED, on: true }).unwrap(),
            )
            .unwrap();
            assert_eq!(back, Command::Keyword { id: "e-1".into(), keyword: crate::labels::FLAGGED, on: true });
        }

        /// Вложенные перечисления договора — каждый вариант, без `_`.
        #[test]
        fn nested_enums_round_trips() {
            let statuses = [
                Status::Connecting { url: "https://example.test".into() },
                Status::SignedIn { user: "me".into() },
                Status::Loading { mailbox: "Входящие".into() },
                Status::Synced,
            ];
            let covered = |s: &Status| match s {
                Status::Connecting { .. } => 0,
                Status::SignedIn { .. } => 1,
                Status::Loading { .. } => 2,
                Status::Synced => 3,
            };
            assert_eq!(statuses.iter().map(covered).collect::<Vec<_>>(), (0..statuses.len()).collect::<Vec<_>>());
            statuses.iter().for_each(round);

            let stages = [Stage::Start, Stage::Connect, Stage::Load, Stage::Refresh, Stage::Prefetch, Stage::Command, Stage::Restore];
            let covered = |s: &Stage| match s {
                Stage::Start => 0,
                Stage::Connect => 1,
                Stage::Load => 2,
                Stage::Refresh => 3,
                Stage::Prefetch => 4,
                Stage::Command => 5,
                Stage::Restore => 6,
            };
            assert_eq!(stages.iter().map(covered).collect::<Vec<_>>(), (0..stages.len()).collect::<Vec<_>>());
            stages.iter().for_each(round);

            let kinds = [ErrorKind::Auth, ErrorKind::Network, ErrorKind::Server, ErrorKind::Protocol, ErrorKind::Storage, ErrorKind::Internal];
            let covered = |k: &ErrorKind| match k {
                ErrorKind::Auth => 0,
                ErrorKind::Network => 1,
                ErrorKind::Server => 2,
                ErrorKind::Protocol => 3,
                ErrorKind::Storage => 4,
                ErrorKind::Internal => 5,
            };
            assert_eq!(kinds.iter().map(covered).collect::<Vec<_>>(), (0..kinds.len()).collect::<Vec<_>>());
            kinds.iter().for_each(round);

            let rounds = [SendReceive::Started, SendReceive::Finished { error: None }, SendReceive::Finished { error: Some("down".into()) }];
            let covered = |s: &SendReceive| match s {
                SendReceive::Started => 0,
                SendReceive::Finished { error: None } => 1,
                SendReceive::Finished { error: Some(_) } => 2,
            };
            assert_eq!(rounds.iter().map(covered).collect::<Vec<_>>(), (0..rounds.len()).collect::<Vec<_>>());
            rounds.iter().for_each(round);

            let connections = [Connection::Online, Connection::Offline { retry_in: 5, queued: 1 }];
            let covered = |c: &Connection| match c {
                Connection::Online => 0,
                Connection::Offline { .. } => 1,
            };
            assert_eq!(connections.iter().map(covered).collect::<Vec<_>>(), (0..connections.len()).collect::<Vec<_>>());
            connections.iter().for_each(round);

            let errors = [ListError::Unsupported, ListError::Failed("500".into())];
            let covered = |e: &ListError| match e {
                ListError::Unsupported => 0,
                ListError::Failed(_) => 1,
            };
            assert_eq!(errors.iter().map(covered).collect::<Vec<_>>(), (0..errors.len()).collect::<Vec<_>>());
            errors.iter().for_each(round);

            let deliveries =
                [crate::invite::Delivery::Server, crate::invite::Delivery::Mail { imported: true }, crate::invite::Delivery::Silent { saved: false }];
            let covered = |d: &crate::invite::Delivery| match d {
                crate::invite::Delivery::Server => 0,
                crate::invite::Delivery::Mail { .. } => 1,
                crate::invite::Delivery::Silent { .. } => 2,
            };
            assert_eq!(deliveries.iter().map(covered).collect::<Vec<_>>(), (0..deliveries.len()).collect::<Vec<_>>());
            deliveries.iter().for_each(round);

            let releases = [Release::Now, Release::After(30), Release::At(1_768_467_600)];
            let covered = |r: &Release| match r {
                Release::Now => 0,
                Release::After(_) => 1,
                Release::At(_) => 2,
            };
            assert_eq!(releases.iter().map(covered).collect::<Vec<_>>(), (0..releases.len()).collect::<Vec<_>>());
            releases.iter().for_each(round);

            let homes = [Home::Account("@2".into()), Home::Local];
            let covered = |h: &Home| match h {
                Home::Account(_) => 0,
                Home::Local => 1,
            };
            assert_eq!(homes.iter().map(covered).collect::<Vec<_>>(), (0..homes.len()).collect::<Vec<_>>());
            homes.iter().for_each(round);

            let states = [
                MigrationState::Asking,
                MigrationState::Running,
                MigrationState::Paused,
                MigrationState::QuotaFull,
                MigrationState::Done,
                MigrationState::Cancelled,
            ];
            let covered = |s: &MigrationState| match s {
                MigrationState::Asking => 0,
                MigrationState::Running => 1,
                MigrationState::Paused => 2,
                MigrationState::QuotaFull => 3,
                MigrationState::Done => 4,
                MigrationState::Cancelled => 5,
            };
            assert_eq!(states.iter().map(covered).collect::<Vec<_>>(), (0..states.len()).collect::<Vec<_>>());
            states.iter().for_each(round);

            let requests = [
                MigrationRequest::Answer { job: JobId(1), mode: crate::model::MigrationMode::Upload },
                MigrationRequest::Pause { job: JobId(1) },
                MigrationRequest::Resume { job: JobId(1) },
                MigrationRequest::Cancel { job: JobId(1), keep_local: true },
            ];
            let covered = |r: &MigrationRequest| match r {
                MigrationRequest::Answer { .. } => 0,
                MigrationRequest::Pause { .. } => 1,
                MigrationRequest::Resume { .. } => 2,
                MigrationRequest::Cancel { .. } => 3,
            };
            assert_eq!(requests.iter().map(covered).collect::<Vec<_>>(), (0..requests.len()).collect::<Vec<_>>());
            requests.iter().for_each(round);

            let import_errors =
                [ImportError::TooLarge, ImportError::Quota, ImportError::Throttled { retry_after: 30 }, ImportError::Rejected("550".into())];
            let covered = |e: &ImportError| match e {
                ImportError::TooLarge => 0,
                ImportError::Quota => 1,
                ImportError::Throttled { .. } => 2,
                ImportError::Rejected(_) => 3,
            };
            assert_eq!(import_errors.iter().map(covered).collect::<Vec<_>>(), (0..import_errors.len()).collect::<Vec<_>>());
            import_errors.iter().for_each(round);

            [crate::model::Features::ALL, crate::model::Features::NONE, crate::model::Features::NONE.with_import(true).with_server_copy(true)]
                .iter()
                .for_each(round);
            round(&SyncError { kind: ErrorKind::Server, stage: Stage::Command, detail: "serverFail".into() });
        }
    }
}
