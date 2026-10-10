//! Windows backend воспроизведения ввода.
//!
//! * Клавиши — `SendInput` со scan code (физические клавиши, как на Linux).
//! * Кнопки мыши и колесо — `SendInput`.
//! * Движение — `SendInput(MOUSEEVENTF_MOVE)`, как от физической мыши:
//!   действуют родная скорость указателя и акселерация Windows.
//!   Чтобы логический `CursorTracker` не расходился с настоящим курсором,
//!   `cursor_position()` отдаёт реальную позицию, и вызывающий код
//!   периодически к ней подтягивается.
//! * `sync_to_edge` — прямой `SetCursorPos` на нужный край.

use std::{
    mem::size_of,
    sync::atomic::{AtomicBool, Ordering},
};

use anyhow::Result;
use windows_sys::Win32::{
    Foundation::POINT,
    UI::{
        Input::KeyboardAndMouse::{INPUT, INPUT_0, KEYBDINPUT, MOUSEINPUT, SendInput},
        WindowsAndMessaging::{GetCursorPos, GetSystemMetrics, SetCursorPos, SetProcessDPIAware},
    },
};

use crate::{
    cursor::CursorPosition,
    input::win32::INJECTING,
    keymap::{self, KeySpec},
    protocol::{Edge, WireEvent, ev},
};

const INPUT_MOUSE: u32 = 0;
const INPUT_KEYBOARD: u32 = 1;

const KEYEVENTF_EXTENDEDKEY: u32 = 0x0001;
const KEYEVENTF_KEYUP: u32 = 0x0002;
const KEYEVENTF_SCANCODE: u32 = 0x0008;

const MOUSEEVENTF_MOVE: u32 = 0x0001;
const MOUSEEVENTF_LEFTDOWN: u32 = 0x0002;
const MOUSEEVENTF_LEFTUP: u32 = 0x0004;
const MOUSEEVENTF_RIGHTDOWN: u32 = 0x0008;
const MOUSEEVENTF_RIGHTUP: u32 = 0x0010;
const MOUSEEVENTF_MIDDLEDOWN: u32 = 0x0020;
const MOUSEEVENTF_MIDDLEUP: u32 = 0x0040;
const MOUSEEVENTF_XDOWN: u32 = 0x0080;
const MOUSEEVENTF_XUP: u32 = 0x0100;
const MOUSEEVENTF_WHEEL: u32 = 0x0800;
const MOUSEEVENTF_HWHEEL: u32 = 0x1000;

const XBUTTON1: i32 = 1;
const XBUTTON2: i32 = 2;

const WHEEL_DELTA: i32 = 120;

const SM_CXSCREEN: i32 = 0;
const SM_CYSCREEN: i32 = 1;
const SM_XVIRTUALSCREEN: i32 = 76;
const SM_YVIRTUALSCREEN: i32 = 77;
const SM_CXVIRTUALSCREEN: i32 = 78;
const SM_CYVIRTUALSCREEN: i32 = 79;

static SEND_INPUT_WARNED: AtomicBool = AtomicBool::new(false);

pub fn init_process() {
    unsafe {
        SetProcessDPIAware();
    }
}

pub fn detect_screen_size() -> Option<(i32, i32)> {
    let (_, _, width, height) = virtual_screen();

    Some((width, height))
}

/// (left, top, width, height) виртуального рабочего стола.
fn virtual_screen() -> (i32, i32, i32, i32) {
    unsafe {
        let left = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let top = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let width = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let height = GetSystemMetrics(SM_CYVIRTUALSCREEN);

        if width <= 0 || height <= 0 {
            (
                0,
                0,
                GetSystemMetrics(SM_CXSCREEN).max(1),
                GetSystemMetrics(SM_CYSCREEN).max(1),
            )
        } else {
            (left, top, width, height)
        }
    }
}

/// Пока жив, хуки захвата пропускают события как «свои».
struct InjectGuard;

impl InjectGuard {
    fn new() -> Self {
        INJECTING.store(true, Ordering::SeqCst);
        Self
    }
}

impl Drop for InjectGuard {
    fn drop(&mut self) {
        INJECTING.store(false, Ordering::SeqCst);
    }
}

pub struct WindowsBackend;

impl WindowsBackend {
    pub fn new(_name: &str) -> Result<Self> {
        Ok(Self)
    }
}

