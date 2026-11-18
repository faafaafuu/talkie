//! Клавиша диктовки: удерживают — запись идёт, отпустили — текст вставляется.
//!
//! Хук, а не `RegisterHotKey`: тот сообщает только о нажатии, а здесь нужно
//! и отпускание, и одиночные клавиши вроде правого Ctrl, которые сочетанием
//! не являются вовсе.
//!
//! Хук стоит на всей системе, и Windows снимает те, что думают дольше
//! положенного. Поэтому внутри только атомарные флаги и отправка в канал, а
//! решения — долгое это нажатие или двойное — принимает другой поток.
//!
//! Клавиша — это до трёх «слотов», в каждом до двух кодов (левый или правый
//! Ctrl — любой). Она нажата, когда нажаты все слоты. В настройках хранится
//! либо имя готового варианта (`rctrl`), либо своё сочетание: `keys:` и коды
//! слотов через `+`, внутри слота через `|` — например `keys:162|163+68`.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
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
    /// Человек назначал свою клавишу и нажал её — забрать через `take_capture`.
    Captured,
    /// Назначение отменено: Esc или вышло время.
    CaptureCancelled,
}

type Slots = [[u32; 2]; 3];

/// Готовый вариант клавиши в настройках.
pub struct Preset {
    pub id: &'static str,
    pub title: &'static str,
    slots: Slots,
}

const NONE: [u32; 2] = [0, 0];

pub const PRESETS: &[Preset] = &[
    Preset { id: "rctrl", title: "Правый Ctrl", slots: [[0xA3, 0], NONE, NONE] },
    Preset { id: "ralt", title: "Правый Alt", slots: [[0xA5, 0], NONE, NONE] },
    Preset { id: "ctrl+win", title: "Ctrl + Win", slots: [[0xA2, 0xA3], [0x5B, 0x5C], NONE] },
    Preset { id: "capslock", title: "Caps Lock", slots: [[0x14, 0], NONE, NONE] },
    Preset { id: "f8", title: "F8", slots: [[0x77, 0], NONE, NONE] },
    Preset { id: "f9", title: "F9", slots: [[0x78, 0], NONE, NONE] },
    Preset { id: "pause", title: "Pause", slots: [[0x13, 0], NONE, NONE] },
];

/// Приставка своего сочетания в настройках.
const CUSTOM: &str = "keys:";

fn is_modifier(vk: u32) -> bool {
    matches!(vk, 0x10 | 0x11 | 0x12 | 0x5B | 0x5C | 0xA0..=0xA5)
}

/// Alt и Win, отпущенные без другой клавиши, открывают меню окна и «Пуск».
fn opens_menu(vk: u32) -> bool {
    matches!(vk, 0x12 | 0x5B | 0x5C | 0xA4 | 0xA5)
}

/// Разбирает запись из настроек в слоты. Непонятное — первый готовый вариант:
/// программа с неработающей клавишей хуже программы с клавишей по умолчанию.
fn slots_of(id: &str) -> Slots {
    if let Some(preset) = PRESETS.iter().find(|p| p.id == id) {
        return preset.slots;
    }
    parse_custom(id).unwrap_or(PRESETS[0].slots)
}

fn parse_custom(id: &str) -> Option<Slots> {
    let mut slots: Slots = [NONE; 3];
    let parts: Vec<&str> = id.strip_prefix(CUSTOM)?.split('+').collect();
    if parts.is_empty() || parts.len() > 3 {
        return None;
    }
    for (slot, part) in slots.iter_mut().zip(&parts) {
        let codes: Vec<u32> = part.split('|').filter_map(|code| code.parse().ok()).collect();
        if codes.is_empty() || codes.len() > 2 || codes.iter().any(|code| *code == 0 || *code > 0xFF) {
            return None;
        }
        slot[..codes.len()].copy_from_slice(&codes);
    }
    Some(slots)
}

