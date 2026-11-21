//! Модули просмотра WASM (S8-24, ADR 0007, решение A): ответ модуля и его проверка — здесь, хост (`wasmi`) — только
//! в `postman-viewer.exe` (признак `viewer`); в `postman-worker.exe` интерпретатора нет.
//!
//! ABI v1 — ядро модели WASM без компонентов, импортов у модуля нет вовсе (ни WASI, ни своих функций):
//! - `pm_abi() -> i32` — версия ABI, [`ABI`];
//! - `pm_alloc(len: i32) -> i32` — место под вход длины `len` в памяти модуля (экспорт `memory`);
//! - `pm_view(ptr: i32, len: i32) -> i64` — просмотр входа; итог — указатель ответа в старших 32 битах и его длина в
//!   младших; ответ — [`ViewerOutput`] в postcard, не длиннее [`MAX_OUTPUT`].
//!
//! Пределы — на запрос: модуль — [`MAX_MODULE`], вход и память модуля, топливо и ответ — [`Limits`] (заявленные в
//! описании, ужатые до общих констант), время процесса — сторож [`VIEW_LIMIT`]; Job Object процесса —
//! [`JOB_MEMORY`]. Ответ — чужие данные: разделы и блоки — в пределах `.docx`, ссылки — через проверку IR,
//! картинка — не больше [`MAX_SIDE`] по стороне.

use std::time::Duration;

use postman_ir::{Block, Inline};
use serde::{Deserialize, Serialize};

use crate::preview::{self, Section, SectionTitle, docx, sheet, slides};

/// Версия ABI, которую понимает хост.
pub const ABI: u32 = 1;
/// Модуль больше — не загружаем.
pub const MAX_MODULE: usize = 16 * 1024 * 1024;
/// Вход модуля — как у встроенного просмотра.
pub const MAX_INPUT: usize = preview::MAX_INPUT;
/// Память модуля (`StoreLimits`): рост сверх — отказ.
pub const MAX_MEMORY: usize = 256 * 1024 * 1024;
/// Топливо на запрос (порядка 10⁹ инструкций).
pub const FUEL: u64 = 1_000_000_000;
/// Ответ модуля длиннее — отказ.
pub const MAX_OUTPUT: usize = 8 * 1024 * 1024;
/// Картинка больше по стороне — отказ.
pub const MAX_SIDE: u32 = 2048;
/// Разделов в ответе — не больше, чем слайдов у презентации.
pub const MAX_SECTIONS: usize = slides::MAX_SLIDES;
/// Вложенность блоков (цитаты, списки) — как у `.md`.
pub const MAX_NESTING: usize = preview::markdown::MAX_NESTING;
/// Ошибка модуля длиннее — обрезается.
pub const MAX_ERROR: usize = 1000;
/// Сторож времени процесса-хоста: запрос дольше — выход с [`crate::EXIT_TIME`].
pub const VIEW_LIMIT: Duration = Duration::from_secs(10);
/// Сколько клиент ждёт ответа хоста: сторож срабатывает раньше.
pub const VIEW_TIMEOUT: Duration = Duration::from_secs(12);
/// Предел памяти процесса-хоста (Windows — Job Object, macOS — распределитель с пределом).
pub const JOB_MEMORY: usize = 512 * 1024 * 1024;

/// Пределы просмотра модулем — заявленные в описании (ядро, `viewers.rs`, проверяет их против общих ещё на
/// чтении реестра). Хост применяет ужатые до общих констант ([`Limits::clamped`]): модуль может только
/// ужесточать песочницу. Клиент передаёт их хосту в [`Request::ViewModule`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    /// Вход больше — отказ до запуска модуля.
    pub input: usize,
    /// Память модуля (`StoreLimits`): рост сверх — отказ.
    pub memory: usize,
    /// Топливо на просмотр.
    pub fuel: u64,
    /// Ответ длиннее — отказ.
    pub output: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self { input: MAX_INPUT, memory: MAX_MEMORY, fuel: FUEL, output: MAX_OUTPUT }
    }
}

impl Limits {
    /// Общие константы — верхний потолок: заявленное больше общего или нулевое (не заявлено) заменяется общим.
    pub fn clamped(self) -> Self {
        let one = |own: usize, host: usize| if own == 0 || own > host { host } else { own };
        Self {
            input: one(self.input, MAX_INPUT),
            memory: one(self.memory, MAX_MEMORY),
            fuel: if self.fuel == 0 || self.fuel > FUEL { FUEL } else { self.fuel },
            output: one(self.output, MAX_OUTPUT),
        }
    }
}

