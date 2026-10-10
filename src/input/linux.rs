use anyhow::{Context, Result};
use evdev::{
    AttributeSet,
    Device,
    EventType,
    InputEvent,
    KeyCode,
    RelativeAxisCode,
};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};
use tokio::sync::mpsc;

use crate::protocol::WireEvent;

const EV_SYN: u16 = EventType::SYNCHRONIZATION.0;
const EV_KEY: u16 = EventType::KEY.0;
const EV_REL: u16 = EventType::RELATIVE.0;

const MAX_INPUT_EVENTS: usize = 128;

/// Создаёт виртуальное устройство для вывода событий.
/// Используется `uinput`-backend'ом.
pub fn create_virtual_device(
    name: &str,
) -> Result<evdev::uinput::VirtualDevice> {
    let mut keys = AttributeSet::<KeyCode>::new();

    // Полный диапазон Linux key/button codes.
    for code in 0..=767 {
        keys.insert(KeyCode::new(code));
    }

    let mut relative_axes =
        AttributeSet::<RelativeAxisCode>::new();

    relative_axes.insert(
        RelativeAxisCode::REL_X,
    );

    relative_axes.insert(
        RelativeAxisCode::REL_Y,
    );

    relative_axes.insert(
        RelativeAxisCode::REL_WHEEL,
    );

    relative_axes.insert(
        RelativeAxisCode::REL_HWHEEL,
    );

    let device =
        evdev::uinput::VirtualDevice::builder()?
            .name(name)
            .with_keys(&keys)?
            .with_relative_axes(
                &relative_axes
            )?
            .build()?;

    Ok(device)
}

/// Запускает readers для переданных устройств.
///
/// Если `devices` пустой, автоматически ищет `/dev/input/event*`.
///
/// Каждый reader отправляет в канал батчи `Vec<WireEvent>`.
pub fn spawn_readers(
    devices: Vec<PathBuf>,
    tx: mpsc::Sender<Vec<WireEvent>>,
) -> Result<()> {
    let devices =
        if devices.is_empty() {
            discover_devices()?
        } else {
            devices
        };

    if devices.is_empty() {
        anyhow::bail!(
            "no usable input devices found"
        );
    }

    for path in devices {
        let tx = tx.clone();

        std::thread::Builder::new()
            .name(
                format!(
                    "klam-input-{}",
                    path.file_name()
                        .and_then(|v| v.to_str())
                        .unwrap_or("unknown")
                )
            )
            .spawn(move || {
                if let Err(error) =
                    read_device(path.clone(), tx)
                {
                    eprintln!(
                        "[INPUT] reader {:?} stopped: {:#}",
                        path,
                        error
                    );
                }
            })
            .context(
                "failed to spawn input reader thread"
            )?;
    }

    Ok(())
}

/// Автоматически ищет пригодные `/dev/input/event*`.
fn discover_devices() -> Result<Vec<PathBuf>> {
    let mut devices = Vec::new();

    let entries =
        fs::read_dir("/dev/input")
            .context(
                "failed to read /dev/input"
            )?;

    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                eprintln!(
                    "[INPUT] failed to inspect /dev/input entry: {}",
                    error
                );
                continue;
            }
        };

        let path = entry.path();

        let Some(name) =
            path.file_name()
                .and_then(|v| v.to_str())
        else {
            continue;
        };

        if !name.starts_with("event") {
            continue;
        }

        let device =
            match Device::open(&path) {
                Ok(device) => device,

                Err(error) => {
                    eprintln!(
                        "[INPUT] cannot open {:?}: {}",
                        path,
                        error
                    );
                    continue;
                }
            };

        let useful =
            is_useful_device(&device);

        let relative =
            device.supported_relative_axes();

        let has_x = relative.map_or(
            false,
            |axes| {
                axes.contains(
                    RelativeAxisCode::REL_X
                )
            },
        );

        let has_y = relative.map_or(
            false,
            |axes| {
                axes.contains(
                    RelativeAxisCode::REL_Y
                )
            },
        );

        let has_keys =
            device.supported_keys().is_some();

        eprintln!(
            "[INPUT DISCOVERY] {:?} | {} | mouse={} (x={} y={}) keyboard={} useful={}",
            path,
            device.name().unwrap_or("<unknown>"),
            has_x || has_y,
            has_x,
            has_y,
            has_keys,
            useful,
        );

        if useful {
            devices.push(path);
        }
    }

    Ok(devices)
}

/// Определяет, представляет ли устройство интерес для Klam.
fn is_useful_device(
    device: &Device,
) -> bool {
    let name =
        device.name().unwrap_or("<unknown>");

    // Никогда не читаем собственные uinput-устройства Klam.
    if name.starts_with("Klam ") {
        return false;
    }

    // ------------------------------------------------------------------------
    // MOUSE / POINTER
    // ------------------------------------------------------------------------

    let has_relative_xy =
        device.supported_relative_axes().map_or(
            false,
            |axes| {
                axes.contains(
                    RelativeAxisCode::REL_X
                ) || axes.contains(
                    RelativeAxisCode::REL_Y
                )
            },
        );

    if has_relative_xy {
        return true;
    }

    // ------------------------------------------------------------------------
    // KEYBOARD
    // ------------------------------------------------------------------------
    //
    // Не принимаем любой KEY-девайс подряд:
    // иначе в автопоиск могут попасть HDMI buttons,
    // power buttons и другие служебные устройства.
    //

    let looks_like_keyboard =
        device.supported_keys().map_or(
            false,
            |keys| {
                keys.contains(KeyCode::KEY_A)
                    && keys.contains(
                    KeyCode::KEY_SPACE
                )
            },
        );

    looks_like_keyboard
}

