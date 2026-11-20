//! Пакет MSI (решение владельца 2026-11-07): установка для всех пользователей в `Program Files\Postman`.
//!
//! Без своих окон: Windows Installer показывает базовый ход установки и запрос UAC. Ставит файлы комплекта
//! (в том числе набор плагинов `postman-plugins.exe`, PLG-03, ADR 0013), ярлык «Postman» в «Пуске», источник
//! «Postman» журнала «Приложение» (как `install.ps1 -RegisterEventSource`).
//! Новая версия снимает старую (большое обновление по `UPGRADE_CODE`), старую поверх новой не ставит.
//! Права для песочниц не нужны: `Program Files` уже читаем для ALL APPLICATION PACKAGES.
//! Каналы Telegram и Matrix ставятся по умолчанию вместе со всем (PLG-13, решение 2026-11-19): обычному
//! пользователю выбирать ничего не надо — неиспользуемый канал не запускается (ADR 0013, плагины по запросу).
//! Состав каналов для сетевой раскатки задаёт свойство `CHANNELS` (`Telegram,Matrix`; `none` или пустое — без
//! каналов; не задано — все), выбор переживает обновления (маркеры в реестре), `REMOVE=имя` снимает канал.
//! Данные пользователя (`%LOCALAPPDATA%\Postman`) и сохранённый вход удаление не трогает; профили песочницы
//! (AppContainer `Postman.MailWorker` и сетевой `Postman.Plugins`) снимает (PAR-05, ADR 0011, ADR 0013).

use std::fs;
use std::io::{Cursor, Write};
use std::path::Path;

use anyhow::{Context, Result, bail};
use msi::{Category, CodePage, Column, Insert, Package, PackageType, Value};
use sha2::{Digest, Sha256};

use crate::ids::{UPGRADE_CODE, component_code, msi_version, product_code, sha256_hex, uuid};
use crate::pe;

/// Производитель в «Приложениях» и в winget — семейство Pets (как AUMID `Pets.Postman`).
pub const MANUFACTURER: &str = "Pets";
const PRODUCT: &str = "Postman";
const CABINET: &str = "postman.cab";
const ICON: &str = "postman.ico";
const ICON_DATA: &[u8] = include_bytes!("../../../app-gpui/res/postman.ico");
const EVENT_KEY: &str = r"SYSTEM\CurrentControlSet\Services\EventLog\Application\Postman";
/// Файл сообщений .NET Framework 4 — «%1» для любого кода: журнал показывает текст Postman как есть.
const EVENT_MESSAGES: &str = r"#%%SystemRoot%\Microsoft.NET\Framework64\v4.0.30319\EventLogMessages.dll";
/// Компонент 64-битный: реестр и папки — без перенаправления WOW64.
const COMPONENT_64BIT: i32 = 256;
const FILE_VITAL: i32 = 512;
/// 34 — exe с командной строкой из формат-строки (`Target`), `Source` — ключ `Directory` (рабочая папка);
/// +64 (msidbCustomActionTypeContinue) — код возврата не важен: отказ не ломает удаление. Без отложенного —
/// immediate, от имени пользователя: профиль AppContainer в его HKCU, SYSTEM его не видит. Не Type 18: в
/// сессии снятия старой версии (RemoveExistingProducts) exe из таблицы File «не помечен к установке» —
/// фатальный 2753; формат-путь `[#worker.exe]` резолвится в установленный файл всегда.
const CA_EXE_IGNORE: i32 = 34 + 64;
/// Только полное удаление; при обновлении (старую версию снимает новая) профиль нужен дальше.
const ON_REMOVE: &str = "REMOVE~=\"ALL\" AND NOT UPGRADINGPRODUCTCODE";
/// Ключ-маркер состава каналов (PLG-13): значение с именем фичи — канал установлен; нет значения — канал не
/// ставили или сняли. Его читает `AppSearch` (`PREV<КАНАЛ>`), чтобы состав пережил большое обновление в обе
/// стороны: что было выключено — остаётся выключенным, что было — ставится.
const CHANNELS_KEY: &str = r"SOFTWARE\Pets\Postman\Channels";
/// Каналы-опции (PLG-13): имя фичи MSI и файл набора из `ITEMS`. Ставятся по умолчанию (уровень 1),
/// ограничивает состав свойство `CHANNELS`.
const CHANNELS: [(&str, &str); 2] = [("Telegram", "plugins_telegram.exe"), ("Matrix", "plugins_matrix.exe")];
/// 4 — ключевой путь компонента — запись реестра (`msidbComponentAttributesRegistryKeyPath`).
const REGISTRY_KEYPATH: i32 = 4;

/// Файл комплекта: ключ в таблицах (идентификатор MSI), имя в комплекте и на диске (`короткое|длинное`).
struct Item {
    key: &'static str,
    name: &'static str,
    file_name: &'static str,
}

const ITEMS: [Item; 7] = [
    Item { key: "postman.exe", name: "postman.exe", file_name: "postman.exe" },
    Item { key: "worker.exe", name: "postman-worker.exe", file_name: "POSTMA~1.EXE|postman-worker.exe" },
    Item { key: "viewer.exe", name: "postman-viewer.exe", file_name: "POSTMA~2.EXE|postman-viewer.exe" },
    Item { key: "plugins.exe", name: "postman-plugins.exe", file_name: "POSTMA~3.EXE|postman-plugins.exe" },
    Item { key: "plugins_telegram.exe", name: "postman-plugins-telegram.exe", file_name: "POSTMA~4.EXE|postman-plugins-telegram.exe" },
    Item { key: "plugins_matrix.exe", name: "postman-plugins-matrix.exe", file_name: "POSTMA~5.EXE|postman-plugins-matrix.exe" },
    Item { key: "README.txt", name: "README.txt", file_name: "README.txt" },
];