/// Есть ли у модуля импорты (раздел Import — просьба доступа к системе, у ABI v1 их нет). Реестр разобранного
/// без интерпретатора модуля показывает отказ в настройках; не разобрался — `false`, полную проверку всё равно
/// делает хост.
pub fn has_imports(wasm: &[u8]) -> bool {
    if !wasm.starts_with(b"\0asm\x01\0\0\0") {
        return false;
    }
    let mut r = Reader(&wasm[8..]);
    while let Some((id, body)) = r.section() {
        if id != 2 {
            continue;
        }
        let mut s = Reader(body);
        let Some(count) = s.leb() else { return false };
        if count == 0 {
            return false;
        }
        for _ in 0..count {
            if s.import().is_none() {
                return false;
            }
        }
        return true;
    }
    false
}

/// Читатель байт WASM: срезы и LEB128.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    /// Раздел: номер и тело по длине из заголовка; не поместился — раздел не годится.
    fn section(&mut self) -> Option<(u8, &'a [u8])> {
        let id = self.byte()?;
        let len = usize::try_from(self.leb()?).ok()?;
        let body = self.take(len)?;
        Some((id, body))
    }

    fn byte(&mut self) -> Option<u8> {
        let (&b, rest) = self.0.split_first()?;
        self.0 = rest;
        Some(b)
    }

    /// LEB128 без знака (до 10 байт).
    fn leb(&mut self) -> Option<u64> {
        let mut n = 0u64;
        for shift in (0..64).step_by(7) {
            let b = self.byte()?;
            n |= u64::from(b & 0x7F) << shift;
            if b & 0x80 == 0 {
                return Some(n);
            }
        }
        None
    }

    fn take(&mut self, len: usize) -> Option<&'a [u8]> {
        if len > self.0.len() {
            return None;
        }
        let (body, rest) = self.0.split_at(len);
        self.0 = rest;
        Some(body)
    }

    /// Импорт: два имени и дескриптор (функция, таблица, память или глобальная переменная).
    fn import(&mut self) -> Option<()> {
        let name = usize::try_from(self.leb()?).ok()?;
        self.take(name)?;
        let field = usize::try_from(self.leb()?).ok()?;
        self.take(field)?;
        match self.byte()? {
            // Функция: индекс типа.
            0 => {
                self.leb()?;
            }
            // Таблица: тип элемента и пределы.
            1 => {
                self.byte()?;
                self.limits()?;
            }
            // Память: пределы.
            2 => {
                self.limits()?;
            }
            // Глобальная переменная: тип и изменяемость.
            3 => {
                self.byte()?;
                self.byte()?;
            }
            _ => return None,
        }
        Some(())
    }

    /// Пределы: минимум и, с флагом 1, максимум (флаг 2 — общая у потоков, дескриптор тот же).
    fn limits(&mut self) -> Option<()> {
        let flags = self.byte()?;
        self.leb()?;
        if flags & 1 != 0 {
            self.leb()?;
        }
        Some(())
    }
}

/// Ответ модуля (ABI v1, postcard). Типы — общие с SDK модулей (`viewers/sdk`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ViewerOutput {
    /// Документ разделами, как `Preview::Document`; `truncated` — показано начало.
    Sections { sections: Vec<Section>, truncated: bool },
    /// Картинка RGBA8 (`width × height × 4` байт); от хоста к клиенту — пиксели в бинарной части.
    Image { width: u32, height: u32, rgba: Vec<u8> },
    /// Модуль файл не показал; текст — от модуля, без управляющих символов, не длиннее [`MAX_ERROR`].
    Error(String),
}

/// Почему хост не показал файл модулем — данными: текст составляет интерфейс.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Refusal {
    /// Модуль больше [`MAX_MODULE`] или вход больше [`MAX_INPUT`].
    TooLarge,
    /// Не модуль WASM или не прошёл проверку.
    Invalid,
    /// У модуля есть импорты.
    Imports,
    /// Нет экспортов ABI v1, другая версия ABI или указатель вне памяти модуля.
    Abi,
    /// Кончилось топливо.
    Fuel,
    /// Память модуля сверх [`MAX_MEMORY`].
    Memory,
    /// Ловушка в модуле.
    Trap,
    /// Ответ длиннее [`MAX_OUTPUT`], не разобран или вне пределов.
    Output,
}

