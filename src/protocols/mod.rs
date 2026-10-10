pub mod buffer;
pub mod clipboard_sync;

#[cfg(unix)]
#[path = "clipboard_unix.rs"]
pub mod clipboard;

#[cfg(windows)]
#[path = "clipboard_windows.rs"]
pub mod clipboard;
