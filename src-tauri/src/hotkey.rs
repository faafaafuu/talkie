//! Клавиша диктовки: удерживают — запись идёт, отпустили — текст вставляется.
//!
//! Хук, а не `RegisterHotKey`: тот сообщает только о нажатии, а здесь нужно
//! и отпускание, и одиночные клавиши вроде правого Ctrl, которые сочетанием
//! не являются вовсе.
//!
//! Хук стоит на всей системе, и Windows снимает те, что думают дольше
//! положенного. Поэтому внутри только атомарные флаги и отправка в канал, а
//! решения — долгое это нажатие или двойное — принимает другой поток.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::OnceLock;

/// Метка наших собственных нажатий (`dwExtraInfo`). Вставка текста — это
/// Ctrl+V, посланный программой, и хук не должен принять его за то, что
/// человек нажал что-то ещё поверх клавиши диктовки.
pub const OWN_INPUT: usize = 0x474F_564F;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// Клавиша диктовки нажата.
    Down,
    /// Отпущена.
    Up,
    /// Пока клавишу держали, нажали другую: это было сочетание (правый
    /// Ctrl+C), а не диктовка.
    Chord,
    /// Esc во время записи — отменить.
    Cancel,
}

/// Выбор клавиши в настройках: имя, подпись и из чего она состоит.
///
/// Каждый слот — до двух кодов, годится любой (левый или правый Ctrl). Клавиша
/// считается нажатой, когда нажаты все слоты. `swallow` — забирать ли нажатие
/// у системы: F9 и CapsLock иначе сработают и в программе под курсором, а
/// модификаторы забирать нельзя — без них перестанут работать сочетания.
pub struct Preset {
    pub id: &'static str,
    pub title: &'static str,
    slots: [[u32; 2]; 2],
    swallow: bool,
}

pub const PRESETS: &[Preset] = &[
    Preset { id: "rctrl", title: "Правый Ctrl", slots: [[0xA3, 0], [0, 0]], swallow: false },
    Preset { id: "ralt", title: "Правый Alt", slots: [[0xA5, 0], [0, 0]], swallow: false },
    Preset { id: "ctrl+win", title: "Ctrl + Win", slots: [[0xA2, 0xA3], [0x5B, 0x5C]], swallow: false },
    Preset { id: "capslock", title: "Caps Lock", slots: [[0x14, 0], [0, 0]], swallow: true },
    Preset { id: "f8", title: "F8", slots: [[0x77, 0], [0, 0]], swallow: true },
    Preset { id: "f9", title: "F9", slots: [[0x78, 0], [0, 0]], swallow: true },
    Preset { id: "pause", title: "Pause", slots: [[0x13, 0], [0, 0]], swallow: true },
];

pub fn title(id: &str) -> &'static str {
    PRESETS.iter().find(|p| p.id == id).map(|p| p.title).unwrap_or(PRESETS[0].title)
}

static SLOT_A: [AtomicU32; 2] = [AtomicU32::new(0xA3), AtomicU32::new(0)];
static SLOT_B: [AtomicU32; 2] = [AtomicU32::new(0), AtomicU32::new(0)];
static SWALLOW: AtomicBool = AtomicBool::new(false);

static A_DOWN: AtomicBool = AtomicBool::new(false);
static B_DOWN: AtomicBool = AtomicBool::new(false);
/// Клавиша диктовки сейчас нажата целиком.
static HELD: AtomicBool = AtomicBool::new(false);
/// Идёт запись — Esc её отменяет.
static RECORDING: AtomicBool = AtomicBool::new(false);
/// Запись без рук: клавишу уже отпустили, и Esc забираем себе целиком —
/// иначе он заодно закрыл бы окно, в которое человек диктует.
static LOCKED: AtomicBool = AtomicBool::new(false);
/// Выключено — хук пропускает всё насквозь (пауза из трея).
static ENABLED: AtomicBool = AtomicBool::new(true);

static EVENTS: OnceLock<Sender<Event>> = OnceLock::new();

/// Применяет выбор клавиши из настроек.
pub fn configure(id: &str) {
    let preset = PRESETS.iter().find(|p| p.id == id).unwrap_or(&PRESETS[0]);
    for i in 0..2 {
        SLOT_A[i].store(preset.slots[0][i], Ordering::Relaxed);
        SLOT_B[i].store(preset.slots[1][i], Ordering::Relaxed);
    }
    SWALLOW.store(preset.swallow, Ordering::Relaxed);
    A_DOWN.store(false, Ordering::Relaxed);
    B_DOWN.store(false, Ordering::Relaxed);
    HELD.store(false, Ordering::Relaxed);
}

pub fn set_recording(on: bool, locked: bool) {
    RECORDING.store(on, Ordering::Relaxed);
    LOCKED.store(on && locked, Ordering::Relaxed);
}

pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

fn send(event: Event) {
    if let Some(tx) = EVENTS.get() {
        let _ = tx.send(event);
    }
}

/// Ставит хук и отдаёт приёмник событий. Зовётся один раз при запуске.
pub fn install() -> Receiver<Event> {
    let (tx, rx) = channel::<Event>();
    let _ = EVENTS.set(tx);

    std::thread::Builder::new()
        .name("govor-hotkey".into())
        .spawn(hook_loop)
        .ok();

    rx
}