/// Как клавиша называется для человека.
fn key_name(vk: u32) -> String {
    let fixed = match vk {
        0x08 => "Backspace",
        0x09 => "Tab",
        0x0D => "Enter",
        0x13 => "Pause",
        0x14 => "Caps Lock",
        0x20 => "Пробел",
        0x21 => "Page Up",
        0x22 => "Page Down",
        0x23 => "End",
        0x24 => "Home",
        0x25 => "←",
        0x26 => "↑",
        0x27 => "→",
        0x28 => "↓",
        0x2C => "Print Screen",
        0x2D => "Insert",
        0x2E => "Delete",
        0x5B => "Левый Win",
        0x5C => "Правый Win",
        0x6A => "Num *",
        0x6B => "Num +",
        0x6D => "Num −",
        0x6E => "Num .",
        0x6F => "Num /",
        0x90 => "Num Lock",
        0x91 => "Scroll Lock",
        0xA0 => "Левый Shift",
        0xA1 => "Правый Shift",
        0xA2 => "Левый Ctrl",
        0xA3 => "Правый Ctrl",
        0xA4 => "Левый Alt",
        0xA5 => "Правый Alt",
        0xBA => ";",
        0xBB => "=",
        0xBC => ",",
        0xBD => "-",
        0xBE => ".",
        0xBF => "/",
        0xC0 => "`",
        0xDB => "[",
        0xDC => "\\",
        0xDD => "]",
        0xDE => "'",
        _ => "",
    };
    if !fixed.is_empty() {
        return fixed.to_string();
    }
    match vk {
        0x30..=0x39 | 0x41..=0x5A => char::from(vk as u8).to_string(),
        0x60..=0x69 => format!("Num {}", vk - 0x60),
        0x70..=0x87 => format!("F{}", vk - 0x6F),
        _ => format!("клавиша {vk}"),
    }
}

fn slot_name(slot: [u32; 2]) -> String {
    match slot {
        [0xA2, 0xA3] => "Ctrl".into(),
        [0xA4, 0xA5] => "Alt".into(),
        [0xA0, 0xA1] => "Shift".into(),
        [0x5B, 0x5C] => "Win".into(),
        [code, _] => key_name(code),
    }
}

pub fn title(id: &str) -> String {
    if let Some(preset) = PRESETS.iter().find(|p| p.id == id) {
        return preset.title.to_string();
    }
    match parse_custom(id) {
        Some(slots) => slots
            .iter()
            .filter(|slot| slot[0] != 0)
            .map(|slot| slot_name(*slot))
            .collect::<Vec<_>>()
            .join(" + "),
        None => PRESETS[0].title.to_string(),
    }
}

/// Собирает своё сочетание из нажатых клавиш — или объясняет, чем оно плохо.
///
/// Обычная клавиша без модификатора не годится: она нужна для набора текста,
/// а диктовка забрала бы её себе во всех программах. То же с Shift и буквой —
/// это заглавная буква.
pub fn compose(pressed: &[u32]) -> Result<String, String> {
    let mut modifiers: Vec<u32> = pressed.iter().copied().filter(|vk| is_modifier(*vk)).collect();
    let plain: Vec<u32> = pressed.iter().copied().filter(|vk| !is_modifier(*vk)).collect();
    if pressed.is_empty() {
        return Err("Клавиша не нажата".into());
    }
    if plain.len() > 1 {
        return Err("В сочетании может быть только одна обычная клавиша — остальные Ctrl, Alt, Shift или Win".into());
    }

    // Порядок привычный для глаза: Ctrl, Alt, Shift, Win.
    let rank = |vk: &u32| match *vk {
        0xA2 | 0xA3 => 0,
        0xA4 | 0xA5 => 1,
        0xA0 | 0xA1 => 2,
        _ => 3,
    };
    modifiers.sort_by_key(rank);
    modifiers.dedup_by_key(|vk| rank(&*vk));

    // Свободна ли клавиша от набора текста сама по себе: функциональные,
    // Pause, Caps Lock, Scroll Lock, Insert.
    let standalone = |vk: u32| matches!(vk, 0x13 | 0x14 | 0x2D | 0x91 | 0x70..=0x87);

    if let Some(&key) = plain.first() {
        if key == 0x1B {
            return Err("Esc занят: им отменяют запись".into());
        }
        if modifiers.is_empty() && !standalone(key) {
            return Err(format!(
                "«{}» нужна для набора текста — добавьте к ней Ctrl, Alt или Win",
                key_name(key)
            ));
        }
        let only_shift = modifiers.iter().all(|vk| matches!(vk, 0xA0 | 0xA1));
        if !modifiers.is_empty() && only_shift && !standalone(key) {
            return Err("Shift с клавишей — это заглавная буква; добавьте Ctrl, Alt или Win".into());
        }
    }

    // Одинокий модификатор запоминается с той стороны, с какой нажат: правый
    // Ctrl свободен, а левый нужен для сочетаний. В сочетании сторона не
    // важна — годится любая.
    let single = modifiers.len() == 1 && plain.is_empty();
    let mut parts: Vec<String> = modifiers
        .iter()
        .map(|vk| {
            if single {
                return vk.to_string();
            }
            match *vk {
                0xA2 | 0xA3 => "162|163".into(),
                0xA4 | 0xA5 => "164|165".into(),
                0xA0 | 0xA1 => "160|161".into(),
                _ => "91|92".into(),
            }
        })
        .collect();
    parts.extend(plain.iter().map(u32::to_string));
    Ok(format!("{CUSTOM}{}", parts.join("+")))
}

