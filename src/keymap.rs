//! Трансляция клавиш: Linux evdev key code <-> Windows scan code.
//!
//! Wire-формат Klam использует коды Linux evdev. Для основной части
//! клавиатуры (0x01..=0x58) evdev-код совпадает с PS/2 scan code set 1,
//! поэтому трансляция идёт по физическим клавишам и не зависит от
//! раскладки. Расширенные (E0) клавиши берутся из таблицы.

#[derive(Debug, Clone, Copy)]
pub enum KeySpec {
    /// Инжектировать через scan code (KEYEVENTF_SCANCODE).
    Scan { scan: u16, extended: bool },

    /// Клавиши, которые нельзя надёжно выразить scan code'ом
    /// (Pause, NumLock) — инжектируем через virtual-key.
    Vk { vk: u16, extended: bool },
}

pub const KEY_NUMLOCK: u16 = 69;
pub const KEY_SYSRQ: u16 = 99;
pub const KEY_PAUSE: u16 = 119;

/// (Windows scan code с префиксом E0, evdev code)
const EXTENDED: &[(u16, u16)] = &[
    (0x1C, 96),  // KEY_KPENTER
    (0x1D, 97),  // KEY_RIGHTCTRL
    (0x35, 98),  // KEY_KPSLASH
    (0x37, 99),  // KEY_SYSRQ (PrintScreen)
    (0x38, 100), // KEY_RIGHTALT
    (0x47, 102), // KEY_HOME
    (0x48, 103), // KEY_UP
    (0x49, 104), // KEY_PAGEUP
    (0x4B, 105), // KEY_LEFT
    (0x4D, 106), // KEY_RIGHT
    (0x4F, 107), // KEY_END
    (0x50, 108), // KEY_DOWN
    (0x51, 109), // KEY_PAGEDOWN
    (0x52, 110), // KEY_INSERT
    (0x53, 111), // KEY_DELETE
    (0x5B, 125), // KEY_LEFTMETA
    (0x5C, 126), // KEY_RIGHTMETA
    (0x5D, 127), // KEY_COMPOSE (Menu)
    (0x20, 113), // KEY_MUTE
    (0x2E, 114), // KEY_VOLUMEDOWN
    (0x30, 115), // KEY_VOLUMEUP
    (0x22, 164), // KEY_PLAYPAUSE
    (0x24, 166), // KEY_STOPCD
    (0x10, 165), // KEY_PREVIOUSSONG
    (0x19, 163), // KEY_NEXTSONG
];

#[inline]
fn is_base_scan(scan: u16) -> bool {
    matches!(scan, 0x01..=0x53 | 0x56..=0x58)
}

/// Windows scan code -> evdev key code.
///
/// `None`, если клавиша неизвестна (например, «fake shift» E0 2A,
/// который Windows генерирует вокруг некоторых клавиш).
pub fn evdev_from_scancode(scan: u16, extended: bool) -> Option<u16> {
    if extended {
        EXTENDED
            .iter()
            .find(|(s, _)| *s == scan)
            .map(|(_, code)| *code)
    } else if is_base_scan(scan) {
        Some(scan)
    } else {
        None
    }
}

/// evdev key code -> способ инжекта в Windows.
pub fn key_spec(code: u16) -> Option<KeySpec> {
    match code {
        KEY_NUMLOCK => Some(KeySpec::Vk {
            vk: 0x90,
            extended: true,
        }),

        KEY_PAUSE => Some(KeySpec::Vk {
            vk: 0x13,
            extended: false,
        }),

        _ if is_base_scan(code) => Some(KeySpec::Scan {
            scan: code,
            extended: false,
        }),

        _ => EXTENDED
            .iter()
            .find(|(_, c)| *c == code)
            .map(|(scan, _)| KeySpec::Scan {
                scan: *scan,
                extended: true,
            }),
    }
}
