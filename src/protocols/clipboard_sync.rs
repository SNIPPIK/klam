//! Фоновый воркер синхронизации буфера обмена.
//!
//! `ClipboardBuffer` делает блокирующие системные вызовы (на Linux —
//! запуск `runuser`/`xclip`/`wl-paste`, на Windows — OpenClipboard), поэтому
//! он живёт в отдельном потоке и не тормозит основной цикл с вводом.
//!
//!   local  -> network:  `ClipboardSync::recv_local()` / `next_clipboard()`
//!   network -> local:   `ClipboardSync::push_remote()`

use std::{
    sync::mpsc as std_mpsc,
    thread,
    time::Duration,
};

use tokio::sync::mpsc;

use super::clipboard::ClipboardBuffer;

#[derive(Debug)]
pub struct ClipboardUpdate {
    pub hash: u64,
    pub text: String,
}

pub struct ClipboardSync {
    local_rx: mpsc::Receiver<ClipboardUpdate>,
    remote_tx: std_mpsc::Sender<ClipboardUpdate>,
}

impl ClipboardSync {
    /// Запускает воркер. `None`, если буфер обмена недоступен
    /// (например, не установлен xclip/wl-clipboard) — синхронизация при этом
    /// просто отключается, всё остальное работает.
    pub fn spawn(poll_interval: Duration) -> Option<Self> {
        let (local_tx, local_rx) = mpsc::channel::<ClipboardUpdate>(16);
        let (remote_tx, remote_rx) = std_mpsc::channel::<ClipboardUpdate>();
        let (ready_tx, ready_rx) = std_mpsc::channel::<bool>();

        thread::Builder::new()
            .name("klam-clipboard".into())
            .spawn(move || {
                let mut buffer = match ClipboardBuffer::new() {
                    Ok(buffer) => {
                        let _ = ready_tx.send(true);
                        buffer
                    }

                    Err(error) => {
                        eprintln!("[CLIPBOARD] disabled: {:#}", error);
                        let _ = ready_tx.send(false);
                        return;
                    }
                };

                loop {
                    match remote_rx.recv_timeout(poll_interval) {
                        Ok(update) => {
                            if update.hash != buffer.current_hash() {
                                if let Err(error) =
                                    buffer.set_remote_text(update.hash, &update.text)
                                {
                                    eprintln!("[CLIPBOARD] write error: {:#}", error);
                                }
                            }

                            // Сразу после записи не опрашиваем буфер:
                            // wl-copy/xclip могут обновиться с задержкой.
                            continue;
                        }

                        Err(std_mpsc::RecvTimeoutError::Timeout) => {}

                        Err(std_mpsc::RecvTimeoutError::Disconnected) => break,
                    }

                    match buffer.poll_changed() {
                        Ok(Some((hash, text))) => {
                            if local_tx
                                .blocking_send(ClipboardUpdate { hash, text })
                                .is_err()
                            {
                                break;
                            }
                        }

                        Ok(None) => {}

                        Err(error) => {
                            eprintln!("[CLIPBOARD] poll error: {:#}", error);
                        }
                    }
                }
            })
            .ok()?;

        match ready_rx.recv() {
            Ok(true) => Some(Self {
                local_rx,
                remote_tx,
            }),

            _ => None,
        }
    }

    /// Новое локальное значение буфера обмена.
    pub async fn recv_local(&mut self) -> Option<ClipboardUpdate> {
        self.local_rx.recv().await
    }

    /// Значение, полученное по сети, — записать в локальный буфер.
    pub fn push_remote(&self, update: ClipboardUpdate) {
        let _ = self.remote_tx.send(update);
    }
}

/// Удобный future для `tokio::select!`: если синхронизация отключена,
/// никогда не завершается.
pub async fn next_clipboard(
    sync: &mut Option<ClipboardSync>,
) -> Option<ClipboardUpdate> {
    match sync.as_mut() {
        Some(sync) => sync.recv_local().await,
        None => std::future::pending().await,
    }
}