/// Собрать `out` из файлов каталога комплекта `dir` (выход `package.ps1`). Версия — версия сборки (Cargo);
/// у exe она же должна быть в ресурсе версии, иначе комплект от другой сборки. `check_files: false` — только
/// для проверки обновления в CI (`msi-check.ps1`): тот же комплект под другой версией.
pub fn build(dir: &Path, out: &Path, version: &str, check_files: bool) -> Result<()> {
    let msi_ver = msi_version(version)?;
    let core = version.split('-').next().unwrap_or(version);
    let mut files = Vec::new();
    for item in &ITEMS {
        let path = dir.join(item.name);
        if !path.exists() {
            bail!("{} not found in {}", item.name, dir.display());
        }
        let data = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
        let file_version = if item.name.ends_with(".exe") {
            let v = pe::file_version(&data).with_context(|| format!("{}: no version resource", item.name))?;
            if check_files && v != format!("{core}.0") {
                bail!("{}: file version {v}, expected {core}.0 (built by another version)", item.name);
            }
            Some(v)
        } else {
            None
        };
        files.push((item, data, file_version));
    }

    let mut hash = Sha256::new();
    hash.update(version.as_bytes());
    for (item, data, _) in &files {
        hash.update(item.key.as_bytes());
        hash.update(Sha256::digest(data));
    }
    let package_code = uuid(&format!("Pets.Postman package {}", sha256_hex(&hash.finalize())));

    let cab = cabinet(&files)?;
    let file =
        fs::File::options().read(true).write(true).create(true).truncate(true).open(out).with_context(|| format!("create {}", out.display()))?;
    let mut package = Package::create(PackageType::Installer, file)?;
    // UTF-8 (по умолчанию у крейта) Windows Installer в сводке не читает: строки пусты, пакет — «не открыть» (1620).
    // Все строки пакета — латиница.
    package.set_database_codepage(CodePage::Windows1252);
    summary(&mut package, version, package_code);
    tables(&mut package, version, &msi_ver, &files)?;
    package.write_stream(CABINET)?.write_all(&cab)?;
    package.write_stream(&format!("Icon.{ICON}"))?.write_all(ICON_DATA)?;
    package.flush()?;
    Ok(())
}

type Files<'a> = [(&'a Item, Vec<u8>, Option<String>)];

/// Встроенный CAB: файлы под ключами таблицы `File`, в порядке их `Sequence`.
fn cabinet(files: &Files) -> Result<Vec<u8>> {
    let mut builder = cab::CabinetBuilder::new();
    let folder = builder.add_folder(cab::CompressionType::MsZip);
    for (item, _, _) in files {
        folder.add_file(item.key);
    }
    let mut writer = builder.build(Cursor::new(Vec::new()))?;
    let mut i = 0;
    while let Some(mut w) = writer.next_file()? {
        w.write_all(&files[i].1)?;
        i += 1;
    }
    Ok(writer.finish()?.into_inner())
}

fn summary<F: std::io::Read + std::io::Write + std::io::Seek>(package: &mut Package<F>, version: &str, package_code: uuid::Uuid) {
    let s = package.summary_info_mut();
    s.set_codepage(CodePage::Windows1252);
    s.set_title("Installation Database");
    s.set_subject(PRODUCT);
    s.set_author(MANUFACTURER);
    s.set_comments(format!("{PRODUCT} {version}: mail, calendar and contacts"));
    s.set_creating_application("postman-pack");
    s.set_arch("x64");
    s.set_languages(&[msi::LanguageId::from_id(1033)]);
    s.set_uuid(package_code);
    // 500 — схема Windows Installer 5 (x64 требует не меньше 200); 2 — файлы сжаты (в CAB), UAC нужен.
    s.set_page_count(500);
    s.set_word_count(2);
    s.set_doc_security(2);
    s.set_creation_time_to_now();
}

fn s(v: &str) -> Value {
    Value::Str(v.to_owned())
}
fn i(v: i32) -> Value {
    Value::Int(v)
}
const NULL: Value = Value::Null;

/// Компонент маркера канала в реестре: ключевой путь — сама запись (без файла).
fn reg_component(feature: &str) -> String {
    format!("{}_reg", feature.to_lowercase())
}

