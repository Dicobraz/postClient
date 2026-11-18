//! Области учётной записи разводит ядро (ADR 0013 «Области», PLG-07): когда почта и область — разные процессы,
//! команды областей приходят сначала областям учётки («не моя — назад», как [`Areas::handle`]), затем почтовому
//! пути. Возможности сервера ([`model::Features`]) объединяет ядро: события почтового пути идут через мост, который
//! заменяет `Event::Features` объединёнными с областями. Опрос без push ([`Areas::changed`]) — таймер ядра, раз в
//! 2 минуты (как `POLL` у IMAP). Почтовые пути внутри `postman.exe` (до PLG-08…10) работают с областью-процессом
//! так же: реестр даёт заместителей ([`crate::backend::remote`]), для ядра разницы нет.
//!
//! Ядро стоит между интерфейсом и почтовым путём: команды — сначала области, потом почте; события почтового пути —
//! интерфейсу (через мост), `Event::Features` — объединённым с областями (у дополнительной учётки своего события
//! нет, как у почтовых путей). Ответ на приглашение и шаг поручения — совместные: сначала область (копия в
//! календаре, правки задач), письма (iMIP, iTIP) — почтовому пути. Задачи поверх кэша — услуга ядра: команда задач
//! идёт в [`crate::tasks::handle`] у ядра, а хранилище задач (VTODO календарей CalDAV) спрашивает плагин по обмену
//! ([`crate::backend::wire::TaskOp`]).

use std::sync::{Arc, Mutex, PoisonError};

use postman_cache::Cache;
use postman_calendar::{Owner, Partstat};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use super::{AreaCtx, AreaPlugin, Areas, Login, MailPlugin, Spawn};
use crate::invite::{Answer, Responded, delivery};
use crate::model::{Auth, Feature, Features};
use crate::sync::{self, Account, Command, Event, Index, Notifier};
use crate::tasks;

/// Опрос областей без push (CalDAV): ctag и sync-token — раз в 2 минуты, как `POLL` у IMAP был раньше.
const POLL: std::time::Duration = std::time::Duration::from_secs(120);