const fn empty_slot() -> [AtomicU32; 2] {
    [AtomicU32::new(0), AtomicU32::new(0)]
}

static KEYS: [[AtomicU32; 2]; 3] = [[AtomicU32::new(0xA3), AtomicU32::new(0)], empty_slot(), empty_slot()];
static DOWN: [AtomicBool; 3] = [AtomicBool::new(false), AtomicBool::new(false), AtomicBool::new(false)];
/// Сочетание из одних модификаторов: их у системы не забираем, зато другая
/// клавиша поверх означает «это было сочетание, а не диктовка».
static MODS_ONLY: AtomicBool = AtomicBool::new(true);
/// В сочетании есть Alt или Win — после нажатия посылаем пустую клавишу.
static NEED_MASK: AtomicBool = AtomicBool::new(false);
/// Нажатие обычной клавиши сочетания забрали — заберём и отпускание.
static EATEN: AtomicBool = AtomicBool::new(false);

/// Клавиша диктовки — одиночный Alt или Win, и система её не видит вовсе.
///
/// Alt, отпущенный без другой клавиши, открывает меню окна, Win — «Пуск».
/// Пустая клавиша между нажатием и отпусканием (`mask_modifiers`) это гасит
/// только там, где меню ведёт сама Windows. Программы на Electron и Chromium
/// следят за Alt сами: в них каждое второе отпускание открывало меню, фокус
/// уходил из поля ввода, и продиктованное пропадало — «через раз» или
/// целиком, кроме первого слова. Поэтому одиночный Alt или Win забирается
/// у системы целиком, а если поверх нажали другую клавишу (Alt+Tab,
/// Alt+Shift), система получает и модификатор, и эту клавишу — сочетание
/// работает как обычно, а запись отменяется.
static HIDDEN: AtomicBool = AtomicBool::new(false);
/// Системе уже отдали нажатие спрятанного модификатора (было сочетание) —
/// значит, отдать ей и отпускание.
static REPLAYED: AtomicBool = AtomicBool::new(false);

/// Клавиша диктовки сейчас нажата целиком.
static HELD: AtomicBool = AtomicBool::new(false);
/// Идёт запись — Esc её отменяет.
static RECORDING: AtomicBool = AtomicBool::new(false);
/// Запись без рук: клавишу уже отпустили, и Esc забираем себе целиком —
/// иначе он заодно закрыл бы окно, в которое человек диктует.
static LOCKED: AtomicBool = AtomicBool::new(false);
/// Выключено — хук пропускает всё насквозь (пауза из трея).
static ENABLED: AtomicBool = AtomicBool::new(true);

/// Идёт назначение своей клавиши: всё нажатое собирается, а не исполняется.
static CAPTURING: AtomicBool = AtomicBool::new(false);
static CAPTURED: [AtomicU32; 3] = [AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0)];
/// Номер назначения — чтобы таймер старого не оборвал новое.
static CAPTURE_RUN: AtomicU64 = AtomicU64::new(0);

/// Сколько ждать нажатия при назначении. Пока идёт назначение, клавиатура
/// у всей системы отнята, и навсегда так оставаться не должно.
const CAPTURE_SECONDS: u64 = 8;

static EVENTS: OnceLock<Sender<Event>> = OnceLock::new();

