//! Компоненты каналов (PLG-15, решение владельца 2026-11-19 «components»): файлов наборов Telegram и Matrix может
//! не быть в пакете (PLG-17) — докачивает приложение из **своего** выпуска той же версии. Перечень — список релизов
//! того же репозитория, что у самообновления (`crate::update`); файл — zip без сжатия с exe набора; подпись —
//! minisign теми же ключами (`Updater::verify` крейта обновления: свои файлы качаем `Transport`-ом, проверка —
//! общая). Распаковка — в `<data>\Postman\Components`, откуда хост запускает набор (PLG-14); на Windows каталогу
//! даётся право чтения для AppContainer, как в `install.ps1`. Сеть — только по кнопке пользователя и только на
//! выпуск своей версии (процесс плагина сверяет сборку в `Hello`, чужой файл не заведётся); офлайн — установка из
//! файла (PLG-16). Версия маркером: после самообновления файл прошлого выпуска помечается «обновить» — кнопка
//! та же; канал с файлом чужой сборки поднимается «проблемой» до нажатия.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use gpui_kit::prelude::*;
use gpui_kit::*;
use pets_i18n::tr;
use pets_update::{Cancel, Downloaded, InstallKind, Policy, Transport, Update, UreqTransport, Version, releases_url};

use crate::update;

use crate::roles::Look;
use crate::settings_tab::link;

/// Состояние компонента канала: «на месте» — по файлам на диске, остальное — из модели.
#[derive(Clone, Debug)]
pub(crate) enum State {
    /// Файл есть и той же сборки (в установке или в компонентах) — на экране ничего.
    Present,
    /// Файла нет — можно скачать из выпуска.
    Offered,
    /// Файл есть, но другой версии (после самообновления): скачать заново.
    Stale,
    /// Скачивается: получено/всего байт (общие с потоком работы) и отмена.
    Busy { received: Arc<AtomicU64>, total: Arc<AtomicU64>, cancel: Cancel },
    /// В выпуске этой версии файла нет: канал остаётся «не установлен».
    Absent,
    /// Не удалось (текст для журнала и строки настроек).
    Failed(String),
}

/// Транзитные состояния: скачивание и итоги попыток. «На месте» вычисляется по дискам при каждом показе.
#[derive(Default)]
pub(crate) struct Model {
    states: std::collections::HashMap<String, State>,
}

impl Global for Model {}

fn edit(cx: &mut App, f: impl FnOnce(&mut Model)) {
    if !cx.has_global::<Model>() {
        cx.set_global(Model::default());
    }
    cx.update_global::<Model, _>(|m, _| f(m));
}

/// Состояние компонента для экрана канала.
pub(crate) fn state(set: &'static str, cx: &App) -> State {
    match which(set) {
        Which::Install => State::Present,
        Which::Components => match marker(set) {
            Marker::Current => State::Present,
            Marker::Stale => State::Stale,
        },
        Which::Missing => cx.try_global::<Model>().and_then(|m| m.states.get(set).cloned()).unwrap_or(State::Offered),
    }
}

/// Отмена скачивания.
pub(crate) fn cancel(set: &'static str, cx: &App) {
    let busy = cx.try_global::<Model>().and_then(|m| m.states.get(set).cloned());
    if let Some(State::Busy { cancel, .. }) = busy {
        cancel.cancel();
    }
}

/// Строка компонента на панели канала: `None` — компонент на месте, на экране ничего.
pub(crate) fn row(set: &'static str, st: &State, l: &Look) -> Option<AnyElement> {
    let busy = matches!(st, State::Busy { .. });
    let (text, action) = match st {
        State::Present => return None,
        State::Offered => (tr!("comp-missing"), Some(tr!("comp-download"))),
        State::Stale => (tr!("comp-stale"), Some(tr!("comp-download"))),
        State::Absent => (tr!("comp-missing"), None),
        State::Failed(e) => (tr!("comp-failed", error = e.clone()), Some(tr!("comp-download"))),
        State::Busy { received, total, .. } => {
            let (done, all) = (received.load(Ordering::Relaxed), total.load(Ordering::Relaxed));
            let text = match (done * 100).checked_div(all) {
                Some(pct) => tr!("comp-percent", pct = pct.to_string()),
                None => tr!("comp-busy"),
            };
            (text, Some(tr!("update-cancel")))
        }
    };
    let mut out = div().flex_none().pt_2().text_size(px(12.)).text_color(l.text2()).child(text);
    if let Some(label) = action {
        let id = format!("{}-{set}", if busy { "comp-cancel" } else { "comp-download" });
        out = out.child(link(id, label, l).on_click(move |_, _, cx| if busy { cancel(set, cx) } else { download(set, cx) }));
    }
    Some(out.into_any_element())
}