fn tables<F: std::io::Read + std::io::Write + std::io::Seek>(p: &mut Package<F>, version: &str, msi_ver: &str, files: &Files) -> Result<()> {
    let product_code = product_code(version);
    let main = "postman.exe";

    p.create_table("Property", vec![Column::build("Property").primary_key().id_string(72), Column::build("Value").localizable().text_string(0)])?;
    let props = [
        ("ProductCode", product_code.as_str()),
        ("ProductName", PRODUCT),
        ("ProductVersion", msi_ver),
        ("ProductLanguage", "1033"),
        ("Manufacturer", MANUFACTURER),
        ("UpgradeCode", UPGRADE_CODE),
        ("ALLUSERS", "1"),
        ("ARPPRODUCTICON", ICON),
        ("ARPNOMODIFY", "1"),
        ("ARPURLINFOABOUT", "https://github.com/Dicobraz421/Postman"),
        ("ARPHELPLINK", "https://github.com/Dicobraz421/Postman/blob/main/docs/USER-GUIDE.md"),
        ("ARPCOMMENTS", "Mail, calendar and contacts"),
        // CHANNELS — с командной строки; PREV* — из `AppSearch` (попадают на повышенную часть установки).
        ("SecureCustomProperties", "INSTALLDIR;NEWERFOUND;OLDERFOUND;CHANNELS;PREVTELEGRAM;PREVMATRIX"),
    ];
    p.insert_rows(Insert::into("Property").rows(props.iter().map(|(k, v)| vec![s(k), s(v)]).collect()))?;

    p.create_table(
        "Directory",
        vec![
            Column::build("Directory").primary_key().id_string(72),
            Column::build("Directory_Parent").nullable().id_string(72),
            Column::build("DefaultDir").localizable().category(Category::DefaultDir).string(255),
        ],
    )?;
    p.insert_rows(Insert::into("Directory").rows(vec![
        vec![s("TARGETDIR"), NULL, s("SourceDir")],
        vec![s("ProgramFiles64Folder"), s("TARGETDIR"), s(".")],
        vec![s("INSTALLDIR"), s("ProgramFiles64Folder"), s(PRODUCT)],
        vec![s("ProgramMenuFolder"), s("TARGETDIR"), s(".")],
    ]))?;

    p.create_table(
        "Component",
        vec![
            Column::build("Component").primary_key().id_string(72),
            Column::build("ComponentId").nullable().category(Category::Guid).string(38),
            Column::build("Directory_").id_string(72),
            Column::build("Attributes").int16(),
            Column::build("Condition").nullable().category(Category::Condition).string(255),
            Column::build("KeyPath").nullable().id_string(72),
        ],
    )?;
    let mut component_rows: Vec<Vec<Value>> = files
        .iter()
        .map(|(item, _, _)| vec![s(item.key), s(&component_code(item.name)), s("INSTALLDIR"), i(COMPONENT_64BIT), NULL, s(item.key)])
        .collect();
    // Маркер канала в реестре — ключевой путь своего компонента (без файла): сняли канал — исчезло значение.
    for (name, _) in &CHANNELS {
        let r = reg_component(name);
        component_rows.push(vec![s(&r), s(&component_code(&r)), s("INSTALLDIR"), i(COMPONENT_64BIT | REGISTRY_KEYPATH), NULL, s(&r)]);
    }
    p.insert_rows(Insert::into("Component").rows(component_rows))?;

    p.create_table(
        "Feature",
        vec![
            Column::build("Feature").primary_key().id_string(38),
            Column::build("Feature_Parent").nullable().id_string(38),
            Column::build("Title").nullable().localizable().text_string(64),
            Column::build("Description").nullable().localizable().text_string(255),
            Column::build("Display").nullable().int16(),
            Column::build("Level").int16(),
            Column::build("Directory_").nullable().id_string(72),
            Column::build("Attributes").int16(),
        ],
    )?;
    // Уровень 1 у всех: по умолчанию ставится всё, включая каналы (решение 2026-11-19); состав ограничивает
    // CHANNELS (не входит в уровень — входит в REMOVE у своих действий).
    let mut feature_rows = vec![vec![s("Main"), NULL, s(PRODUCT), NULL, i(1), i(1), s("INSTALLDIR"), i(0)]];
    for (n, (name, _)) in CHANNELS.iter().enumerate() {
        feature_rows.push(vec![s(name), NULL, s(name), NULL, i(n as i32 + 2), i(1), s("INSTALLDIR"), i(0)]);
    }
    p.insert_rows(Insert::into("Feature").rows(feature_rows))?;

    p.create_table(
        "FeatureComponents",
        vec![Column::build("Feature_").primary_key().id_string(38), Column::build("Component_").primary_key().id_string(72)],
    )?;
    let mut feature_components: Vec<Vec<Value>> = files
        .iter()
        .map(|(item, _, _)| {
            let feature = CHANNELS.iter().find(|(_, key)| *key == item.key).map_or("Main", |(name, _)| name);
            vec![s(feature), s(item.key)]
        })
        .collect();
    for (name, _) in &CHANNELS {
        feature_components.push(vec![s(name), s(&reg_component(name))]);
    }
    p.insert_rows(Insert::into("FeatureComponents").rows(feature_components))?;

    p.create_table(
        "File",
        vec![
            Column::build("File").primary_key().id_string(72),
            Column::build("Component_").id_string(72),
            Column::build("FileName").localizable().category(Category::Filename).string(255),
            Column::build("FileSize").int32(),
            Column::build("Version").nullable().category(Category::Version).string(72),
            Column::build("Language").nullable().category(Category::Language).string(20),
            Column::build("Attributes").nullable().int16(),
            Column::build("Sequence").int32(),
        ],
    )?;
    let mut rows = Vec::new();
    for (n, (item, data, ver)) in files.iter().enumerate() {
        let size = i32::try_from(data.len()).context("file over 2 GiB")?;
        let attrs = if ver.is_some() { i(FILE_VITAL) } else { i(0) };
        let ver = ver.as_deref().map_or(NULL, s);
        rows.push(vec![s(item.key), s(item.key), s(item.file_name), i(size), ver, NULL, attrs, i(n as i32 + 1)]);
    }
    p.insert_rows(Insert::into("File").rows(rows))?;

    p.create_table(
        "Media",
        vec![
            Column::build("DiskId").primary_key().int16(),
            Column::build("LastSequence").int32(),
            Column::build("DiskPrompt").nullable().localizable().text_string(64),
            Column::build("Cabinet").nullable().category(Category::Cabinet).string(255),
            Column::build("VolumeLabel").nullable().text_string(32),
            Column::build("Source").nullable().category(Category::Property).string(72),
        ],
    )?;
    p.insert_rows(Insert::into("Media").row(vec![i(1), i(files.len() as i32), NULL, s(&format!("#{CABINET}")), NULL, NULL]))?;

    p.create_table(
        "Shortcut",
        vec![
            Column::build("Shortcut").primary_key().id_string(72),
            Column::build("Directory_").id_string(72),
            Column::build("Name").localizable().category(Category::Filename).string(128),
            Column::build("Component_").id_string(72),
            Column::build("Target").category(Category::Shortcut).string(72),
            Column::build("Arguments").nullable().formatted_string(255),
            Column::build("Description").nullable().localizable().text_string(255),
            Column::build("Hotkey").nullable().int16(),
            Column::build("Icon_").nullable().id_string(72),
            Column::build("IconIndex").nullable().int16(),
            Column::build("ShowCmd").nullable().int16(),
            Column::build("WkDir").nullable().id_string(72),
        ],
    )?;
    p.insert_rows(Insert::into("Shortcut").row(vec![
        s("StartMenu"),
        s("ProgramMenuFolder"),
        s(PRODUCT),
        s(main),
        s(&format!("[#{main}]")),
        NULL,
        s("Mail, calendar and contacts"),
        NULL,
        NULL,
        NULL,
        NULL,
        s("INSTALLDIR"),
    ]))?;

    p.create_table(
        "Registry",
        vec![
            Column::build("Registry").primary_key().id_string(72),
            Column::build("Root").int16(),
            Column::build("Key").localizable().category(Category::RegPath).string(255),
            Column::build("Name").nullable().localizable().formatted_string(255),
            Column::build("Value").nullable().localizable().formatted_string(0),
            Column::build("Component_").id_string(72),
        ],
    )?;
    // Root 2 — HKEY_LOCAL_MACHINE; имя «-» без значения — при удалении снять ключ целиком (маркеры каналов —
    // при полном удалении; при снятии одного канала ключ живёт, уходят только его значения).
    let mut registry_rows = vec![
        vec![s("EventSource"), i(2), s(EVENT_KEY), s("EventMessageFile"), s(EVENT_MESSAGES), s(main)],
        vec![s("EventSourceKey"), i(2), s(EVENT_KEY), s("-"), NULL, s(main)],
        vec![s("ChannelsKey"), i(2), s(CHANNELS_KEY), s("-"), NULL, s(main)],
    ];
    // Маркер канала: значение с именем фичи устанавливает компонент маркера, при снятии канала исчезает.
    for (name, _) in &CHANNELS {
        let r = reg_component(name);
        registry_rows.push(vec![s(&r), i(2), s(CHANNELS_KEY), s(name), s(name), s(&r)]);
    }
    p.insert_rows(Insert::into("Registry").rows(registry_rows))?;

    p.create_table("Icon", vec![Column::build("Name").primary_key().id_string(72), Column::build("Data").binary()])?;
    p.insert_rows(Insert::into("Icon").row(vec![s(ICON), Value::Binary]))?;

    p.create_table(
        "Upgrade",
        vec![
            Column::build("UpgradeCode").primary_key().category(Category::Guid).string(38),
            Column::build("VersionMin").primary_key().nullable().category(Category::Text).string(20),
            Column::build("VersionMax").primary_key().nullable().category(Category::Text).string(20),
            Column::build("Language").primary_key().nullable().category(Category::Language).string(255),
            Column::build("Attributes").primary_key().int32(),
            Column::build("Remove").nullable().formatted_string(255),
            Column::build("ActionProperty").category(Category::UpperCase).string(72),
        ],
    )?;
    // Старше этой — снять (VersionMax не включительно); новее — только найти, чтобы не ставить поверх (2 — OnlyDetect).
    p.insert_rows(Insert::into("Upgrade").rows(vec![
        vec![s(UPGRADE_CODE), NULL, s(msi_ver), NULL, i(0), NULL, s("OLDERFOUND")],
        vec![s(UPGRADE_CODE), s(msi_ver), NULL, NULL, i(2), NULL, s("NEWERFOUND")],
    ]))?;

    // Состав каналов прошлого установки (PLG-13): маркер есть — канал стоял; после FindRelatedProducts,
    // до расчёта стоимости.
    p.create_table(
        "RegLocator",
        vec![
            Column::build("Signature_").primary_key().id_string(72),
            Column::build("Root").int16(),
            Column::build("Key").localizable().category(Category::RegPath).string(255),
            Column::build("Name").nullable().id_string(72),
            Column::build("Type").int16(),
        ],
    )?;
    // Root 2 — HKLM; тип 18 = сырое значение (2) из 64-битного представления (16): маркеры пишут 64-битные
    // компоненты, а без 16 движок читает WOW6432Node и на обновлении не находит их (KeepNo срабатывает зря).
    p.insert_rows(
        Insert::into("RegLocator")
            .rows(CHANNELS.iter().map(|(name, _)| vec![s(&format!("{name}PrevLoc")), i(2), s(CHANNELS_KEY), s(name), i(18)]).collect()),
    )?;
    p.create_table(
        "AppSearch",
        // Первая колонка — ровно `Property` (без подчёркивания): её имя спрашивает действие AppSearch
        // (`SELECT `Property`, `Signature_` FROM `AppSearch``); не совпало — 2235 и 1603 (диагностика CI).
        vec![Column::build("Property").primary_key().id_string(72), Column::build("Signature_").primary_key().id_string(72)],
    )?;
    p.insert_rows(
        Insert::into("AppSearch")
            .rows(CHANNELS.iter().map(|(name, _)| vec![s(&format!("PREV{}", name.to_uppercase())), s(&format!("{name}PrevLoc"))]).collect()),
    )?;
    // AppSearch зондирует таблицу `Signature` на каждом поиске: отсутствие самой таблицы — фатальные 2228 и
    // 1603 даже для чисто реестровых поисков (go-msi SEARCH-03); пустой таблицы достаточно.
    p.create_table(
        "Signature",
        vec![
            Column::build("Signature").primary_key().id_string(72),
            Column::build("FileName").nullable().category(Category::WildCardFilename).string(255),
            Column::build("MinVersion").nullable().category(Category::Version).string(20),
            Column::build("MaxVersion").nullable().category(Category::Version).string(20),
            Column::build("MinSize").nullable().category(Category::DoubleInteger).int16(),
            Column::build("MaxSize").nullable().category(Category::DoubleInteger).int16(),
            Column::build("MinDate").nullable().category(Category::DoubleInteger).int16(),
            Column::build("MaxDate").nullable().category(Category::DoubleInteger).int16(),
            Column::build("Languages").nullable().category(Category::Language).string(128),
        ],
    )?;

    p.create_table(
        "CustomAction",
        vec![
            Column::build("Action").primary_key().id_string(72),
            Column::build("Type").int16(),
            Column::build("Source").nullable().category(Category::CustomSource).string(72),
            Column::build("Target").nullable().formatted_string(255),
        ],
    )?;
    // 19 — ошибка с текстом и отказ; 51 — задать свойство (путь установки для «Приложений» и winget; состав
    // каналов: имя фичи — в REMOVE, если CHANNELS его не назвал или канал стоял выключенным).
    let mut action_rows = vec![
        vec![s("PreventDowngrade"), i(19), NULL, s("A newer version of Postman is already installed.")],
        vec![s("SetInstallLocation"), i(51), s("ARPINSTALLLOCATION"), s("[INSTALLDIR]")],
        vec![s("RemoveAppContainer"), i(CA_EXE_IGNORE), s("INSTALLDIR"), s("\"[#worker.exe]\" --remove-appcontainer")],
    ];
    // Состав каналов (PLG-13, пересмотр 2026-11-20): в свежей транзакции (новый код продукта, есть
    // OLDERFOUND) REMOVE трогать нельзя — InstallValidate при большом обновлении переписывает непустой
    // REMOVE в «ALL», транзакция вырождается в обслуживание («Skipping RemoveExistingProducts: maintenance
    // mode or an uninstall»: старая версия не снимается, новая не регистрируется). Состав на обновлении
    // задаёт ADDLOCAL — эксклюзивный список устанавливаемого: основа Main, названные в CHANNELS каналы
    // дописываются; без CHANNELS состав берётся из маркеров прошлой установки (четыре взаимоисключающих
    // варианта). REMOVE остаётся только для обслуживания того же кода (CHANNELS без OLDERFOUND), где он
    // безопасен, — после CostFinalize (доступно состояние фичи) и до InstallValidate.
    let mut channel_actions = Vec::new();
    action_rows.push(vec![s("SetAddLocalMain"), i(51), s("ADDLOCAL"), s("Main")]);
    channel_actions.push(("SetAddLocalMain".into(), "CHANNELS AND NOT ADDLOCAL".into(), 940));
    for (n, (name, _)) in CHANNELS.iter().enumerate() {
        let action = format!("AddLocal{name}");
        action_rows.push(vec![s(&action), i(51), s("ADDLOCAL"), s(&format!("[ADDLOCAL],{name}"))]);
        channel_actions.push((action, format!("CHANNELS AND (CHANNELS >< \"{name}\")"), 941 + n as i32));
    }
    let preserves = [
        ("KeepMain", "NOT PREVTELEGRAM AND NOT PREVMATRIX", "Main"),
        ("KeepTelegram", "PREVTELEGRAM AND NOT PREVMATRIX", "Main,Telegram"),
        ("KeepMatrix", "NOT PREVTELEGRAM AND PREVMATRIX", "Main,Matrix"),
        ("KeepBoth", "PREVTELEGRAM AND PREVMATRIX", "Main,Telegram,Matrix"),
    ];
    for (n, (name, markers, target)) in preserves.iter().enumerate() {
        action_rows.push(vec![s(name), i(51), s("ADDLOCAL"), s(target)]);
        channel_actions.push(((*name).into(), format!("OLDERFOUND AND NOT CHANNELS AND {markers} AND NOT ADDLOCAL"), 943 + n as i32));
    }
    // REMOVE — только обслуживание того же кода (свойство Installed задано, свежая транзакция исключена —
    // см. выше), и строго ДО CostFinalize: после него состав уже посчитан, поздний REMOVE на выбор фич не
    // влияет. Канал стоит ⟺ его маркер прошлой установки найден (PREV*). Список через запятую: пустое и
    // непустое значения дописываются разными действиями, иначе пустой токен — отказ CostFinalize (2711);
    // [~] для этого не годится — это NUL-разделитель REG_MULTI_SZ, не списков свойств.
    for (n, (name, _)) in CHANNELS.iter().enumerate() {
        let no = format!(
            "CHANNELS AND NOT OLDERFOUND AND Installed AND PREV{} AND NOT (CHANNELS >< \"{name}\") AND NOT (REMOVE >< \"{name}\")",
            name.to_uppercase()
        );
        let seq = 950 + 2 * n as i32;
        for (m, (kind, extra, target)) in
            [("Empty", "AND NOT REMOVE", name.to_string()), ("Join", "AND REMOVE", format!("[REMOVE],{name}"))].iter().enumerate()
        {
            let action = format!("No{name}{kind}");
            action_rows.push(vec![s(&action), i(51), s("REMOVE"), s(target)]);
            channel_actions.push((action, format!("{no} {extra}"), seq + m as i32));
        }
    }
    p.insert_rows(Insert::into("CustomAction").rows(action_rows))?;
    let mut ui = vec![
        ("FindRelatedProducts", None, 25),
        ("AppSearch", None, 100),
        ("PreventDowngrade", Some("NEWERFOUND"), 30),
        ("CostInitialize", None, 800),
        ("FileCost", None, 900),
        ("CostFinalize", None, 1000),
        ("ExecuteAction", None, 1300),
    ];
    ui.extend(channel_actions.iter().map(|(a, c, n)| (a.as_str(), Some(c.as_str()), *n)));
    let mut execute = vec![
        ("FindRelatedProducts", None, 25),
        ("AppSearch", None, 100),
        ("PreventDowngrade", Some("NEWERFOUND"), 30),
        ("CostInitialize", None, 800),
        ("FileCost", None, 900),
        ("CostFinalize", None, 1000),
        ("SetInstallLocation", None, 1001),
        ("InstallValidate", None, 1400),
        // До сценария удаления: файл воркера ещё на месте.
        ("RemoveAppContainer", Some(ON_REMOVE), 1450),
        ("InstallInitialize", None, 1500),
        ("RemoveExistingProducts", None, 1501),
        ("ProcessComponents", None, 1600),
        ("UnpublishFeatures", None, 1800),
        ("RemoveRegistryValues", None, 2600),
        ("RemoveShortcuts", None, 3200),
        ("RemoveFiles", None, 3500),
        ("InstallFiles", None, 4000),
        ("CreateShortcuts", None, 4500),
        ("WriteRegistryValues", None, 5000),
        ("RegisterUser", None, 6000),
        ("RegisterProduct", None, 6100),
        ("PublishFeatures", None, 6300),
        ("PublishProduct", None, 6400),
        ("InstallFinalize", None, 6600),
    ];
    execute.extend(channel_actions.iter().map(|(a, c, n)| (a.as_str(), Some(c.as_str()), *n)));
    let admin_ui = [("CostInitialize", None, 800), ("FileCost", None, 900), ("CostFinalize", None, 1000), ("ExecuteAction", None, 1300)];
    let admin = [
        ("CostInitialize", None, 800),
        ("FileCost", None, 900),
        ("CostFinalize", None, 1000),
        ("InstallValidate", None, 1400),
        ("InstallInitialize", None, 1500),
        ("InstallAdminPackage", None, 3900),
        ("InstallFiles", None, 4000),
        ("InstallFinalize", None, 6600),
    ];
    for (table, actions) in [
        ("InstallUISequence", &ui[..]),
        ("InstallExecuteSequence", &execute[..]),
        ("AdminUISequence", &admin_ui[..]),
        ("AdminExecuteSequence", &admin[..]),
    ] {
        p.create_table(
            table,
            vec![
                Column::build("Action").primary_key().id_string(72),
                Column::build("Condition").nullable().category(Category::Condition).string(255),
                Column::build("Sequence").nullable().int16(),
            ],
        )?;
        p.insert_rows(Insert::into(table).rows(actions.iter().map(|&(a, c, n)| vec![s(a), c.map_or(NULL, s), i(n)]).collect()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use msi::Select;

    /// Exe с ресурсом версии `0.1.0.0` — как у сборки версии `0.1.0-beta.2`.
    fn fake_exe(major_minor_patch: (u32, u32, u32)) -> Vec<u8> {
        let (a, b, c) = major_minor_patch;
        let mut image = b"MZ fake image ".to_vec();
        image.extend([0xBD, 0x04, 0xEF, 0xFE, 0, 0, 1, 0]);
        image.extend(((a << 16) | b).to_le_bytes());
        image.extend((c << 16).to_le_bytes());
        image.extend([0u8; 32]);
        image
    }

    fn kit(dir: &Path, v: (u32, u32, u32)) {
        for name in [
            "postman.exe",
            "postman-worker.exe",
            "postman-viewer.exe",
            "postman-plugins.exe",
            "postman-plugins-telegram.exe",
            "postman-plugins-matrix.exe",
        ] {
            fs::write(dir.join(name), fake_exe(v)).unwrap();
        }
        fs::write(dir.join("README.txt"), "Postman\r\n").unwrap();
    }

    fn property(p: &mut Package<fs::File>, name: &str) -> String {
        let rows = p.select_rows(Select::table("Property")).unwrap();
        let row = rows.into_iter().find(|r| r[0].as_str() == Some(name)).unwrap_or_else(|| panic!("no {name}"));
        row[1].as_str().unwrap().to_owned()
    }

    #[test]
    fn builds_installer_with_kit_files() {
        let tmp = tempdir();
        let dir = tmp.path();
        kit(dir, (0, 1, 0));
        let out = dir.join("postman.msi");
        build(dir, &out, "0.1.0-beta.2", true).unwrap();

        let mut p = msi::open(&out).unwrap();
        assert_eq!(p.summary_info().arch(), Some("x64"));
        assert_eq!(property(&mut p, "ProductVersion"), "0.1.302");
        assert_eq!(property(&mut p, "ProductCode"), product_code("0.1.0-beta.2"));
        assert_eq!(property(&mut p, "UpgradeCode"), UPGRADE_CODE);
        assert_eq!(property(&mut p, "ALLUSERS"), "1");
        let names: Vec<String> = p.select_rows(Select::table("File")).unwrap().map(|r| r[2].as_str().unwrap().to_owned()).collect();
        assert_eq!(names.len(), 7);
        assert!(names.contains(&"POSTMA~1.EXE|postman-worker.exe".to_owned()), "{names:?}");
        assert!(names.contains(&"POSTMA~3.EXE|postman-plugins.exe".to_owned()), "{names:?}");
        assert!(p.has_stream(CABINET));
        assert!(p.has_stream(&format!("Icon.{ICON}")));

        // PLG-13: каналы — фичи уровня 1 (по умолчанию ставятся со всем), у каждого свой компонент-маркер.
        let features: Vec<(String, i32)> =
            p.select_rows(Select::table("Feature")).unwrap().map(|r| (r[0].as_str().unwrap().to_owned(), r[5].as_int().unwrap())).collect();
        assert_eq!(features, vec![("Main".into(), 1), ("Telegram".into(), 1), ("Matrix".into(), 1)], "{features:?}");
        let fc: Vec<(String, String)> = p
            .select_rows(Select::table("FeatureComponents"))
            .unwrap()
            .map(|r| (r[0].as_str().unwrap().to_owned(), r[1].as_str().unwrap().to_owned()))
            .collect();
        for pair in [
            ("Main", "postman.exe"),
            ("Main", "worker.exe"),
            ("Telegram", "plugins_telegram.exe"),
            ("Matrix", "plugins_matrix.exe"),
            ("Telegram", "telegram_reg"),
            ("Matrix", "matrix_reg"),
        ] {
            assert!(fc.contains(&(pair.0.into(), pair.1.into())), "{pair:?} not in {fc:?}");
        }
        assert_eq!(fc.len(), 9);
        let registry: Vec<String> = p.select_rows(Select::table("Registry")).unwrap().map(|r| r[2].as_str().unwrap().to_owned()).collect();
        assert!(registry.iter().all(|key| key == EVENT_KEY || key == CHANNELS_KEY), "{registry:?}");
        // `AppSearch` читает маркеры в PREV* до расчёта стоимости; действия пишут имя фичи в REMOVE.
        let search: Vec<(String, String)> = p
            .select_rows(Select::table("AppSearch"))
            .unwrap()
            .map(|r| (r[0].as_str().unwrap().to_owned(), r[1].as_str().unwrap().to_owned()))
            .collect();
        assert_eq!(search, vec![("PREVTELEGRAM".into(), "TelegramPrevLoc".into()), ("PREVMATRIX".into(), "MatrixPrevLoc".into())]);
        // AppSearch требует саму таблицу `Signature` (даже пустую): без неё — 2228 и 1603 (go-msi SEARCH-03).
        assert!(p.tables().any(|t| t.name() == "Signature"), "Signature table missing from the file");
        let secure = property(&mut p, "SecureCustomProperties");
        assert!(secure.contains("CHANNELS") && secure.contains("PREVTELEGRAM") && secure.contains("PREVMATRIX"), "{secure}");
        // Локатор маркеров читает 64-битное представление: тип 18 = сырое значение (2) + 64-битный вид (16).
        let loc = p.select_rows(Select::table("RegLocator")).unwrap().find(|r| r[0].as_str() == Some("TelegramPrevLoc")).unwrap();
        assert_eq!(loc[4].as_int(), Some(18), "TelegramPrevLoc type");
        // Состав каналов: ADDLOCAL — эксклюзивный состав на обновлении (непустой REMOVE в свежей транзакции
        // InstallValidate переписывает в «ALL» и вырождает её в обслуживание); REMOVE — только в обслуживании
        // (Installed + маркер PREV*), строго до CostFinalize (поздний REMOVE на состав не влияет). Пустой
        // токен в списке — отказ 2711.
        let expected = [
            ("SetAddLocalMain", "Main", "CHANNELS AND NOT ADDLOCAL", 940),
            ("AddLocalTelegram", "[ADDLOCAL],Telegram", "CHANNELS >< \"Telegram\"", 941),
            ("AddLocalMatrix", "[ADDLOCAL],Matrix", "CHANNELS >< \"Matrix\"", 942),
            ("KeepMain", "Main", "NOT PREVTELEGRAM AND NOT PREVMATRIX", 943),
            ("KeepTelegram", "Main,Telegram", "PREVTELEGRAM AND NOT PREVMATRIX", 944),
            ("KeepMatrix", "Main,Matrix", "NOT PREVTELEGRAM AND PREVMATRIX", 945),
            ("KeepBoth", "Main,Telegram,Matrix", "PREVTELEGRAM AND PREVMATRIX", 946),
        ];
        for (action, target, cond_part, sequence) in expected {
            let ca = p.select_rows(Select::table("CustomAction")).unwrap().find(|r| r[0].as_str() == Some(action)).unwrap();
            assert_eq!((ca[1].as_int(), ca[2].as_str(), ca[3].as_str()), (Some(51), Some("ADDLOCAL"), Some(target)), "{action}");
            let seq = p.select_rows(Select::table("InstallExecuteSequence")).unwrap().find(|r| r[0].as_str() == Some(action)).unwrap();
            let cond = seq[1].as_str().unwrap();
            assert!(cond.contains(cond_part), "{action}: {cond}");
            assert_eq!(seq[2].as_int(), Some(sequence), "{action}");
        }
        for name in ["Telegram", "Matrix"] {
            for (kind, target) in [("Empty", name), ("Join", format!("[REMOVE],{name}").as_str())] {
                let action = format!("No{name}{kind}");
                let ca = p.select_rows(Select::table("CustomAction")).unwrap().find(|r| r[0].as_str() == Some(action.as_str())).unwrap();
                assert_eq!((ca[1].as_int(), ca[2].as_str(), ca[3].as_str()), (Some(51), Some("REMOVE"), Some(target)), "{action}");
                let seq = p.select_rows(Select::table("InstallExecuteSequence")).unwrap().find(|r| r[0].as_str() == Some(action.as_str())).unwrap();
                let cond = seq[1].as_str().unwrap();
                assert!(
                    cond.contains("NOT OLDERFOUND")
                        && cond.contains("Installed")
                        && cond.contains(&format!("PREV{}", name.to_uppercase()))
                        && cond.contains(&format!("(REMOVE >< \"{name}\")")),
                    "{action}: {cond}"
                );
                assert!((950..1000).contains(&seq[2].as_int().unwrap()), "before CostFinalize: {cond}");
                let ends = if kind == "Empty" { "AND NOT REMOVE" } else { "AND REMOVE" };
                assert!(cond.ends_with(ends), "{cond}");
            }
        }

        // Встроенный CAB читается и содержит файлы под ключами таблицы File.
        let mut cab_bytes = Vec::new();
        std::io::Read::read_to_end(&mut p.read_stream(CABINET).unwrap(), &mut cab_bytes).unwrap();
        let mut cabinet = cab::Cabinet::new(Cursor::new(cab_bytes)).unwrap();
        let mut readme = String::new();
        std::io::Read::read_to_string(&mut cabinet.read_file("README.txt").unwrap(), &mut readme).unwrap();
        assert_eq!(readme, "Postman\r\n");
        assert!(cabinet.read_file("worker.exe").is_ok());

        // PAR-05: при полном удалении воркер снимает профиль AppContainer; отказ не ломает удаление.
        let ca = p.select_rows(Select::table("CustomAction")).unwrap().find(|r| r[0].as_str() == Some("RemoveAppContainer")).unwrap();
        assert_eq!(
            (ca[1].as_int(), ca[2].as_str(), ca[3].as_str()),
            (Some(CA_EXE_IGNORE), Some("INSTALLDIR"), Some("\"[#worker.exe]\" --remove-appcontainer"))
        );
        let seq = p.select_rows(Select::table("InstallExecuteSequence")).unwrap().find(|r| r[0].as_str() == Some("RemoveAppContainer")).unwrap();
        assert_eq!((seq[1].as_str(), seq[2].as_int()), (Some(ON_REMOVE), Some(1450)));
    }

    #[test]
    fn refuses_kit_of_another_version() {
        let tmp = tempdir();
        let dir = tmp.path();
        kit(dir, (0, 2, 0));
        let err = build(dir, &dir.join("postman.msi"), "0.1.0-beta.2", true).unwrap_err();
        // Проверка обновления в CI собирает тот же комплект под другой версией.
        build(dir, &dir.join("postman.msi"), "0.1.0-beta.2", false).unwrap();
        assert!(err.to_string().contains("expected 0.1.0.0"), "{err}");
        kit(dir, (0, 1, 0));
        fs::remove_file(dir.join("postman-viewer.exe")).unwrap();
        assert!(build(dir, &dir.join("postman.msi"), "0.1.0-beta.2", true).is_err());
    }

    /// Windows Installer принимает пакет: административная установка (без прав администратора и без записи
    /// в систему) раскладывает файлы под длинными именами в `Postman\`.
    #[cfg(windows)]
    #[test]
    fn windows_installer_extracts_the_package() {
        let tmp = tempdir();
        let dir = tmp.path();
        kit(dir, (0, 1, 0));
        let out = dir.join("postman.msi");
        build(dir, &out, "0.1.0-beta.2", true).unwrap();
        let target = dir.join("admin");
        let status =
            std::process::Command::new("msiexec.exe").arg("/a").arg(&out).arg("/qn").arg(format!("TARGETDIR={}", target.display())).status().unwrap();
        assert!(status.success(), "msiexec /a: {status}");
        for name in [
            "postman.exe",
            "postman-worker.exe",
            "postman-viewer.exe",
            "postman-plugins.exe",
            "postman-plugins-telegram.exe",
            "postman-plugins-matrix.exe",
            "README.txt",
        ] {
            assert!(target.join("Postman").join(name).is_file(), "{name}");
        }
        assert_eq!(fs::read_to_string(target.join("Postman/README.txt")).unwrap(), "Postman\r\n");
    }

    /// H-01: временные каталоги тестов — `tempfile` с префиксом `postman-`.
    fn tempdir() -> tempfile::TempDir {
        tempfile::Builder::new().prefix("postman-pack").tempdir().unwrap()
    }
}
