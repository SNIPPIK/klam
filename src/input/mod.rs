#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
pub use linux::{
    create_virtual_device,
    spawn_readers,
};

#[cfg(windows)]
pub mod win32;

#[cfg(windows)]
pub use win32::spawn_readers;