/// «Скачать компонент»: перечень выпуска своей версии, файл и подпись, проверка, распаковка — в фоне; итог —
/// в модель и «Повторить» плагину (канал поднимется сам). Блокирующая сеть — в отдельном потоке, как в
/// `crate::update`.
pub(crate) fn download(set: &'static str, cx: &mut App) {
    let busy = cx.try_global::<Model>().is_some_and(|m| matches!(m.states.get(set), Some(State::Busy { .. })));
    if busy || update::product().is_none() {
        return;
    }
    let cancel = Cancel::new();
    let received = Arc::new(AtomicU64::new(0));
    let total = Arc::new(AtomicU64::new(0));
    edit(cx, |m| {
        m.states.insert(set.to_owned(), State::Busy { received: Arc::clone(&received), total: Arc::clone(&total), cancel: cancel.clone() });
    });
    let (tx, rx) = futures::channel::oneshot::channel();
    let spawned = std::thread::Builder::new().name(format!("component {set}")).spawn(move || {
        let _ = tx.send(fetch(set, cancel, &received, &total));
    });
    if let Err(e) = spawned {
        tracing::error!(component = set, "component thread not started: {e}");
        edit(cx, |m| {
            m.states.insert(set.to_owned(), State::Failed(format!("thread: {e}")));
        });
        return;
    }
    cx.spawn(async move |cx| {
        if let Ok(result) = rx.await {
            cx.update(|cx| done(set, result, cx));
        }
    })
    .detach();
    // Полоса хода перерисовывается, пока идёт скачивание (как у обновления).
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(std::time::Duration::from_millis(250)).await;
            let going = cx.update(|cx| {
                let going = cx.try_global::<Model>().is_some_and(|m| m.states.values().any(|s| matches!(s, State::Busy { .. })));
                if going {
                    edit(cx, |_| {});
                }
                going
            });
            if !going {
                return;
            }
        }
    })
    .detach();
}

fn done(set: &'static str, result: Result<(), String>, cx: &mut App) {
    edit(cx, |m| {
        m.states.insert(
            set.to_owned(),
            match &result {
                Ok(()) => State::Present,
                Err(e) if e == ABSENT => State::Absent,
                Err(e) => State::Failed(e.clone()),
            },
        );
    });
    match result {
        Ok(()) => {
            tracing::info!(component = set, "component installed");
            crate::plugins::component_ready(set);
        }
        Err(e) => tracing::error!(component = set, %e, "component install failed"),
    }
}

/// Нет файла в выпуске (метка из потока работы).
const ABSENT: &str = "absent from the release";