impl super::InputBackend for WindowsBackend {
    fn send_events(&mut self, events: &[WireEvent]) -> Result<()> {
        let mut inputs = Vec::<INPUT>::with_capacity(events.len());

        let mut dx = 0i32;
        let mut dy = 0i32;

        for event in events.iter().copied() {
            if event.event_type == ev::EV_SYN {
                continue;
            }

            if event.event_type == ev::EV_REL {
                match event.code {
                    ev::REL_X => {
                        dx = dx.saturating_add(event.value);
                        continue;
                    }

                    ev::REL_Y => {
                        dy = dy.saturating_add(event.value);
                        continue;
                    }

                    _ => {}
                }
            }

            if let Some(input) = translate(event) {
                // Сохраняем порядок: накопленное движение — до этого события.
                apply_motion(&mut inputs, &mut dx, &mut dy);
                inputs.push(input);
            }
        }

        apply_motion(&mut inputs, &mut dx, &mut dy);
        send(&inputs);

        Ok(())
    }

    fn cursor_position(&mut self) -> Option<CursorPosition> {
        let (left, top, width, height) = virtual_screen();

        let mut point = POINT { x: 0, y: 0 };

        if unsafe { GetCursorPos(&mut point) } == 0 {
            return None;
        }

        Some(CursorPosition {
            x: (point.x - left).clamp(0, width - 1),
            y: (point.y - top).clamp(0, height - 1),
        })
    }

    fn sync_to_edge(
        &mut self,
        edge: Edge,
        coordinate: i32,
        _current: CursorPosition,
    ) -> Result<()> {
        let (left, top, width, height) = virtual_screen();

        let along_y = coordinate.clamp(0, height - 1);
        let along_x = coordinate.clamp(0, width - 1);

        let (x, y) = match edge {
            Edge::Left => (left, top + along_y),
            Edge::Right => (left + width - 1, top + along_y),
            Edge::Top => (left + along_x, top),
            Edge::Bottom => (left + along_x, top + height - 1),
        };

        let _guard = InjectGuard::new();

        unsafe {
            SetCursorPos(x, y);
        }

        Ok(())
    }
}

fn apply_motion(inputs: &mut Vec<INPUT>, dx: &mut i32, dy: &mut i32) {
    if *dx == 0 && *dy == 0 {
        return;
    }

    inputs.push(mouse_move_input(*dx, *dy));

    *dx = 0;
    *dy = 0;
}

fn send(inputs: &[INPUT]) {
    if inputs.is_empty() {
        return;
    }

    let _guard = InjectGuard::new();

    let sent = unsafe {
        SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            size_of::<INPUT>() as i32,
        )
    };

    if (sent as usize) < inputs.len() && !SEND_INPUT_WARNED.swap(true, Ordering::Relaxed) {
        eprintln!(
            "[WIN] SendInput was rejected ({}/{}). Input into elevated windows \
             requires running klam as Administrator.",
            sent,
            inputs.len()
        );
    }
}

fn translate(event: WireEvent) -> Option<INPUT> {
    match event.event_type {
        ev::EV_KEY => translate_key(event.code, event.value),

        ev::EV_REL => match event.code {
            ev::REL_WHEEL => Some(mouse_input(
                MOUSEEVENTF_WHEEL,
                event.value.saturating_mul(WHEEL_DELTA),
            )),

            ev::REL_HWHEEL => Some(mouse_input(
                MOUSEEVENTF_HWHEEL,
                event.value.saturating_mul(WHEEL_DELTA),
            )),

            // REL_WHEEL_HI_RES и т.п. игнорируем, чтобы не прокручивать дважды.
            _ => None,
        },

        _ => None,
    }
}

fn translate_key(code: u16, value: i32) -> Option<INPUT> {
    let up = value == 0;

    let button = match code {
        ev::BTN_LEFT => Some((MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, 0)),
        ev::BTN_RIGHT => Some((MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, 0)),
        ev::BTN_MIDDLE => Some((MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, 0)),
        ev::BTN_SIDE => Some((MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, XBUTTON1)),
        ev::BTN_EXTRA => Some((MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, XBUTTON2)),
        _ => None,
    };

    if let Some((down_flag, up_flag, data)) = button {
        // У кнопок нет автоповтора.
        if value == 2 {
            return None;
        }

        return Some(mouse_input(if up { up_flag } else { down_flag }, data));
    }

    // value == 2 (autorepeat) — просто ещё один keydown.
    let mut flags = if up { KEYEVENTF_KEYUP } else { 0 };

    match keymap::key_spec(code)? {
        KeySpec::Scan { scan, extended } => {
            flags |= KEYEVENTF_SCANCODE;

            if extended {
                flags |= KEYEVENTF_EXTENDEDKEY;
            }

            Some(key_input(0, scan, flags))
        }

        KeySpec::Vk { vk, extended } => {
            if extended {
                flags |= KEYEVENTF_EXTENDEDKEY;
            }

            Some(key_input(vk, 0, flags))
        }
    }
}

fn key_input(vk: u16, scan: u16, flags: u32) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn mouse_move_input(dx: i32, dy: i32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: 0,
                dwFlags: MOUSEEVENTF_MOVE,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn mouse_input(flags: u32, data: i32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: data as u32,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}