/// Разводить команды областей учётной записи (ADR 0013 «Области», PLG-07): почтовый путь `mail` запускается здесь
/// же — его события идут через мост ([`Both`], `Features` — объединённые с областями), команды — сначала областям
/// (`addresses` по ключам: `dav` → `https://…`), затем почтовому пути. `plugins` — пути областей реестра. Ответ —
/// канал команд учётной записи.
pub(crate) fn serve(
    account: Account,
    profile: Option<String>,
    cache: Cache,
    index: Index,
    events: std::sync::mpsc::Sender<Event>,
    repaint: Arc<dyn Fn() + Send + Sync>,
    mail: Arc<dyn MailPlugin>,
    plugins: Vec<Arc<dyn AreaPlugin>>,
    addresses: Vec<(&'static str, String)>,
) -> UnboundedSender<Command> {
    // Мост событий почтового пути: интерфейсу — как есть, `Features` — объединённые с областями (правило
    // «Нет у сервера — нет на экране»): событие почтового пути без областей скрыло бы календарь.
    let both = Arc::new(Mutex::new(Both { mail: Features::NONE, areas: Features::NONE }));
    let (mail_events, mail_rx) = std::sync::mpsc::channel();
    let forward = Both::forwarder(Arc::clone(&both), events.clone());
    let bridged = std::thread::Builder::new().name("sync-areas-mail".into()).spawn(move || {
        for event in mail_rx {
            forward(event);
        }
    });
    if let Err(e) = bridged {
        let _ = events.send(Event::Error(crate::sync::SyncError::internal(format!("areas bridge: {e}"))));
        return unbounded_channel().0;
    }
    let for_mail: Box<dyn Fn() + Send> = Box::new({
        let r = Arc::clone(&repaint);
        move || r()
    });
    let (url, auth) = (account.url.clone(), account.auth.clone());
    let commands = mail.spawn(Spawn { account, profile: profile.clone(), cache: cache.clone(), index, events: mail_events, repaint: for_mail });
    let (tx, rx) = unbounded_channel();
    sync::spawn_thread("sync-areas", events, Box::new(move || repaint()), move |notify| async move {
        run(url, auth, profile, cache, plugins, addresses, commands, rx, &notify, &both).await;
    });
    tx
}

/// Возможности почтового пути и областей — по отдельности; интерфейсу — объединение.
struct Both {
    mail: Features,
    areas: Features,
}

impl Both {
    fn merged(&self) -> Features {
        self.mail.union(self.areas)
    }

    /// Событие почтового пути интерфейсу: `Features` — объединённые с областями.
    fn forwarder(both: Arc<Mutex<Both>>, events: std::sync::mpsc::Sender<Event>) -> impl Fn(Event) {
        move |event| {
            let out = {
                let mut b = lock(&both);
                if let Event::Features(mail) = &event {
                    b.mail = *mail;
                }
                match event {
                    Event::Features(_) => Event::Features(b.merged()),
                    other => other,
                }
            };
            let _ = events.send(out);
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Жизнь областей учётной записи: подключение, развод команд, опрос. До конца канала команд интерфейса.
async fn run(
    url: String,
    auth: Auth,
    profile: Option<String>,
    cache: Cache,
    plugins: Vec<Arc<dyn AreaPlugin>>,
    addresses: Vec<(String, String)>,
    mail: UnboundedSender<Command>,
    mut rx: UnboundedReceiver<Command>,
    notify: &Notifier,
    both: &Mutex<Both>,
) {
    let Some(login) = login_of(&url, &auth) else {
        // Вход по токену у путей с областями в адресе не бывает: области не подключить, команды — почте напрямую.
        while let Some(cmd) = rx.recv().await {
            let _ = mail.send(cmd);
        }
        return;
    };
    let mut areas: Vec<(&'static str, Box<dyn Areas>)> = Vec::new();
    for (key, address) in addresses {
        let Some(plugin) = plugins.iter().find(|p| p.key() == key) else {
            tracing::warn!(area = %key, "areas: no area backend");
            continue;
        };
        match plugin.connect(&address, &login, profile.as_deref()).await {
            Ok(a) => areas.push((plugin.key(), a)),
            // Без календаря почта всё равно работает: ошибка — тостом календаря, области скрыты.
            Err(e) => {
                tracing::warn!(area = %key, "area unavailable, mail works without it: {e}");
                notify.send(Event::CalendarFailed(e.to_string()));
            }
        }
    }
    let owner = profile.as_ref().map(|_| Owner { email: login.email.clone(), name: login.email.clone() });
    // Задачи — у основной учётной записи, как у JMAP (S7-03б): список из кэша сразу, с сервера — дельта; экран
    // задач ждёт этого ответа. Сбой — тостом, почта работает.
    let tasks_on = profile.is_none() && areas.iter().any(|(_, a)| a.features().has(Feature::Tasks));
    if tasks_on {
        area_command(&mut areas, &cache, profile.as_deref(), owner.as_ref(), notify, Command::Tasks(tasks::Request::Load)).await;
    }
    if profile.is_none() {
        lock(both).areas = areas.iter().fold(Features::NONE, |f, (_, a)| f.union(a.features()));
        notify.send(Event::Features(lock(both).merged()));
    }
    let mut poll = tokio::time::interval(POLL);
    poll.tick().await;
    loop {
        tokio::select! {
            biased;
            cmd = rx.recv() => {
                let Some(cmd) = cmd else { return };
                dispatch(&mut areas, &cache, profile.as_deref(), owner.as_ref(), notify, &mail, cmd).await;
            }
            _ = poll.tick() => {
                let mut changed = false;
                for (key, a) in areas.iter_mut() {
                    match a.changed().await {
                        Ok(c) => changed |= c,
                        Err(e) => tracing::debug!("{key} poll: {e}"),
                    }
                }
                if changed {
                    // Задачи — те же записи календаря (VTODO): сверить и их.
                    if tasks_on {
                        area_command(&mut areas, &cache, profile.as_deref(), owner.as_ref(), notify, Command::Tasks(tasks::Request::Refresh)).await;
                    }
                    notify.send(Event::CalendarChanged);
                }
            }
        }
    }
}

/// Вход в области учётной записи: как у почтовых путей без своих областей (IMAP) — логин и пароль, адрес — сам
/// пользователь. Вход по токену областей не даёт.
fn login_of(url: &str, auth: &Auth) -> Option<Login> {
    match auth {
        Auth::Basic { user, password } => Some(Login { user: user.clone(), password: password.clone(), email: user.clone() }),
        _ => None,
    }
}

/// Одна команда интерфейса: сначала области, затем почтовый путь. Ответ на приглашение и шаг поручения —
/// совместные: область делает свою часть, письма — почтовому пути.
async fn dispatch(
    areas: &mut [(&'static str, Box<dyn Areas>)],
    cache: &Cache,
    profile: Option<&str>,
    owner: Option<&Owner>,
    notify: &Notifier,
    mail: &UnboundedSender<Command>,
    cmd: Command,
) {
    match cmd {
        Command::RespondInvite { invite, status, in_reply_to, answer } => {
            if respond(areas, notify, &invite, status, &answer).await {
                return;
            }
            // Моей копии нет (или области не ответили) — письмо организатору (iMIP): почтовому пути как было.
            let _ = mail.send(Command::RespondInvite { invite, status, in_reply_to, answer });
        }
        cmd @ Command::Delegate { .. } => {
            if delegate(areas, cache, profile, owner, notify, &cmd).await {
                return;
            }
            // Правки задач ушли на сервер — письма iTIP шага: почтовому пути.
            let _ = mail.send(cmd);
        }
        other => {
            if let Some(back) = area_command(areas, cache, profile, owner, notify, other).await {
                let _ = mail.send(back);
            }
        }
    }
}

/// Ответ на приглашение: своей копией на сервере области (REPLY шлёт сервер). `true` — всё сделано (события
/// отправлены); `false` — отвечать письмом, командой почтовому пути.
async fn respond(
    areas: &mut [(&'static str, Box<dyn Areas>)],
    notify: &Notifier,
    invite: &postman_calendar::Invitation,
    status: Partstat,
    answer: &Answer,
) -> bool {
    let uid = invite.event.uid.clone();
    let mut done = Responded::NoCopy;
    if let Some(a) = areas.iter_mut().find(|(_, a)| a.features().has(Feature::Calendar)).map(|(_, a)| a) {
        match a.respond(invite, status, answer).await {
            Ok(d) => done = d,
            Err(e) => return send_and_done(notify, Event::InviteFailed { uid, error: e.to_string() }),
        }
    }
    if done != Responded::NoCopy {
        notify.send(Event::CalendarChanged);
    }
    match delivery(done, answer) {
        // Сервер отправил ответ (или «без ответа») — больше ничего.
        Ok(delivery) => send_and_done(notify, Event::InviteAnswered { uid, status, delivery }),
        // Письмо организатору: `Err(imported)` — почтовому пути.
        Err(_) => false,
    }
}

/// Шаг поручения (S6-09): правки задач — на сервер области (услуги ядра: кэш у ядра, хранилище — у плагина),
/// письма iTIP — почтовому пути. `true` — дело сделано (или не удалось: событие отправлено); `false` — команду
/// переслать почтовому пути.
async fn delegate(
    areas: &mut [(&'static str, Box<dyn Areas>)],
    cache: &Cache,
    profile: Option<&str>,
    owner: Option<&Owner>,
    notify: &Notifier,
    cmd: &Command,
) -> bool {
    let Command::Delegate { uid, ticket, .. } = cmd else { return false };
    let Some((_, a)) = areas.iter_mut().find(|(_, a)| a.features().has(Feature::Tasks)).map(|(_, a)| a) else {
        return send_and_done(
            notify,
            Event::Delegated { ticket: *ticket, uid: uid.clone(), error: Some("no tasks on the server".into()), scheduled: Vec::new() },
        );
    };
    let ctx = AreaCtx { cache, profile, owner, notify: &|e| notify.send(e) };
    if let Err(e) = a.flush_tasks(ctx).await {
        return send_and_done(notify, Event::Delegated { ticket: *ticket, uid: uid.clone(), error: Some(e.to_string()), scheduled: Vec::new() });
    }
    false
}

/// Команда по областям учётки по порядку (`Areas::handle`); `Some` — не её, почтовому пути.
async fn area_command(
    areas: &mut [(&'static str, Box<dyn Areas>)],
    cache: &Cache,
    profile: Option<&str>,
    owner: Option<&Owner>,
    notify: &Notifier,
    mut cmd: Command,
) -> Option<Command> {
    for (_, a) in areas.iter_mut() {
        let ctx = AreaCtx { cache, profile, owner, notify: &|e| notify.send(e) };
        match a.handle(cmd, ctx).await {
            None => return None,
            Some(back) => cmd = back,
        }
    }
    Some(cmd)
}

/// Отправить событие и сказать, что дело сделано (`true`): общий хвост у ответа на приглашение и поручения.
fn send_and_done(notify: &Notifier, event: Event) -> bool {
    notify.send(event);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Мост: `Features` почтового пути уходит объединёнными с областями, остальные события — как есть.
    #[test]
    fn bridge_merges_features_of_mail_and_areas() {
        let (tx, rx) = std::sync::mpsc::channel();
        let both = Arc::new(Mutex::new(Both { mail: Features::NONE, areas: Features::NONE }));
        let mail = Features::NONE.with(Feature::Keywords, true);
        let calendar = Features::NONE.with(Feature::Calendar, true);
        lock(&both).areas = calendar;
        let forward = Both::forwarder(Arc::clone(&both), tx);
        forward(Event::Features(mail));
        forward(Event::CalendarChanged);
        assert_eq!(rx.recv(), Ok(Event::Features(mail.union(calendar))), "почта + области");
        assert_eq!(rx.recv(), Ok(Event::CalendarChanged), "остальное — как есть");
    }
}