/// Ответ модуля как чужие данные: пределы разделов, блоков, символов и вложенности, ссылки — через
/// [`postman_ir::check_link`] (негодная — текстом без ссылки), картинки `cid:` — их подписью (вложений у модуля нет),
/// размер картинки.
pub fn checked(out: ViewerOutput) -> Result<ViewerOutput, Refusal> {
    match out {
        ViewerOutput::Sections { sections, truncated } => {
            if sections.len() > MAX_SECTIONS {
                return Err(Refusal::Output);
            }
            let mut budget = Budget::default();
            let sections = sections
                .into_iter()
                .map(|s| {
                    if let SectionTitle::Sheet(name) = &s.title {
                        budget.chars(name)?;
                    }
                    Ok(Section { title: s.title, blocks: budget.blocks(s.blocks, 0)? })
                })
                .collect::<Result<_, Refusal>>()?;
            Ok(ViewerOutput::Sections { sections, truncated })
        }
        ViewerOutput::Image { width, height, rgba } => {
            let sides = (1..=MAX_SIDE).contains(&width) && (1..=MAX_SIDE).contains(&height);
            if !sides || rgba.len() != width as usize * height as usize * 4 {
                return Err(Refusal::Output);
            }
            Ok(ViewerOutput::Image { width, height, rgba })
        }
        ViewerOutput::Error(e) => Ok(ViewerOutput::Error(e.chars().filter(|c| !c.is_control()).take(MAX_ERROR).collect())),
    }
}

/// Сколько ответа уже принято: блоки (с вложенными), ячейки таблиц, символы.
#[derive(Default)]
struct Budget {
    blocks: usize,
    cells: usize,
    chars: usize,
}

impl Budget {
    fn chars(&mut self, s: &str) -> Result<(), Refusal> {
        self.chars += s.chars().count();
        if self.chars > docx::MAX_CHARS { Err(Refusal::Output) } else { Ok(()) }
    }

    fn blocks(&mut self, blocks: Vec<Block>, depth: usize) -> Result<Vec<Block>, Refusal> {
        if depth > MAX_NESTING {
            return Err(Refusal::Output);
        }
        blocks.into_iter().map(|b| self.block(b, depth)).collect()
    }

    fn block(&mut self, b: Block, depth: usize) -> Result<Block, Refusal> {
        self.blocks += 1;
        if self.blocks > docx::MAX_BLOCKS {
            return Err(Refusal::Output);
        }
        Ok(match b {
            Block::Paragraph { align, content } => Block::Paragraph { align, content: self.inlines(content, false)? },
            Block::Heading { level, content } => Block::Heading { level: level.clamp(1, 6), content: self.inlines(content, false)? },
            Block::Quote(inner) => Block::Quote(self.blocks(inner, depth + 1)?),
            Block::List { ordered, items } => {
                Block::List { ordered, items: items.into_iter().map(|i| self.blocks(i, depth + 1)).collect::<Result<_, _>>()? }
            }
            Block::Code(code) => {
                self.chars(&code)?;
                Block::Code(code)
            }
            Block::Table(rows) => {
                let mut out = Vec::with_capacity(rows.len());
                for row in rows {
                    self.cells += row.len();
                    if self.cells > sheet::MAX_CELLS {
                        return Err(Refusal::Output);
                    }
                    out.push(row.into_iter().map(|c| self.inlines(c, false)).collect::<Result<_, _>>()?);
                }
                Block::Table(out)
            }
            b @ (Block::Rule | Block::Figure) => b,
        })
    }

    /// Инлайны; `in_link` — внутри ссылки: вложенная ссылка — её текстом.
    fn inlines(&mut self, inlines: Vec<Inline>, in_link: bool) -> Result<Vec<Inline>, Refusal> {
        let mut out = Vec::with_capacity(inlines.len());
        for i in inlines {
            match i {
                Inline::Text { text, marks } => {
                    self.chars(&text)?;
                    out.push(Inline::Text { text, marks });
                }
                Inline::Link { href, content, .. } => {
                    let content = self.inlines(content, true)?;
                    let check = if in_link { None } else { postman_ir::check_link(&href, &postman_ir::plain_text(&content)) };
                    match check {
                        Some(c) => out.push(Inline::Link { href: c.href, host: c.host, suspicious: c.suspicious, content }),
                        None => out.extend(content),
                    }
                }
                // Внешней картинки у модуля нет (сети нет и у хоста): остаётся подпись, адрес — мимо.
                Inline::Image { alt, .. } | Inline::Remote { alt, .. } => {
                    self.chars(&alt)?;
                    if !alt.is_empty() {
                        out.push(Inline::text(alt));
                    }
                }
                Inline::LineBreak => out.push(Inline::LineBreak),
                // Упоминаний у модуля нет (людей канала он не знает): видимым текстом.
                Inline::Mention { text, .. } => {
                    self.chars(&text)?;
                    out.push(Inline::text(text));
                }
            }
        }
        Ok(out)
    }
}

