//! Захват ввода на Windows.
//!
//! Архитектура повторяет Linux-вариант («grab everything, replay what is
//! needed»): физический ввод перехватывается и не доходит до системы,
//! а затем воспроизводится backend'ом (`wm::win32`) или уходит по сети.
//!
//!   * WH_KEYBOARD_LL — клавиатура (scan code + флаг E0). Событие глотается.
//!   * WH_MOUSE_LL    — кнопки и колесо. Событие глотается.
//!                      WM_MOUSEMOVE тоже глотается, но дельты берутся из
//!                      Raw Input: хук отдаёт координаты, обрезанные по
//!                      границе экрана, а Klam нужен «перелёт» через край.
//!   * Raw Input      — относительные дельты мыши (окно message-only).
//!
//! Все три источника живут в одном потоке с message loop, поэтому порядок
//! событий в канале соответствует порядку, в котором их увидел поток.
//!
//! События, помеченные как injected (в т.ч. наши собственные SendInput и
//! SetCursorPos), хуки пропускают без изменений.

use std::{
    cell::{Cell, RefCell},
    ffi::c_void,
    mem::{size_of, zeroed},
    path::PathBuf,
    ptr::{null, null_mut},
    sync::{
        OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::LocalKey,
};

use anyhow::{Context, Result, anyhow, bail};
use tokio::sync::mpsc::{self, error::TrySendError};
use windows_sys::Win32::{
    Foundation::{GetLastError, HWND, LPARAM, LRESULT, WPARAM},
    System::LibraryLoader::GetModuleHandleW,
    UI::{
        Input::{
            GetRawInputData, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER,
            RegisterRawInputDevices,
        },
        WindowsAndMessaging::{
            CallNextHookEx, CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW,
            GetSystemMetrics, HWND_MESSAGE, KBDLLHOOKSTRUCT, MSG, MSLLHOOKSTRUCT, RegisterClassW,
            SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, WNDCLASSW,
        },
    },
};

use crate::{
    keymap,
    protocol::{WireEvent, ev},
};

// ---------------------------------------------------------------------------
// Константы Win32 (числовые значения вынесены сюда, чтобы не зависеть от
// типов-алиасов конкретной версии windows-sys).
// ---------------------------------------------------------------------------

const HC_ACTION: i32 = 0;
const WH_KEYBOARD_LL: i32 = 13;
const WH_MOUSE_LL: i32 = 14;

const WM_INPUT: u32 = 0x00FF;
const WM_KEYDOWN: u32 = 0x0100;
const WM_KEYUP: u32 = 0x0101;
const WM_SYSKEYDOWN: u32 = 0x0104;
const WM_SYSKEYUP: u32 = 0x0105;
const WM_MOUSEMOVE: u32 = 0x0200;
const WM_LBUTTONDOWN: u32 = 0x0201;
const WM_LBUTTONUP: u32 = 0x0202;
const WM_RBUTTONDOWN: u32 = 0x0204;
const WM_RBUTTONUP: u32 = 0x0205;
const WM_MBUTTONDOWN: u32 = 0x0207;
const WM_MBUTTONUP: u32 = 0x0208;
const WM_MOUSEWHEEL: u32 = 0x020A;
const WM_XBUTTONDOWN: u32 = 0x020B;
const WM_XBUTTONUP: u32 = 0x020C;
const WM_MOUSEHWHEEL: u32 = 0x020E;

const LLKHF_EXTENDED: u32 = 0x01;
const LLKHF_INJECTED: u32 = 0x10;
const LLMHF_INJECTED: u32 = 0x01;

const VK_PAUSE: u32 = 0x13;
const VK_SNAPSHOT: u32 = 0x2C;
const VK_NUMLOCK: u32 = 0x90;

const RID_INPUT: u32 = 0x1000_0003;
const RIM_TYPEMOUSE: u32 = 0;
const RIDEV_INPUTSINK: u32 = 0x0000_0100;
const MOUSE_MOVE_ABSOLUTE: u16 = 0x01;

const SM_CXSCREEN: i32 = 0;
const SM_CYSCREEN: i32 = 1;

const WHEEL_DELTA: i32 = 120;

/// Сколько WM_MOUSEMOVE без единого Raw Input события терпим, прежде чем
/// решить, что Raw Input для этого устройства недоступен (например,
/// Precision Touchpad), и перестать глотать движение.
const RAW_MISSING_THRESHOLD: u64 = 100;

// ---------------------------------------------------------------------------
// Глобальное состояние (хуки — это голые extern "system" функции)
// ---------------------------------------------------------------------------

/// Выставляется backend'ом на время собственных SendInput/SetCursorPos,
/// чтобы хуки гарантированно пропускали наши же события.
pub static INJECTING: AtomicBool = AtomicBool::new(false);

static ACTIVE: AtomicBool = AtomicBool::new(true);
static PASS_MOVES: AtomicBool = AtomicBool::new(false);
static HOOK_MOVES: AtomicU64 = AtomicU64::new(0);
static RAW_MOVES: AtomicU64 = AtomicU64::new(0);
static TX: OnceLock<mpsc::Sender<Vec<WireEvent>>> = OnceLock::new();

thread_local! {
    static KEY_DOWN: RefCell<[bool; 768]> = const { RefCell::new([false; 768]) };
    static WHEEL_V: Cell<i32> = const { Cell::new(0) };
    static WHEEL_H: Cell<i32> = const { Cell::new(0) };
    static LAST_ABS: Cell<Option<(i32, i32)>> = const { Cell::new(None) };
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Запускает поток захвата ввода.
///
/// `devices` на Windows не используется: перехватывается весь ввод.
pub fn spawn_readers(
    devices: Vec<PathBuf>,
    tx: mpsc::Sender<Vec<WireEvent>>,
) -> Result<()> {
    if !devices.is_empty() {
        eprintln!("[INPUT] --device is ignored on Windows");
    }

    TX.set(tx)
        .map_err(|_| anyhow!("input capture already started"))?;

    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();

    std::thread::Builder::new()
        .name("klam-input-win32".into())
        .spawn(move || input_thread(ready_tx))
        .context("failed to spawn input thread")?;

    match ready_rx.recv() {
        Ok(Ok(())) => {
            eprintln!("[INPUT] Windows hooks installed");
            Ok(())
        }
        Ok(Err(message)) => bail!("{message}"),
        Err(_) => bail!("input thread exited during startup"),
    }
}

// ---------------------------------------------------------------------------
// Input thread
// ---------------------------------------------------------------------------

fn input_thread(ready: std::sync::mpsc::Sender<Result<(), String>>) {
    let (keyboard_hook, mouse_hook) = match unsafe { setup() } {
        Ok(hooks) => {
            let _ = ready.send(Ok(()));
            hooks
        }

        Err(error) => {
            let _ = ready.send(Err(format!("{error:#}")));
            return;
        }
    };

    unsafe {
        let mut msg: MSG = zeroed();

        while GetMessageW(&mut msg, 0 as _, 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        UnhookWindowsHookEx(keyboard_hook as _);
        UnhookWindowsHookEx(mouse_hook as _);
    }

    ACTIVE.store(false, Ordering::Relaxed);
    eprintln!("[INPUT] Windows message loop stopped");
}

unsafe fn setup() -> Result<(usize, usize)> {
    unsafe {
        let hinstance = GetModuleHandleW(null());
        let class_name = wide("KlamRawInputWindow");

        let mut class: WNDCLASSW = zeroed();
        class.lpfnWndProc = Some(window_proc);
        class.hInstance = hinstance;
        class.lpszClassName = class_name.as_ptr();

        if RegisterClassW(&class) == 0 {
            bail!("RegisterClassW failed (error {})", GetLastError());
        }

        let hwnd = CreateWindowExW(
            0,
            class_name.as_ptr(),
            class_name.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            0 as _,
            hinstance,
            null(),
        );

        if hwnd as usize == 0 {
            bail!("CreateWindowExW failed (error {})", GetLastError());
        }

        let device = RAWINPUTDEVICE {
            usUsagePage: 0x01, // Generic Desktop
            usUsage: 0x02,     // Mouse
            dwFlags: RIDEV_INPUTSINK,
            hwndTarget: hwnd,
        };

        if RegisterRawInputDevices(&device, 1, size_of::<RAWINPUTDEVICE>() as u32) == 0 {
            bail!("RegisterRawInputDevices failed (error {})", GetLastError());
        }

        let keyboard = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), hinstance, 0);

        if keyboard as usize == 0 {
            bail!(
                "failed to install keyboard hook (error {})",
                GetLastError()
            );
        }

        let mouse = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), hinstance, 0);

        if mouse as usize == 0 {
            let error = GetLastError();
            UnhookWindowsHookEx(keyboard);
            bail!("failed to install mouse hook (error {})", error);
        }

        Ok((keyboard as usize, mouse as usize))
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

// ---------------------------------------------------------------------------
// Отправка в канал
// ---------------------------------------------------------------------------

/// Хук нельзя блокировать (Windows снимет хук по таймауту), поэтому
/// только `try_send`.
fn emit(mut events: Vec<WireEvent>) {
    events.push(WireEvent::new(ev::EV_SYN, 0, 0));

    let Some(tx) = TX.get() else {
        return;
    };

    match tx.try_send(events) {
        Ok(()) => {}

        Err(TrySendError::Full(_)) => {
            eprintln!("[INPUT] event queue is full, input dropped");
        }

        // Потребитель умер: перестаём глотать ввод, чтобы не заблокировать
        // пользователя в системе.
        Err(TrySendError::Closed(_)) => {
            ACTIVE.store(false, Ordering::Relaxed);
        }
    }
}

#[inline]
fn hook_enabled() -> bool {
    ACTIVE.load(Ordering::Relaxed) && !INJECTING.load(Ordering::SeqCst)
}

// ---------------------------------------------------------------------------
// Клавиатура
// ---------------------------------------------------------------------------

unsafe extern "system" fn keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if code == HC_ACTION && hook_enabled() {
            let key = &*(lparam as *const KBDLLHOOKSTRUCT);

            if key.flags & LLKHF_INJECTED == 0 {
                if let Some(event) = translate_key(key, wparam) {
                    emit(vec![event]);
                    return 1;
                }
            }
        }

        CallNextHookEx(0 as _, code, wparam, lparam)
    }
}