/// Модификаторы по порядку битов в `PHYSICAL`.
const MODIFIERS: [u32; 8] = [0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0x5B, 0x5C];

/// Какие модификаторы человек держит на клавиатуре — по событиям хука, без
/// наших собственных нажатий. Нужно вставке текста: она отпускает их на время
/// набора, иначе буквы ушли бы в окно сочетаниями Alt+буква.
static PHYSICAL: AtomicU32 = AtomicU32::new(0);

fn modifier_bit(vk: u32) -> Option<u32> {
    MODIFIERS.iter().position(|code| *code == vk).map(|index| 1 << index)
}

/// Модификаторы, которые сейчас зажаты рукой.
///
/// Хук не видит клавиш на защищённом столе (Ctrl+Alt+Del, запрос UAC), и
/// отпускание там теряется. Поэтому каждый отмеченный модификатор сверяется
/// с состоянием системы: лишний «зажатый» Alt вставка потом нажала бы обратно,
/// и он залип бы.
pub fn held_modifiers() -> Vec<u32> {
    use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;

    let bits = PHYSICAL.load(Ordering::Relaxed);
    MODIFIERS
        .iter()
        .enumerate()
        .filter(|(index, _)| bits & (1 << index) != 0)
        .map(|(_, vk)| *vk)
        .filter(|vk| {
            // SAFETY: только читает состояние клавиши.
            let down = unsafe { (GetAsyncKeyState(*vk as i32) as u16 & 0x8000) != 0 };
            if !down {
                if let Some(bit) = modifier_bit(*vk) {
                    PHYSICAL.fetch_and(!bit, Ordering::Relaxed);
                }
            }
            down
        })
        .collect()
}

/// Применяет выбор клавиши из настроек.
pub fn configure(id: &str) {
    let slots = slots_of(id);
    for (slot, codes) in KEYS.iter().zip(slots) {
        slot[0].store(codes[0], Ordering::Relaxed);
        slot[1].store(codes[1], Ordering::Relaxed);
    }
    let all = slots.iter().flatten().copied().filter(|vk| *vk != 0);
    MODS_ONLY.store(all.clone().all(is_modifier), Ordering::Relaxed);
    NEED_MASK.store(all.clone().any(opens_menu), Ordering::Relaxed);
    let lone = slots[1] == NONE && slots[2] == NONE && slots[0][1] == 0;
    HIDDEN.store(lone && opens_menu(slots[0][0]), Ordering::Relaxed);
    REPLAYED.store(false, Ordering::Relaxed);
    for flag in &DOWN {
        flag.store(false, Ordering::Relaxed);
    }
    EATEN.store(false, Ordering::Relaxed);
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

/// Начинает назначение своей клавиши: следующее нажатие станет ею.
pub fn start_capture() {
    for key in &CAPTURED {
        key.store(0, Ordering::Relaxed);
    }
    let run = CAPTURE_RUN.fetch_add(1, Ordering::SeqCst) + 1;
    CAPTURING.store(true, Ordering::SeqCst);
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(CAPTURE_SECONDS));
        if CAPTURE_RUN.load(Ordering::SeqCst) == run && CAPTURING.swap(false, Ordering::SeqCst) {
            send(Event::CaptureCancelled);
        }
    });
}

pub fn cancel_capture() {
    if CAPTURING.swap(false, Ordering::SeqCst) {
        send(Event::CaptureCancelled);
    }
}