/// Работа скачивания: exe уже распакован в каталог компонентов.
fn fetch(set: &'static str, cancel: Cancel, received: &AtomicU64, total: &AtomicU64) -> Result<(), String> {
    let product = update::product().ok_or_else(|| "no signing keys in this build".to_owned())?;
    let transport = UreqTransport::new();
    let name = asset_name(set);
    let (url, size) = pick(&list(&transport, product.repo)?, &name).ok_or_else(|| ABSENT.to_owned())?;
    total.store(size, Ordering::Relaxed);
    let dir = dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("create components folder: {e}"))?;
    grant(&dir);
    // Подпись — маленькая, целиком; файл — потоком с отменой и точным размером из перечня.
    let signature = dir.join(format!("{name}.minisig"));
    let mut text = Vec::new();
    let mut sig = transport.get(&format!("{url}.minisig")).map_err(|e| e.to_string())?;
    sig.body.read_to_end(&mut text).map_err(|e| format!("signature: {e}"))?;
    std::fs::write(&signature, &text).map_err(|e| format!("write signature: {e}"))?;
    let part = dir.join(format!("{name}.part"));
    let stream = std::fs::File::create(&part).map_err(|e| format!("create download file: {e}"))?;
    let response = transport.get(&url).map_err(|e| e.to_string())?;
    if let Err(e) = write_stream(response.body, stream, size, &cancel, received) {
        let _ = std::fs::remove_file(&part);
        return Err(e);
    }
    let zip = dir.join(&name);
    std::fs::rename(&part, &zip).map_err(|e| format!("rename download file: {e}"))?;
    // Подпись проверяет крейт обновления вшитыми ключами: свои файлы — своим транспортом, проверка — общая.
    let update = Update {
        version: Version::parse(env!("CARGO_PKG_VERSION")).map_err(|e| format!("version: {e}"))?,
        kind: InstallKind::Msi,
        page_url: String::new(),
        asset_name: name.clone(),
        asset_url: url,
        size,
        signature_url: format!("https://example.invalid/{name}.minisig"),
    };
    let verifier = pets_update::Updater::with_parts(product, InstallKind::Msi, dir.clone(), Policy::default(), Box::new(UreqTransport::new()));
    if let Err(e) = verifier.verify(Downloaded { update, file: zip.clone(), signature: signature.clone() }) {
        return Err(format!("signature: {e}"));
    }
    // Zip без сжатия: внутри — один exe набора.
    let exe = extract(&zip, set).map_err(|e| {
        let _ = std::fs::remove_file(&zip);
        format!("unpack: {e}")
    })?;
    let target = dir.join(format!("{set}{}", std::env::consts::EXE_SUFFIX));
    let staged = dir.join(format!("{set}.part"));
    std::fs::write(&staged, exe).map_err(|e| format!("write component: {e}"))?;
    std::fs::rename(&staged, &target).map_err(|e| format!("rename component: {e}"))?;
    let _ = std::fs::remove_file(&zip);
    let _ = std::fs::remove_file(&signature);
    std::fs::write(dir.join(format!("{set}.txt")), env!("CARGO_PKG_VERSION")).map_err(|e| format!("write marker: {e}"))?;
    Ok(())
}

/// Имя файла компонента в выпуске: `postman-plugins-telegram-windows-x64.zip`.
fn asset_name(set: &str) -> String {
    format!("{set}-{}-{}.zip", pets_update::OS, pets_update::ARCH)
}

/// Каталог компонентов (его читает хост, PLG-14).
fn dir() -> PathBuf {
    postman_core::paths::data_dir().join("Components")
}

/// Перечень выпусков — JSON GitHub API (как у самообновления, но нужен свой выпуск, без фильтра версий).
fn list(transport: &UreqTransport, repo: &str) -> Result<serde_json::Value, String> {
    let mut body = Vec::new();
    let mut response = transport.get(&releases_url(repo)).map_err(|e| e.to_string())?;
    response.body.read_to_end(&mut body).map_err(|e| format!("releases: {e}"))?;
    serde_json::from_slice(&body).map_err(|e| format!("releases: {e}"))
}

/// Свой выпуск (`v<версия>`) и файл набора в нём: адрес и размер. Подпись качается рядом (`<файл>.minisig`).
fn pick(releases: &serde_json::Value, name: &str) -> Option<(String, u64)> {
    let tag = format!("v{}", env!("CARGO_PKG_VERSION"));
    for release in releases.as_array()? {
        if release.get("tag_name")?.as_str()? != tag {
            continue;
        }
        for asset in release.get("assets")?.as_array()? {
            if asset.get("name")?.as_str()? == name {
                return Some((asset.get("browser_download_url")?.as_str()?.to_owned(), asset.get("size")?.as_u64()?));
            }
        }
    }
    None
}