#[cfg(feature = "viewer")]
pub use host::{run, serve};

#[cfg(feature = "viewer")]
mod host {
    use wasmi::errors::{ErrorKind, InstantiationError};
    use wasmi::{Config, Engine, Linker, Module, Store, StoreLimits, StoreLimitsBuilder, TrapCode};

    use super::{ABI, Limits, MAX_MODULE, Refusal, VIEW_LIMIT, ViewerOutput, checked};
    use crate::{Request, Response};

    /// Точка входа `postman-viewer.exe`: та же песочница и тот же обмен, что у воркера; запрос — только
    /// [`Request::ViewModule`].
    pub fn serve() -> ! {
        crate::run(VIEW_LIMIT, |req, bin| match req {
            Ok(Request::ViewModule { wasm, limits }) => {
                let Some((module, input)) = bin.split_at_checked(wasm as usize) else {
                    return (Response::View(Err(Refusal::Invalid)), Vec::new());
                };
                // Сбой интерпретатора на чужом модуле — отказ этого просмотра, а не смерть процесса.
                let r = std::panic::catch_unwind(move || run(module, input, limits)).unwrap_or(Err(Refusal::Trap));
                match r {
                    Ok(ViewerOutput::Image { width, height, rgba }) => {
                        (Response::View(Ok(ViewerOutput::Image { width, height, rgba: Vec::new() })), rgba)
                    }
                    r => (Response::View(r), Vec::new()),
                }
            }
            // Проверки песочницы (предел памяти хоста — `JOB_MEMORY`) — те же, что у воркера.
            #[cfg(debug_assertions)]
            Ok(Request::Probe(p)) => (crate::probe(p), Vec::new()),
            Ok(_) => (Response::Err("только запрос модуля просмотра".into()), Vec::new()),
            Err(e) => (Response::Err(e.to_string()), Vec::new()),
        })
    }

    /// Модуль `module` смотрит вход `input` с пределами `limits` (заявленные в описании модуля; применяются ужатые
    /// до общих констант — [`Limits::clamped`], в процессе-хосте без заявленных — [`Limits::default`]); ответ
    /// проверен ([`checked`]). Глубину вложенности postcard не ограничивает: переполнение стека на разборе — падение
    /// процесса хоста, клиент поднимает новый.
    pub fn run(module: &[u8], input: &[u8], limits: Limits) -> Result<ViewerOutput, Refusal> {
        let limits = limits.clamped();
        if module.len() > MAX_MODULE || input.len() > limits.input {
            return Err(Refusal::TooLarge);
        }
        let mut config = Config::default();
        config.consume_fuel(true);
        let engine = Engine::new(&config);
        let module = Module::new(&engine, module).map_err(|_| Refusal::Invalid)?;
        if module.imports().len() != 0 {
            return Err(Refusal::Imports);
        }
        let store_limits =
            StoreLimitsBuilder::new().memory_size(limits.memory).memories(1).tables(1).table_elements(1 << 20).trap_on_grow_failure(true).build();
        let mut store = Store::new(&engine, store_limits);
        store.limiter(|l: &mut StoreLimits| l);
        store.set_fuel(limits.fuel).map_err(|_| Refusal::Fuel)?;
        let instance = Linker::new(&engine).instantiate_and_start(&mut store, &module).map_err(refusal)?;
        let abi = instance.get_typed_func::<(), u32>(&store, "pm_abi").map_err(|_| Refusal::Abi)?;
        let alloc = instance.get_typed_func::<u32, u32>(&store, "pm_alloc").map_err(|_| Refusal::Abi)?;
        let view = instance.get_typed_func::<(u32, u32), u64>(&store, "pm_view").map_err(|_| Refusal::Abi)?;
        let memory = instance.get_memory(&store, "memory").ok_or(Refusal::Abi)?;
        if abi.call(&mut store, ()).map_err(refusal)? != ABI {
            return Err(Refusal::Abi);
        }
        let len = u32::try_from(input.len()).map_err(|_| Refusal::TooLarge)?;
        let ptr = alloc.call(&mut store, len).map_err(refusal)?;
        memory.write(&mut store, ptr as usize, input).map_err(|_| Refusal::Abi)?;
        let packed = view.call(&mut store, (ptr, len)).map_err(refusal)?;
        let (at, n) = ((packed >> 32) as usize, (packed & 0xFFFF_FFFF) as usize);
        if n > limits.output {
            return Err(Refusal::Output);
        }
        let mut out = vec![0; n];
        memory.read(&store, at, &mut out).map_err(|_| Refusal::Output)?;
        // Память модуля больше не нужна: в процессе — ответ и его разбор.
        drop(store);
        checked(postcard::from_bytes(&out).map_err(|_| Refusal::Output)?)
    }

