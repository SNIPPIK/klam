//! Буфер обмена Windows (только текст, CF_UNICODETEXT).
//!
//! Интерфейс совпадает с Linux-версией (`clipboard_unix.rs`):
//!
//!   * `poll_changed`     — вернуть новый локальный текст, если он изменился;
//!   * `set_remote_text`  — записать текст, полученный по сети.
//!
//! Изменения определяются через `GetClipboardSequenceNumber` (дёшево, без
//! открытия буфера), а дедупликация — по хэшу текста, как в Linux-версии.
//! Переводы строк нормализуются: на проводе всегда `\n`, в Windows — `\r\n`.

use std::{thread, time::Duration};

use anyhow::{Result, bail};
use windows_sys::Win32::{
    Foundation::GlobalFree,
    System::{
    DataExchange::{
        CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber,
        IsClipboardFormatAvailable, OpenClipboard, SetClipboardData,
    },
    Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock},
    }
};

use super::buffer::hash_text;

const CF_UNICODETEXT: u32 = 13;
const GMEM_MOVEABLE: u32 = 0x0002;

const OPEN_ATTEMPTS: u32 = 10;
const OPEN_RETRY_DELAY: Duration = Duration::from_millis(10);

#[derive(Debug)]
pub struct ClipboardBuffer {
    last_sequence: u32,
    last_hash: u64,
}

impl ClipboardBuffer {
    pub fn new() -> Result<Self> {
        let sequence = unsafe { GetClipboardSequenceNumber() };

        // Начальное содержимое считаем уже синхронизированным.
        let initial = read_text().ok().flatten().unwrap_or_default();

        eprintln!("[CLIPBOARD] backend=Windows (CF_UNICODETEXT)");

        Ok(Self {
            last_sequence: sequence,
            last_hash: hash_text(&initial),
        })
    }

    /// Возвращает новое локальное значение буфера обмена.
    pub fn poll_changed(&mut self) -> Result<Option<(u64, String)>> {
        let sequence = unsafe { GetClipboardSequenceNumber() };

        if sequence == self.last_sequence {
            return Ok(None);
        }

        let text = match read_text() {
            Ok(Some(text)) => text,

            // Не текст (картинка, файлы...) — запоминаем и пропускаем.
            Ok(None) => {
                self.last_sequence = sequence;
                return Ok(None);
            }

            // Буфер занят другим процессом: sequence не запоминаем,
            // повторим на следующем тике.
            Err(_) => return Ok(None),
        };

        self.last_sequence = sequence;

        let hash = hash_text(&text);

        if hash == self.last_hash {
            return Ok(None);
        }

        self.last_hash = hash;

        Ok(Some((hash, text)))
    }

    /// Устанавливает буфер обмена, полученный из сети.
    pub fn set_remote_text(&mut self, hash: u64, text: &str) -> Result<()> {
        write_text(text)?;

        self.last_hash = hash;
        self.last_sequence = unsafe { GetClipboardSequenceNumber() };

        Ok(())
    }

    #[inline]
    pub fn current_hash(&self) -> u64 {
        self.last_hash
    }
}

/// RAII: закрывает буфер обмена при выходе из области видимости.
struct OpenClipboardGuard;

impl OpenClipboardGuard {
    fn acquire() -> Result<Self> {
        for _ in 0..OPEN_ATTEMPTS {
            if unsafe { OpenClipboard(0 as _) } != 0 {
                return Ok(Self);
            }

            thread::sleep(OPEN_RETRY_DELAY);
        }

        bail!("failed to open clipboard (locked by another process)");
    }
}

impl Drop for OpenClipboardGuard {
    fn drop(&mut self) {
        unsafe {
            CloseClipboard();
        }
    }
}

fn read_text() -> Result<Option<String>> {
    unsafe {
        if IsClipboardFormatAvailable(CF_UNICODETEXT) == 0 {
            return Ok(None);
        }

        let _guard = OpenClipboardGuard::acquire()?;

        let handle = GetClipboardData(CF_UNICODETEXT);

        if handle as usize == 0 {
            return Ok(None);
        }

        let pointer = GlobalLock(handle as _) as *const u16;

        if pointer.is_null() {
            bail!("GlobalLock failed");
        }

        let capacity = GlobalSize(handle as _) / 2;
        let units = std::slice::from_raw_parts(pointer, capacity);
        let length = units.iter().position(|&unit| unit == 0).unwrap_or(capacity);

        let text = String::from_utf16_lossy(&units[..length]);

        GlobalUnlock(handle as _);

        Ok(Some(text.replace("\r\n", "\n")))
    }
}

fn write_text(text: &str) -> Result<()> {
    let windows_text = text.replace("\r\n", "\n").replace('\n', "\r\n");

    let wide: Vec<u16> = windows_text
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    let bytes = wide.len() * 2;

    unsafe {
        let memory = GlobalAlloc(GMEM_MOVEABLE, bytes);

        if memory as usize == 0 {
            bail!("GlobalAlloc failed");
        }

        let destination = GlobalLock(memory) as *mut u16;

        if destination.is_null() {
            GlobalFree(memory);
            bail!("GlobalLock failed");
        }

        std::ptr::copy_nonoverlapping(wide.as_ptr(), destination, wide.len());
        GlobalUnlock(memory);

        let _guard = match OpenClipboardGuard::acquire() {
            Ok(guard) => guard,

            Err(error) => {
                GlobalFree(memory);
                return Err(error);
            }
        };

        EmptyClipboard();

        // При успехе память переходит во владение системы.
        if SetClipboardData(CF_UNICODETEXT, memory as _) as usize == 0 {
            GlobalFree(memory);
            bail!("SetClipboardData failed");
        }
    }

    Ok(())
}