/// Поток в файл с отменой и точным размером (как `fetch` крейта обновления).
fn write_stream(mut body: Box<dyn std::io::Read>, mut out: std::fs::File, total: u64, cancel: &Cancel, received: &AtomicU64) -> Result<(), String> {
    use std::io::Write as _;
    let mut buf = vec![0u8; 64 * 1024];
    let mut got = 0u64;
    loop {
        if cancel.is_cancelled() {
            return Err("cancelled".into());
        }
        let n = body.read(&mut buf).map_err(|e| format!("download: {e}"))?;
        if n == 0 {
            break;
        }
        got += n as u64;
        if got > total {
            return Err(format!("size: got {got}, expected {total}"));
        }
        out.write_all(&buf[..n]).map_err(|e| format!("write download file: {e}"))?;
        received.store(got, Ordering::Relaxed);
    }
    if got != total {
        return Err(format!("size: got {got}, expected {total}"));
    }
    out.sync_all().map_err(|e| format!("write download file: {e}"))?;
    Ok(())
}

/// Извлечь exe набора из zip (крейт `zip` — он же читает вложения): файл набора в корне или в подкаталоге.
fn extract(zip_path: &Path, set: &str) -> Result<Vec<u8>, String> {
    let exe = format!("{set}{}", std::env::consts::EXE_SUFFIX);
    let file = std::fs::File::open(zip_path).map_err(|e| format!("read: {e}"))?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| format!("read: {e}"))?;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|e| format!("read: {e}"))?;
        let name = entry.name().map_err(|e| format!("read: {e}"))?;
        let short = name.rsplit('/').next().unwrap_or(name.as_ref());
        if short == exe {
            let mut out = Vec::with_capacity(entry.size() as usize);
            entry.read_to_end(&mut out).map_err(|e| format!("read: {e}"))?;
            return Ok(out);
        }
    }
    Err(format!("{exe} not found in the archive"))
}

/// Право чтения для AppContainer (`ALL APPLICATION PACKAGES`) — иначе песочница не прочтёт exe
/// (тот же приём в `install.ps1`; ошибка — в журнал, отказ покажет сама песочница).
#[cfg(windows)]
fn grant(dir: &Path) {
    match std::process::Command::new("icacls.exe").arg(dir).args(["/grant", "*S-1-15-2-1:(OI)(CI)(RX)", "/T", "/Q"]).output() {
        Ok(out) if out.status.success() => {}
        Ok(out) => tracing::warn!("component: icacls failed: {}", String::from_utf8_lossy(&out.stderr).trim()),
        Err(e) => tracing::warn!("component: icacls not started: {e}"),
    }
}

#[cfg(not(windows))]
fn grant(_dir: &Path) {}

/// Где лежит файл набора: установка, компоненты или нигде (PLG-14). Каталог программы важнее: там файл своей
/// сборки (его ставит и обновляет установка).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Which {
    Install,
    Components,
    Missing,
}

fn which(set: &str) -> Which {
    let exe = format!("{set}{}", std::env::consts::EXE_SUFFIX);
    let install = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf));
    if install.is_some_and(|d| d.join(&exe).exists()) {
        return Which::Install;
    }
    if dir().join(exe).exists() {
        return Which::Components;
    }
    Which::Missing
}

/// Маркер версии в компонентах: свой выпуск — `Current`; чужой — `Stale`; положенный руками без маркера —
/// `Current` (чужую сборку не пропустит сверка в `Hello`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Marker {
    Current,
    Stale,
}

fn marker(set: &str) -> Marker {
    marker_at(&dir(), set)
}

fn marker_at(base: &Path, set: &str) -> Marker {
    match std::fs::read_to_string(base.join(format!("{set}.txt"))) {
        Ok(v) if v == env!("CARGO_PKG_VERSION") => Marker::Current,
        Ok(_) => Marker::Stale,
        Err(_) => Marker::Current,
    }
}