    fn refusal(e: wasmi::Error) -> Refusal {
        match (e.as_trap_code(), e.kind()) {
            (Some(TrapCode::OutOfFuel), _) => Refusal::Fuel,
            (Some(TrapCode::GrowthOperationLimited), _)
            | (_, ErrorKind::Memory(_) | ErrorKind::Instantiation(InstantiationError::FailedToInstantiateMemory(_))) => Refusal::Memory,
            _ => Refusal::Trap,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Пределы модуля ужимаются до общих констант: меньшее заявленное — своё, большее и нулевое — общее.
    #[test]
    fn declared_limits_are_clamped_to_the_host() {
        let own = Limits { input: 1, memory: 2, fuel: 3, output: 4 };
        assert_eq!(own.clamped(), own, "меньше общего — своё");
        let over = Limits { input: MAX_INPUT * 2, memory: MAX_MEMORY * 2, fuel: FUEL * 2, output: MAX_OUTPUT * 2 };
        assert_eq!(over.clamped(), Limits::default(), "больше общего — общий");
        assert_eq!(Limits { input: 0, memory: 0, fuel: 0, output: 0 }.clamped(), Limits::default(), "нулевое — общее");
        assert_eq!(Limits::default().clamped(), Limits::default());
    }

    /// Модуль из разделов `(номер, тело)`.
    fn module_of(sections: &[(u8, &[u8])]) -> Vec<u8> {
        let mut m = b"\0asm\x01\0\0\0".to_vec();
        for (id, body) in sections {
            m.push(*id);
            m.push(body.len() as u8);
            m.extend_from_slice(body);
        }
        m
    }

    /// Импорт функции `env.f` (номер типа 0): количество, два имени, вид, индекс.
    const IMPORT_FUNC: &[u8] = &[1, 3, b'e', b'n', b'v', 1, b'f', 0, 0];
    /// Импорт памяти `env.mem` с максимальной страницей: количество, два имени, вид, флаг, минимум, максимум.
    const IMPORT_MEMORY: &[u8] = &[1, 3, b'e', b'n', b'v', 3, b'm', b'e', b'm', 2, 1, 1, 2];

    /// Раздел Import разобран без интерпретатора: импорты видны, пустой раздел и не разобранный — нет.
    #[test]
    fn imports_are_seen_without_an_interpreter() {
        assert!(has_imports(&module_of(&[(2, IMPORT_FUNC)])));
        assert!(has_imports(&module_of(&[(2, IMPORT_MEMORY)])));
        assert!(has_imports(&module_of(&[(2, IMPORT_FUNC), (3, &[0])])), "и после других разделов");
        assert!(!has_imports(&module_of(&[(2, &[0])])), "ни одного импорта");
        assert!(!has_imports(&module_of(&[(3, &[0])])), "без раздела Import");
        assert!(!has_imports(&module_of(&[(2, &[1, 3])])), "импорт обрезан");
        assert!(!has_imports(&module_of(&[(2, &[0x80])])), "длина не кончается");
        assert!(!has_imports(&module_of(&[(2, &[1, 3, b'e', b'n', b'v', 1, b'f', 9, 0])])), "чужой вид импорта");
        assert!(!has_imports(b"not wasm"));
        assert!(!has_imports(b"\0asm\x02\0\0\0"), "чужая версия");
    }
}