fn translate_key(key: &KBDLLHOOKSTRUCT, wparam: WPARAM) -> Option<WireEvent> {
    let up = matches!(wparam as u32, WM_KEYUP | WM_SYSKEYUP);
    let down = matches!(wparam as u32, WM_KEYDOWN | WM_SYSKEYDOWN);

    if !up && !down {
        return None;
    }

    let extended = key.flags & LLKHF_EXTENDED != 0;

    let evdev_code = match key.vkCode {
        VK_PAUSE => keymap::KEY_PAUSE,
        VK_NUMLOCK => keymap::KEY_NUMLOCK,
        VK_SNAPSHOT => keymap::KEY_SYSRQ,
        _ => keymap::evdev_from_scancode(key.scanCode as u16, extended)?,
    };

    let index = evdev_code as usize;

    let value = KEY_DOWN.with(|state| {
        let mut state = state.borrow_mut();

        if index >= state.len() {
            return if up { 0 } else { 1 };
        }

        if up {
            state[index] = false;
            0
        } else if state[index] {
            2 // auto-repeat
        } else {
            state[index] = true;
            1
        }
    });

    Some(WireEvent::new(ev::EV_KEY, evdev_code, value))
}

// ---------------------------------------------------------------------------
// Мышь: кнопки и колесо (хук)
// ---------------------------------------------------------------------------

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if code == HC_ACTION && hook_enabled() {
            let mouse = &*(lparam as *const MSLLHOOKSTRUCT);

            if mouse.flags & LLMHF_INJECTED == 0 {
                let high_word = ((mouse.mouseData >> 16) & 0xFFFF) as u16;

                let handled = match wparam as u32 {
                    WM_MOUSEMOVE => {
                        // Движение приходит из Raw Input. Если Raw Input
                        // для устройства не работает — не глотаем движение,
                        // чтобы не оставить пользователя без курсора.
                        if PASS_MOVES.load(Ordering::Relaxed) {
                            false
                        } else {
                            let seen = HOOK_MOVES.fetch_add(1, Ordering::Relaxed);

                            if seen >= RAW_MISSING_THRESHOLD
                                && RAW_MOVES.load(Ordering::Relaxed) == 0
                            {
                                PASS_MOVES.store(true, Ordering::Relaxed);

                                eprintln!(
                                    "[INPUT] no Raw Input mouse data received; \
                                     this pointing device (touchpad?) cannot be \
                                     forwarded, passing it through locally"
                                );

                                false
                            } else {
                                true
                            }
                        }
                    }

                    WM_LBUTTONDOWN => button(ev::BTN_LEFT, 1),
                    WM_LBUTTONUP => button(ev::BTN_LEFT, 0),
                    WM_RBUTTONDOWN => button(ev::BTN_RIGHT, 1),
                    WM_RBUTTONUP => button(ev::BTN_RIGHT, 0),
                    WM_MBUTTONDOWN => button(ev::BTN_MIDDLE, 1),
                    WM_MBUTTONUP => button(ev::BTN_MIDDLE, 0),

                    WM_XBUTTONDOWN | WM_XBUTTONUP => {
                        let value = if wparam as u32 == WM_XBUTTONDOWN { 1 } else { 0 };

                        match high_word {
                            1 => button(ev::BTN_SIDE, value),
                            2 => button(ev::BTN_EXTRA, value),
                            _ => false,
                        }
                    }

                    WM_MOUSEWHEEL => wheel(&WHEEL_V, ev::REL_WHEEL, high_word as i16 as i32),
                    WM_MOUSEHWHEEL => wheel(&WHEEL_H, ev::REL_HWHEEL, high_word as i16 as i32),

                    _ => false,
                };

                if handled {
                    return 1;
                }
            }
        }

        CallNextHookEx(0 as _, code, wparam, lparam)
    }
}