#[cfg(test)]
mod tests {
    // Без `use super::*`: глоб-импорты поверх двойных глобов gpui_kit уводят раскрутку компилятора в глубину
    // (recursion limit даже на тривиальном тесте) — только явные имена.
    use super::{Marker, asset_name, extract, marker_at, pick};
    use pets_update::{ARCH, OS};
    use serde_json::Value;
    use std::path::Path;

    /// Имя файла компонента — по шаблону выпуска этой ОС (его же собирает упаковка, PLG-17).
    #[test]
    fn asset_name_follows_the_release_layout() {
        assert_eq!(asset_name("postman-plugins-telegram"), format!("postman-plugins-telegram-{}-{}.zip", pets_update::OS, pets_update::ARCH));
    }

    /// Свой выпуск и файл набора в нём: адрес и размер; чужой набор или чужой выпуск — нет.
    #[test]
    fn pick_takes_own_release_asset() {
        let v = env!("CARGO_PKG_VERSION");
        let name = format!("postman-plugins-telegram-{OS}-{ARCH}.zip");
        let release = |tag: String, assets: Vec<serde_json::Value>| {
            let mut o = serde_json::Map::new();
            o.insert("tag_name".into(), Value::String(tag));
            o.insert("assets".into(), Value::Array(assets));
            Value::Object(o)
        };
        let asset = |name: String, url: String, size: u64| {
            let mut o = serde_json::Map::new();
            o.insert("name".into(), Value::String(name));
            o.insert("browser_download_url".into(), Value::String(url));
            o.insert("size".into(), Value::from(size));
            Value::Object(o)
        };
        let json = Value::Array(vec![
            release("v0.0.1".into(), vec![asset("old.zip".into(), "https://x/old.zip".into(), 1)]),
            release(
                format!("v{v}"),
                vec![asset(name.clone(), "https://x/t.zip".into(), 10), asset(format!("{name}.minisig"), "https://x/t.zip.minisig".into(), 1)],
            ),
        ]);
        let (url, size) = pick(&json, &name).unwrap();
        assert_eq!((url.as_str(), size), ("https://x/t.zip", 10));
        assert!(pick(&json, &format!("postman-plugins-matrix-{OS}-{ARCH}.zip")).is_none());
        let other = Value::Array(vec![release("v0.0.1".into(), vec![])]);
        assert!(pick(&other, &name).is_none());
    }

    /// Zip: запись набора читается целиком (сжатие упаковке не важно); чужой файл в архиве — ошибка.
    #[test]
    fn extract_reads_zip_entry() {
        use std::io::Write as _;
        let set = "postman-plugins-telegram";
        let exe = format!("{set}{}", std::env::consts::EXE_SUFFIX);
        let tmp = tempfile::tempdir().unwrap();
        let write = |path: &Path, entries: &[(&str, &[u8])]| {
            let file = std::fs::File::create(path).unwrap();
            let mut w = zip::ZipWriter::new(file);
            for (name, data) in entries {
                w.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
                w.write_all(data).unwrap();
            }
            w.finish().unwrap();
        };
        let path = tmp.path().join("c.zip");
        write(&path, &[("readme.txt", b"hi".as_slice()), (exe.as_str(), b"MZ component".as_slice())]);
        assert_eq!(extract(&path, set).unwrap(), b"MZ component");
        let path = tmp.path().join("n.zip");
        write(&path, &[("other.exe", b"x")]);
        assert!(extract(&path, set).is_err());
    }

    /// Маркер версии: свой — `Current`, чужой — `Stale`; нет маркера — `Current` (положен руками, чужую сборку
    /// не пропустит сверка в `Hello`).
    #[test]
    fn marker_tells_stale_after_update() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(marker_at(tmp.path(), "postman-plugins-telegram"), Marker::Current);
        let m = tmp.path().join("postman-plugins-telegram.txt");
        std::fs::write(&m, env!("CARGO_PKG_VERSION")).unwrap();
        assert_eq!(marker_at(tmp.path(), "postman-plugins-telegram"), Marker::Current);
        std::fs::write(&m, "0.0.1").unwrap();
        assert_eq!(marker_at(tmp.path(), "postman-plugins-telegram"), Marker::Stale);
    }
}
