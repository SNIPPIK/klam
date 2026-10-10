use anyhow::Result;

use crate::{
    cursor::CursorPosition,
    protocol::{
        Edge,
        WireEvent,
    },
};

#[cfg(target_os = "linux")]
mod uinput;

#[cfg(windows)]
mod win32;

pub trait InputBackend: Send {
    fn send_events(
        &mut self,
        events: &[WireEvent],
    ) -> Result<()>;

    fn sync_to_edge(
        &mut self,
        edge: Edge,
        coordinate: i32,
        current: CursorPosition,
    ) -> Result<()>;
}

#[cfg(target_os = "linux")]
pub fn create(
    name: &str,
) -> Result<Box<dyn InputBackend>> {
    Ok(Box::new(
        uinput::UinputBackend::new(
            name,
        )?,
    ))
}

#[cfg(windows)]
pub fn create(
    name: &str,
) -> Result<Box<dyn InputBackend>> {
    Ok(Box::new(
        win32::WindowsBackend::new(
            name,
        )?,
    ))
}

/// Одноразовая инициализация процесса (на Windows — DPI awareness,
/// чтобы координаты были в физических пикселях).
#[cfg(windows)]
pub use win32::init_process;

#[cfg(not(windows))]
pub fn init_process() {}

/// Размер экрана, определённый системой.
///
/// На Windows — размер виртуального рабочего стола (все мониторы).
/// На Linux автоопределение не выполняется.
#[cfg(windows)]
pub use win32::detect_screen_size;

#[cfg(not(windows))]
pub fn detect_screen_size() -> Option<(i32, i32)> {
    None
}