fn hook_loop() {
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, GetMessageW, SetWindowsHookExW, TranslateMessage, MSG, WH_KEYBOARD_LL,
        WH_MOUSE_LL,
    };

    // SAFETY: обычная установка хука и цикл сообщений. Хук снимается вместе
    // с процессом, поток живёт до конца работы.
    unsafe {
        let hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), None, 0);
        if hook.is_err() {
            log::error!("не удалось поставить хук на клавиатуру — клавиша диктовки работать не будет");
            return;
        }
        // Мышь — ради одного: Ctrl+щелчок и Ctrl+колесо тоже сочетания.
        if SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), None, 0).is_err() {
            log::warn!("хук на мышь не встал — Ctrl+щелчок будет начинать запись");
        }

        // Без цикла сообщений хук не вызывается вовсе: система доставляет
        // события через очередь этого потока.
        let mut message = MSG::default();
        while GetMessageW(&mut message, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

fn in_slot(slot: &[AtomicU32; 2], vk: u32) -> bool {
    slot.iter().any(|code| {
        let code = code.load(Ordering::Relaxed);
        code != 0 && code == vk
    })
}

unsafe extern "system" fn keyboard_proc(
    code: i32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::Foundation::LRESULT;
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, HC_ACTION, KBDLLHOOKSTRUCT, WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN,
        WM_SYSKEYUP,
    };

    let pass = || unsafe { CallNextHookEx(None, code, wparam, lparam) };

    if code != HC_ACTION as i32 || !ENABLED.load(Ordering::Relaxed) {
        return pass();
    }

    let message = wparam.0 as u32;
    let down = message == WM_KEYDOWN || message == WM_SYSKEYDOWN;
    let up = message == WM_KEYUP || message == WM_SYSKEYUP;
    let info = unsafe { *(lparam.0 as *const KBDLLHOOKSTRUCT) };
    let vk = info.vkCode;

    // Свои нажатия не разбираем. Чужие программные (AutoHotkey и прочие
    // переназначения клавиш) — разбираем: для человека это его клавиатура.
    if info.dwExtraInfo == OWN_INPUT {
        return pass();
    }

    // Esc во время записи — отмена.
    if vk == 0x1B && RECORDING.load(Ordering::Relaxed) {
        if down {
            send(Event::Cancel);
        }
        if LOCKED.load(Ordering::Relaxed) {
            return LRESULT(1);
        }
        return pass();
    }

    let in_a = in_slot(&SLOT_A, vk);
    let in_b = in_slot(&SLOT_B, vk);
    let two = SLOT_B[0].load(Ordering::Relaxed) != 0;

    if !in_a && !in_b {
        // Другая клавиша поверх удерживаемой — это сочетание, а не диктовка.
        // Одиночные модификаторы в счёт не идут: Shift и Alt нажимают, меняя
        // раскладку, и обрывать из-за этого запись незачем.
        let modifier = matches!(vk, 0x10 | 0x11 | 0x12 | 0x5B | 0x5C | 0xA0..=0xA5 | 0xE8);
        if down && !modifier && HELD.load(Ordering::Relaxed) && !SWALLOW.load(Ordering::Relaxed) {
            send(Event::Chord);
        }
        return pass();
    }

    let swallow = SWALLOW.load(Ordering::Relaxed);
    let flag = if in_a { &A_DOWN } else { &B_DOWN };

    if down {
        // Windows шлёт нажатие снова и снова, пока клавишу держат.
        let repeat = flag.swap(true, Ordering::Relaxed);
        if !repeat {
            let all = A_DOWN.load(Ordering::Relaxed) && (!two || B_DOWN.load(Ordering::Relaxed));
            if all && !HELD.swap(true, Ordering::Relaxed) {
                // Одинокий Alt, отпущенный без другой клавиши, открывает меню
                // окна, Win — «Пуск». Пустая клавиша между нажатием и
                // отпусканием снимает оба эффекта.
                if two || vk == 0xA5 {
                    mask_modifiers();
                }
                send(Event::Down);
            }
        }
    } else if up {
        flag.store(false, Ordering::Relaxed);
        if HELD.swap(false, Ordering::Relaxed) {
            send(Event::Up);
        }
    }

    if swallow {
        return LRESULT(1);
    }
    pass()
}

/// Щелчок или колесо, пока клавишу диктовки держат, — это сочетание с мышью
/// (выделить несколько файлов, изменить масштаб), а не диктовка.
///
/// Зовётся на каждое движение мыши во всей системе, поэтому первым делом —
/// одна проверка флага, и ничего больше.
unsafe extern "system" fn mouse_proc(
    code: i32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, WM_LBUTTONDOWN, WM_MBUTTONDOWN, WM_MOUSEHWHEEL, WM_MOUSEWHEEL,
        WM_RBUTTONDOWN, WM_XBUTTONDOWN,
    };

    if code >= 0 && HELD.load(Ordering::Relaxed) && !SWALLOW.load(Ordering::Relaxed) {
        let message = wparam.0 as u32;
        if matches!(
            message,
            WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN | WM_MOUSEWHEEL | WM_MOUSEHWHEEL
        ) {
            send(Event::Chord);
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// Прячет от Windows, что модификатор нажали «вхолостую» — тем же приёмом
/// пользуется AutoHotkey: клавиша 0xE8 ни за чем не закреплена.
fn mask_modifiers() {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP,
        VIRTUAL_KEY,
    };
    let key = |flags: KEYBD_EVENT_FLAGS| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0xE8),
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: OWN_INPUT,
            },
        },
    };
    let inputs = [key(KEYBD_EVENT_FLAGS(0)), key(KEYEVENTF_KEYUP)];
    // SAFETY: посылает два нажатия несуществующей клавиши, состояние не читает.
    unsafe {
        SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_preset_has_a_key() {
        for preset in PRESETS {
            assert_ne!(preset.slots[0][0], 0, "{}", preset.id);
        }
    }

    #[test]
    fn unknown_name_falls_back_to_the_first_preset() {
        assert_eq!(title("нет такой"), PRESETS[0].title);
    }
}