fn button(code: u16, value: i32) -> bool {
    emit(vec![WireEvent::new(ev::EV_KEY, code, value)]);
    true
}

/// Windows даёт дельту колеса в долях 120; evdev REL_WHEEL — в «щелчках».
fn wheel(remainder: &'static LocalKey<Cell<i32>>, axis: u16, delta: i32) -> bool {
    let notches = remainder.with(|cell| {
        let total = cell.get() + delta;
        let notches = total / WHEEL_DELTA;
        cell.set(total - notches * WHEEL_DELTA);
        notches
    });

    if notches != 0 {
        emit(vec![WireEvent::new(ev::EV_REL, axis, notches)]);
    }

    true
}

// ---------------------------------------------------------------------------
// Мышь: относительное движение (Raw Input)
// ---------------------------------------------------------------------------

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        if msg == WM_INPUT {
            handle_raw_input(lparam);
        }

        // Для WM_INPUT DefWindowProc нужен для очистки на стороне системы.
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }
}

unsafe fn handle_raw_input(lparam: LPARAM) {
    unsafe {
        let handle = lparam as HRAWINPUT;
        let header_size = size_of::<RAWINPUTHEADER>() as u32;

        let mut size = 0u32;

        if GetRawInputData(handle, RID_INPUT, null_mut(), &mut size, header_size) != 0 {
            return;
        }

        // 1 KiB с выравниванием по 8 байт; RAWINPUT для мыши заметно меньше.
        let mut buffer = [0u64; 128];

        if size == 0 || size as usize > size_of::<[u64; 128]>() {
            return;
        }

        let read = GetRawInputData(
            handle,
            RID_INPUT,
            buffer.as_mut_ptr() as *mut c_void,
            &mut size,
            header_size,
        );

        if read == u32::MAX || read == 0 {
            return;
        }

        let raw = &*(buffer.as_ptr() as *const RAWINPUT);

        if raw.header.dwType != RIM_TYPEMOUSE {
            return;
        }

        // hDevice == NULL у injected-событий (в т.ч. от других программ).
        if raw.header.hDevice as usize == 0 {
            return;
        }

        let mouse = &raw.data.mouse;

        let (dx, dy) = if mouse.usFlags & MOUSE_MOVE_ABSOLUTE != 0 {
            // Абсолютные устройства (VM/RDP/планшет): приводим к дельтам.
            let width = GetSystemMetrics(SM_CXSCREEN) as i64;
            let height = GetSystemMetrics(SM_CYSCREEN) as i64;

            let x = (mouse.lLastX as i64 * width / 65535) as i32;
            let y = (mouse.lLastY as i64 * height / 65535) as i32;

            match LAST_ABS.with(|cell| cell.replace(Some((x, y)))) {
                Some((last_x, last_y)) => (x - last_x, y - last_y),
                None => (0, 0),
            }
        } else {
            (mouse.lLastX, mouse.lLastY)
        };

        if dx == 0 && dy == 0 {
            return;
        }

        RAW_MOVES.fetch_add(1, Ordering::Relaxed);

        let mut events = Vec::with_capacity(3);

        if dx != 0 {
            events.push(WireEvent::new(ev::EV_REL, ev::REL_X, dx));
        }

        if dy != 0 {
            events.push(WireEvent::new(ev::EV_REL, ev::REL_Y, dy));
        }

        emit(events);
    }
}