/// Что нажали при назначении: запись для настроек или причина отказа.
pub fn take_capture() -> Result<String, String> {
    let pressed: Vec<u32> = CAPTURED
        .iter()
        .map(|key| key.load(Ordering::Relaxed))
        .filter(|vk| *vk != 0)
        .collect();
    compose(&pressed)
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
        .name("talkie-hotkey".into())
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

/// В каком слоте клавиши диктовки стоит этот код.
fn slot_index(vk: u32) -> Option<usize> {
    KEYS.iter().position(|slot| {
        slot.iter().any(|code| {
            let code = code.load(Ordering::Relaxed);
            code != 0 && code == vk
        })
    })
}

/// Нажаты ли все слоты, кроме названного (`usize::MAX` — все без исключения).
fn slots_down(except: usize) -> bool {
    KEYS.iter().enumerate().all(|(index, slot)| {
        index == except || slot[0].load(Ordering::Relaxed) == 0 || DOWN[index].load(Ordering::Relaxed)
    })
}

/// Назначение своей клавиши: собирает нажатое и ничего не пропускает дальше.
fn capture(vk: u32, down: bool) {
    if vk == 0x1B {
        if down && CAPTURING.swap(false, Ordering::SeqCst) {
            send(Event::CaptureCancelled);
        }
        return;
    }
    if down {
        for key in &CAPTURED {
            let stored = key.load(Ordering::Relaxed);
            if stored == vk {
                return;
            }
            if stored == 0 {
                key.store(vk, Ordering::Relaxed);
                return;
            }
        }
    } else if CAPTURED[0].load(Ordering::Relaxed) != 0 && CAPTURING.swap(false, Ordering::SeqCst) {
        // Первое же отпускание — сочетание набрано.
        send(Event::Captured);
    }
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

    if code != HC_ACTION as i32 {
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

    if let Some(bit) = modifier_bit(vk) {
        if down {
            PHYSICAL.fetch_or(bit, Ordering::Relaxed);
        } else if up {
            PHYSICAL.fetch_and(!bit, Ordering::Relaxed);
        }
    }

    if CAPTURING.load(Ordering::SeqCst) {
        if down || up {
            capture(vk, down);
        }
        return LRESULT(1);
    }

    if !ENABLED.load(Ordering::Relaxed) {
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

    let Some(index) = slot_index(vk) else {
        // Поверх спрятанного Alt или Win нажали другую клавишу — любую, и
        // модификатор тоже: Alt+Shift меняет раскладку. Это сочетание: запись
        // отменяется, а система получает спрятанное нажатие и эту клавишу
        // одним вызовом, в правильном порядке.
        if down
            && vk != 0xE8
            && HELD.load(Ordering::Relaxed)
            && HIDDEN.load(Ordering::Relaxed)
            && !REPLAYED.swap(true, Ordering::Relaxed)
        {
            send(Event::Chord);
            replay(KEYS[0][0].load(Ordering::Relaxed), &info);
            return LRESULT(1);
        }
        // Другая клавиша поверх удерживаемой — это сочетание, а не диктовка.
        // Одиночные модификаторы в счёт не идут: Shift и Alt нажимают, меняя
        // раскладку, и обрывать из-за этого запись незачем.
        if down
            && !is_modifier(vk)
            && vk != 0xE8
            && HELD.load(Ordering::Relaxed)
            && MODS_ONLY.load(Ordering::Relaxed)
        {
            send(Event::Chord);
        }
        return pass();
    };

    // Обычную клавишу сочетания забираем у системы, но только когда остальное
    // сочетание уже нажато: сама по себе буква остаётся буквой.
    let plain = !is_modifier(vk);

    if down {
        // Windows шлёт нажатие снова и снова, пока клавишу держат.
        let repeat = DOWN[index].swap(true, Ordering::Relaxed);
        let rest = slots_down(index);
        let hidden = HIDDEN.load(Ordering::Relaxed);
        if !repeat && rest && !HELD.swap(true, Ordering::Relaxed) {
            if NEED_MASK.load(Ordering::Relaxed) && !hidden {
                mask_modifiers();
            }
            send(Event::Down);
        }
        if hidden && !REPLAYED.load(Ordering::Relaxed) {
            return LRESULT(1);
        }
        // Забранная клавиша остаётся нашей до отпускания, даже если
        // модификаторы отпустили раньше: иначе её повторы напечатались бы.
        if plain && (rest || EATEN.load(Ordering::Relaxed)) {
            EATEN.store(true, Ordering::Relaxed);
            return LRESULT(1);
        }
    } else if up {
        DOWN[index].store(false, Ordering::Relaxed);
        if HELD.swap(false, Ordering::Relaxed) {
            send(Event::Up);
        }
        if plain && EATEN.swap(false, Ordering::Relaxed) {
            return LRESULT(1);
        }
        if HIDDEN.load(Ordering::Relaxed) && !REPLAYED.swap(false, Ordering::Relaxed) {
            return LRESULT(1);
        }
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

    if code >= 0 && HELD.load(Ordering::Relaxed) && MODS_ONLY.load(Ordering::Relaxed) {
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

/// Отдаёт системе спрятанное нажатие модификатора и следом — клавишу, из-за
/// которой это оказалось сочетанием.
fn replay(modifier: u32, key: &windows::Win32::UI::WindowsAndMessaging::KBDLLHOOKSTRUCT) {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        MapVirtualKeyW, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS,
        KEYEVENTF_EXTENDEDKEY, MAPVK_VK_TO_VSC, VIRTUAL_KEY,
    };
    let event = |vk: u32, scan: u32, extended: bool| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk as u16),
                wScan: scan as u16,
                dwFlags: if extended { KEYEVENTF_EXTENDEDKEY } else { KEYBD_EVENT_FLAGS(0) },
                time: 0,
                dwExtraInfo: OWN_INPUT,
            },
        },
    };
    // SAFETY: только переводит код клавиши в скан-код.
    let scan = unsafe { MapVirtualKeyW(modifier, MAPVK_VK_TO_VSC) };
    let inputs = [
        event(modifier, scan, matches!(modifier, 0xA5 | 0x5B | 0x5C)),
        event(key.vkCode, key.scanCode, key.flags.0 & 0x01 != 0),
    ];
    // SAFETY: посылает два нажатия; из хука это допустимо — они встанут в
    // очередь после текущего события, которое мы забираем.
    unsafe {
        SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
    }
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
        assert_eq!(slots_of("keys:мусор"), PRESETS[0].slots);
        assert_eq!(slots_of("keys:1+2+3+4"), PRESETS[0].slots);
    }

    #[test]
    fn a_combination_is_composed_and_named() {
        // Левый Ctrl, левый Shift и D — в любом порядке нажатия.
        let id = compose(&[0x44, 0xA0, 0xA2]).unwrap();
        assert_eq!(id, "keys:162|163+160|161+68");
        assert_eq!(title(&id), "Ctrl + Shift + D");
        assert_eq!(slots_of(&id), [[0xA2, 0xA3], [0xA0, 0xA1], [0x44, 0]]);
    }

    #[test]
    fn a_lone_modifier_keeps_its_side() {
        let id = compose(&[0xA1]).unwrap();
        assert_eq!(id, "keys:161");
        assert_eq!(title(&id), "Правый Shift");
    }

    #[test]
    fn pressed_keys_are_collected_until_release() {
        start_capture();
        capture(0xA4, true);
        capture(0x79, true);
        capture(0x79, true); // повтор от удержания
        assert!(CAPTURING.load(Ordering::SeqCst));
        capture(0x79, false);
        assert!(!CAPTURING.load(Ordering::SeqCst));
        assert_eq!(title(&take_capture().unwrap()), "Alt + F10");
    }
    #[test]
    fn function_keys_work_alone() {
        assert_eq!(title(&compose(&[0x7B]).unwrap()), "F12");
        assert_eq!(title(&compose(&[0xA4, 0x79]).unwrap()), "Alt + F10");
    }

    #[test]
    fn a_lone_alt_or_win_is_hidden_from_the_system() {
        configure("keys:164");
        assert!(HIDDEN.load(Ordering::Relaxed), "левый Alt");
        configure("ralt");
        assert!(HIDDEN.load(Ordering::Relaxed), "правый Alt");
        configure("rctrl");
        assert!(!HIDDEN.load(Ordering::Relaxed), "Ctrl меню не открывает — прятать незачем");
        configure("ctrl+win");
        assert!(!HIDDEN.load(Ordering::Relaxed), "сочетание из двух клавиш");
        configure("keys:164|165+121");
        assert!(!HIDDEN.load(Ordering::Relaxed), "Alt + F10");
        configure("rctrl");
    }

    #[test]
    fn typing_keys_are_refused() {
        assert!(compose(&[0x44]).is_err(), "буква без модификатора");
        assert!(compose(&[0xA0, 0x44]).is_err(), "Shift с буквой — заглавная");
        assert!(compose(&[0xA2, 0x44, 0x46]).is_err(), "две обычные клавиши");
        assert!(compose(&[0x1B]).is_err(), "Esc отменяет запись");
        assert!(compose(&[]).is_err());
        // А с Ctrl буква годится.
        assert!(compose(&[0xA2, 0x44]).is_ok());
    }
}
