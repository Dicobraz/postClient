//! Версия `postman-plugins-telegram.exe` из Cargo (PLG-05) — видна в свойствах файла; MSI сверяет её с версией комплекта
//! (`tools/pack`, `pe::file_version`). Нужен rc.exe из Windows SDK. Значка и манифеста нет: окна у набора нет,
//! запускается только хостом плагинов. Как у воркера (`crates/worker/build.rs`).
//!
//! `user32.dll` — отложенная загрузка: статический импорт тянет её `os_info` (вызовом `GetSystemMetrics` под
//! Windows Server 2003 R2 — через `grammers-mtsender`, сведения об ОС для Telegram), а `user32` не инициализируется
//! под запретом win32k песочницы плагинов (ADR 0013) — процесс падал с 0xC0000142 до первого кадра обмена.
//! Отложенный импорт грузит библиотеку только при вызове — его не бывает (путь 2003 R2 мёртв), песочница цела.

use std::error::Error;
use std::path::Path;
use std::{env, fs};

fn main() -> Result<(), Box<dyn Error>> {
    if env::var("CARGO_CFG_TARGET_OS")? != "windows" {
        return Ok(());
    }
    if env::var("CARGO_CFG_TARGET_ENV")? == "msvc" {
        // Заглушка отложенной загрузки: рантайм delayimp (MSVC).
        println!("cargo:rustc-link-arg=/DELAYLOAD:user32.dll");
        println!("cargo:rustc-link-lib=dylib=delayimp");
    }
    let v = env::var("CARGO_PKG_VERSION")?;
    let n = |k: &str| env::var(k).map_err(|e| format!("{k}: {e}"));
    let comma = format!("{},{},{},0", n("CARGO_PKG_VERSION_MAJOR")?, n("CARGO_PKG_VERSION_MINOR")?, n("CARGO_PKG_VERSION_PATCH")?);
    let rc = Path::new(&env::var("OUT_DIR")?).join("postman-plugins-telegram.rc");
    fs::write(
        &rc,
        format!(
            "LANGUAGE 0, 0\n\
             1 VERSIONINFO\n\
             FILEVERSION {comma}\n\
             PRODUCTVERSION {comma}\n\
             FILEFLAGSMASK 0x3f\n\
             FILEFLAGS 0x0\n\
             FILEOS 0x40004\n\
             FILETYPE 0x1\n\
             FILESUBTYPE 0x0\n\
             BEGIN\n\
               BLOCK \"StringFileInfo\"\n\
               BEGIN\n\
                 BLOCK \"040904B0\"\n\
                 BEGIN\n\
                   VALUE \"FileDescription\", \"Postman Telegram plugin set\"\n\
                   VALUE \"FileVersion\", \"{v}\"\n\
                   VALUE \"InternalName\", \"postman-plugins-telegram.exe\"\n\
                   VALUE \"LegalCopyright\", \"Copyright (c) 2026 Anton Antropov. All rights reserved.\"\n\
                   VALUE \"OriginalFilename\", \"postman-plugins-telegram.exe\"\n\
                   VALUE \"ProductName\", \"Postman\"\n\
                   VALUE \"ProductVersion\", \"{v}\"\n\
                 END\n\
               END\n\
               BLOCK \"VarFileInfo\"\n\
               BEGIN\n\
                 VALUE \"Translation\", 0x409, 1200\n\
               END\n\
             END\n"
        ),
    )?;
    embed_resource::compile(&rc, embed_resource::NONE).manifest_optional()?;
    Ok(())
}
