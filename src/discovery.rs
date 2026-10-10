//! Поиск сервера в локальной сети.
//!
//! Сервер раз в секунду рассылает UDP-broadcast `KLAM1|<имя>|<tcp-порт>`
//! на порт 24802. GUI клиента слушает этот порт и показывает найденные
//! серверы.

use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr, UdpSocket},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

pub const DISCOVERY_PORT: u16 = 24802;

pub struct FoundServer {
    pub name: String,
    pub last_seen: Instant,
}

pub type FoundServers = Arc<Mutex<HashMap<(IpAddr, u16), FoundServer>>>;

pub fn hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|name| name.trim().to_string())
        })
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "klam-server".to_string())
}

/// Запускается в `server::run`: периодически рассылает beacon.
pub fn spawn_beacon(tcp_port: u16) {
    tokio::spawn(async move {
        let socket = match tokio::net::UdpSocket::bind("0.0.0.0:0").await {
            Ok(socket) => socket,

            Err(error) => {
                eprintln!("[DISCOVERY] beacon disabled: {}", error);
                return;
            }
        };

        if let Err(error) = socket.set_broadcast(true) {
            eprintln!("[DISCOVERY] beacon disabled: {}", error);
            return;
        }

        let payload = format!("KLAM1|{}|{}", hostname().replace('|', "_"), tcp_port);
        let target = SocketAddr::from(([255, 255, 255, 255], DISCOVERY_PORT));

        let mut tick = tokio::time::interval(Duration::from_secs(1));

        loop {
            tick.tick().await;
            let _ = socket.send_to(payload.as_bytes(), target).await;
        }
    });
}

/// Запускается в GUI клиента: слушает beacon'ы в фоновом потоке.
pub fn spawn_listener() -> FoundServers {
    let found: FoundServers = Arc::new(Mutex::new(HashMap::new()));
    let shared = found.clone();

    std::thread::spawn(move || {
        let socket = match UdpSocket::bind(("0.0.0.0", DISCOVERY_PORT)) {
            Ok(socket) => socket,

            Err(error) => {
                eprintln!("[DISCOVERY] listener disabled: {}", error);
                return;
            }
        };

        let mut buffer = [0u8; 256];

        loop {
            let (size, from) = match socket.recv_from(&mut buffer) {
                Ok(received) => received,

                Err(_) => {
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
            };

            let Ok(text) = std::str::from_utf8(&buffer[..size]) else {
                continue;
            };

            let Some((name, port)) = parse(text) else {
                continue;
            };

            shared.lock().unwrap().insert(
                (from.ip(), port),
                FoundServer {
                    name,
                    last_seen: Instant::now(),
                },
            );
        }
    });

    found
}

fn parse(text: &str) -> Option<(String, u16)> {
    let mut parts = text.split('|');

    if parts.next()? != "KLAM1" {
        return None;
    }

    let name = parts.next()?.to_string();
    let port = parts.next()?.trim().parse().ok()?;

    Some((name, port))
}