/// Читает один `/dev/input/eventX`.
///
/// Все события одного чтения `fetch_events()` собираются
/// в один батч и отправляются в Tokio channel.
fn read_device(
    path: PathBuf,
    tx: mpsc::Sender<Vec<WireEvent>>,
) -> Result<()> {
    let mut device =
        Device::open(&path)
            .with_context(|| {
                format!(
                    "failed to open {:?}",
                    path
                )
            })?;

    // Копируем имя, чтобы оно больше не занимало
    // immutable borrow от device.
    let name = device
        .name()
        .unwrap_or("<unknown>")
        .to_owned();

    eprintln!(
        "[INPUT] opening {:?} | {}",
        path,
        name
    );

    // Теперь device можно мутировать.
    device
        .grab()
        .with_context(|| {
            format!(
                "failed to grab {:?} ({})",
                path,
                name
            )
        })?;

    eprintln!(
        "[INPUT] grabbed {:?} | {}",
        path,
        name
    );

    /*
     * Нормализация под DPI мыши.
     *
     * Родной libinput масштабирует движение по MOUSE_DPI из udev/hwdb.
     * У виртуального uinput-устройства такой записи нет, поэтому без
     * этого скорость отличалась бы от родной.
     */
    let is_mouse =
        device
            .supported_relative_axes()
            .map_or(false, |axes| {
                axes.contains(
                    RelativeAxisCode::REL_X
                )
            });

    let mut scaler =
        if is_mouse {
            DpiScaler::detect(&path, &name)
        } else {
            None
        };

    loop {
        let events =
            device
                .fetch_events()
                .with_context(|| {
                    format!(
                        "failed to fetch events from {:?}",
                        path
                    )
                })?;

        let mut batch =
            Vec::<WireEvent>::with_capacity(
                MAX_INPUT_EVENTS
            );

        for event in events {
            let mut event =
                convert_event(event);

            if let Some(scaler) =
                scaler.as_mut()
            {
                event =
                    scaler.apply(event);
            }

            if batch.len()
                >= MAX_INPUT_EVENTS
            {
                flush_batch(
                    &tx,
                    &mut batch,
                )?;
            }

            batch.push(event);
        }

        if !batch.is_empty() {
            flush_batch(
                &tx,
                &mut batch,
            )?;
        }
    }
}

/// Преобразует `evdev::InputEvent` в наш wire-формат.
#[inline]
fn convert_event(
    event: InputEvent,
) -> WireEvent {
    WireEvent::new(
        event.event_type().0,
        event.code(),
        event.value(),
    )
}

/// Отправляет батч через Tokio channel.
///
/// Поток блокируется только если consumer сильно отстаёт.
fn flush_batch(
    tx: &mpsc::Sender<Vec<WireEvent>>,
    batch: &mut Vec<WireEvent>,
) -> Result<()> {
    if batch.is_empty() {
        return Ok(());
    }

    let events =
        std::mem::take(batch);

    /*eprintln!(
        "[INPUT] sending batch: {} events",
        events.len()
    );*/

    tx.blocking_send(events)
        .map_err(|_| {
            anyhow::anyhow!(
                "input receiver dropped"
            )
        })
}
/// Приводит REL_X/REL_Y к 1000 DPI, как это делает libinput.
struct DpiScaler {
    scale: f64,
    remainder_x: f64,
    remainder_y: f64,
}

impl DpiScaler {
    fn detect(
        path: &Path,
        name: &str,
    ) -> Option<Self> {
        let output =
            Command::new("udevadm")
                .args([
                    "info",
                    "--query=property",
                    "--name",
                ])
                .arg(path)
                .output()
                .ok()?;

        let text =
            String::from_utf8_lossy(
                &output.stdout,
            );

        let value =
            text.lines()
                .find_map(|line| {
                    line.strip_prefix(
                        "MOUSE_DPI="
                    )
                })?;

        let dpi =
            parse_default_dpi(value)?;

        let scale =
            1000.0 / dpi;

        eprintln!(
            "[INPUT] {} | MOUSE_DPI={} -> default {} dpi, scale {:.3}",
            name,
            value,
            dpi,
            scale
        );

        if (scale - 1.0).abs() < 0.01 {
            return None;
        }

        Some(Self {
            scale,
            remainder_x: 0.0,
            remainder_y: 0.0,
        })
    }

    fn apply(
        &mut self,
        mut event: WireEvent,
    ) -> WireEvent {
        if event.event_type == EV_REL {
            match event.code {
                0 => {
                    event.value =
                        Self::step(
                            &mut self.remainder_x,
                            event.value,
                            self.scale,
                        );
                }

                1 => {
                    event.value =
                        Self::step(
                            &mut self.remainder_y,
                            event.value,
                            self.scale,
                        );
                }

                _ => {}
            }
        }

        event
    }

    /// Дробный остаток переносится на следующее событие,
    /// чтобы медленное движение не терялось.
    fn step(
        remainder: &mut f64,
        value: i32,
        scale: f64,
    ) -> i32 {
        let total =
            *remainder
                + value as f64 * scale;

        let whole =
            total.trunc();

        *remainder =
            total - whole;

        whole as i32
    }
}

/// `MOUSE_DPI`: список вида `400 800 *1600@1000`, значение по умолчанию
/// помечено `*`; если пометки нет — берётся первое.
fn parse_default_dpi(value: &str) -> Option<f64> {
    let entries: Vec<&str> =
        value.split_whitespace().collect();

    let chosen =
        entries
            .iter()
            .find(|entry| {
                entry.starts_with('*')
            })
            .or(entries.first())?;

    let dpi: f64 =
        chosen
            .trim_start_matches('*')
            .split('@')
            .next()?
            .parse()
            .ok()?;

    if dpi > 0.0 {
        Some(dpi)
    } else {
        None
    }
}