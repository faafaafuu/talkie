//! Вставка текста туда, где стоит курсор, и сведения об окне под ним.
//!
//! Основной способ — буфер обмена и Ctrl+V: мгновенно при любой длине и
//! работает везде, где работает вставка. То, что лежало в буфере, возвращается
//! на место. Запасной — посимвольный набор: для программ, которые вставку не
//! принимают, и для случая, когда в буфере лежит не текст (снимок экрана),
//! который мы не сумели бы вернуть.

use windows::Win32::Foundation::{HANDLE, HGLOBAL};
use windows::Win32::System::DataExchange::{
    CloseClipboard, CountClipboardFormats, EmptyClipboard, GetClipboardData,
    IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::CF_UNICODETEXT;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, VIRTUAL_KEY, VK_CONTROL, VK_LWIN, VK_MENU, VK_RETURN,
    VK_RWIN, VK_SHIFT,
};

use crate::hotkey::OWN_INPUT;

/// Окно, в которое диктуют: само окно и имя программы.
#[derive(Clone, Debug, Default)]
pub struct Target {
    pub window: isize,
    pub app: String,
}

/// Окно, которое сейчас впереди, числом. Ноль — такого нет.
pub fn foreground_window() -> isize {
    // SAFETY: только читает состояние системы.
    unsafe { windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow().0 as isize }
}

pub fn foreground() -> Target {
    use windows::core::PWSTR;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

    // SAFETY: только читает состояние системы; дескриптор процесса открыт на
    // чтение имени и закрывается здесь же.
    unsafe {
        let window = GetForegroundWindow();
        if window.0.is_null() {
            return Target::default();
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(window, Some(&mut pid));
        let mut app = String::new();
        if let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
            let mut path = [0u16; 1024];
            let mut size = path.len() as u32;
            if QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, PWSTR(path.as_mut_ptr()), &mut size)
                .is_ok()
            {
                let full = String::from_utf16_lossy(&path[..size as usize]);
                app = std::path::Path::new(&full)
                    .file_stem()
                    .map(|stem| stem.to_string_lossy().into_owned())
                    .unwrap_or_default();
            }
            let _ = CloseHandle(process);
        }
        Target { window: window.0 as isize, app }
    }
}

/// Вставляет текст в окно, которое сейчас впереди.
pub fn insert(text: &str, by_typing: bool) {
    if text.is_empty() {
        return;
    }
    wait_for_modifiers();

    // SAFETY: работа с буфером обмена и посылка нажатий; буфер открывается и
    // закрывается внутри каждой функции.
    unsafe {
        if by_typing || clipboard_holds_non_text() {
            type_text(text);
            return;
        }

        let saved = read_clipboard();
        if !write_clipboard(text) {
            type_text(text);
            return;
        }
        send_ctrl_v();
        // Программа читает буфер не в момент нажатия, а когда до него дойдёт
        // её очередь сообщений. Вернуть прежнее раньше — значит вставить его.
        std::thread::sleep(std::time::Duration::from_millis(400));
        match saved {
            Some(previous) => {
                write_clipboard(&previous);
            }
            None => {
                if OpenClipboard(None).is_ok() {
                    let _ = EmptyClipboard();
                    let _ = CloseClipboard();
                }
            }
        }
    }
}

/// Кладёт текст в буфер обмена — кнопка «копировать» в истории.
pub fn copy(text: &str) -> bool {
    // SAFETY: см. `write_clipboard`.
    unsafe { write_clipboard(text) }
}

/// Ждёт, пока человек отпустит модификаторы.
///
/// Клавиша диктовки — сама модификатор, а запись без рук заканчивают, нажав её
/// ещё раз. Ctrl+V, посланный поверх зажатого Win или Alt, стал бы другим
/// сочетанием.
fn wait_for_modifiers() {
    let held = |vk: u16| unsafe { (GetAsyncKeyState(i32::from(vk)) as u16 & 0x8000) != 0 };
    for _ in 0..60 {
        if ![VK_CONTROL, VK_MENU, VK_SHIFT, VK_LWIN, VK_RWIN].iter().any(|vk| held(vk.0)) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

fn key(vk: VIRTUAL_KEY, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT { wVk: vk, wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: OWN_INPUT },
        },
    }
}

unsafe fn send(inputs: &[INPUT]) {
    SendInput(inputs, std::mem::size_of::<INPUT>() as i32);
}

unsafe fn send_ctrl_v() {
    let none = KEYBD_EVENT_FLAGS(0);
    let v = VIRTUAL_KEY(u16::from(b'V'));
    send(&[
        key(VK_CONTROL, 0, none),
        key(v, 0, none),
        key(v, 0, KEYEVENTF_KEYUP),
        key(VK_CONTROL, 0, KEYEVENTF_KEYUP),
    ]);
}

/// Набирает текст посимвольно.
///
/// Перенос строки — Shift+Enter, а не Enter: в мессенджерах и чатах Enter
/// отправляет сообщение, и продиктованный абзац ушёл бы собеседнику половиной.
unsafe fn type_text(text: &str) {
    let none = KEYBD_EVENT_FLAGS(0);
    let mut batch: Vec<INPUT> = Vec::new();

    for ch in text.chars() {
        if ch == '\r' {
            continue;
        }
        if ch == '\n' {
            batch.extend([
                key(VK_SHIFT, 0, none),
                key(VK_RETURN, 0, none),
                key(VK_RETURN, 0, KEYEVENTF_KEYUP),
                key(VK_SHIFT, 0, KEYEVENTF_KEYUP),
            ]);
        } else {
            let mut units = [0u16; 2];
            for unit in ch.encode_utf16(&mut units) {
                batch.push(key(VIRTUAL_KEY(0), *unit, KEYEVENTF_UNICODE));
                batch.push(key(VIRTUAL_KEY(0), *unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP));
            }
        }
        // Порциями: слишком длинную очередь нажатий часть программ не успевает
        // разобрать и теряет буквы.
        if batch.len() >= 40 {
            send(&batch);
            batch.clear();
            std::thread::sleep(std::time::Duration::from_millis(8));
        }
    }
    if !batch.is_empty() {
        send(&batch);
    }
}

/// Открывает буфер: он бывает занят другой программой — несколько попыток.
unsafe fn open_clipboard() -> bool {
    for _ in 0..10 {
        if OpenClipboard(None).is_ok() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(30));
    }
    false
}

/// Лежит ли в буфере что-то, что не текст: картинка, файлы.
unsafe fn clipboard_holds_non_text() -> bool {
    CountClipboardFormats() > 0 && IsClipboardFormatAvailable(u32::from(CF_UNICODETEXT.0)).is_err()
}

unsafe fn read_clipboard() -> Option<String> {
    if IsClipboardFormatAvailable(u32::from(CF_UNICODETEXT.0)).is_err() || !open_clipboard() {
        return None;
    }
    let text = GetClipboardData(u32::from(CF_UNICODETEXT.0)).ok().and_then(|handle| {
        let block = HGLOBAL(handle.0);
        let pointer = GlobalLock(block) as *const u16;
        if pointer.is_null() {
            return None;
        }
        let mut length = 0usize;
        while *pointer.add(length) != 0 {
            length += 1;
        }
        let text = String::from_utf16_lossy(std::slice::from_raw_parts(pointer, length));
        let _ = GlobalUnlock(block);
        Some(text)
    });
    let _ = CloseClipboard();
    text
}

/// Кладёт текст в буфер. После `SetClipboardData` память принадлежит системе —
/// освобождать её нельзя.
unsafe fn write_clipboard(text: &str) -> bool {
    let mut utf16: Vec<u16> = text.encode_utf16().collect();
    utf16.push(0);

    let Ok(block) = GlobalAlloc(GMEM_MOVEABLE, utf16.len() * 2) else {
        return false;
    };
    let pointer = GlobalLock(block) as *mut u16;
    if pointer.is_null() {
        return false;
    }
    std::ptr::copy_nonoverlapping(utf16.as_ptr(), pointer, utf16.len());
    let _ = GlobalUnlock(block);

    if !open_clipboard() {
        return false;
    }
    let _ = EmptyClipboard();
    let placed = SetClipboardData(u32::from(CF_UNICODETEXT.0), Some(HANDLE(block.0))).is_ok();

    // Просьба к журналу буфера Windows (Win+V) и облачной синхронизации не
    // запоминать это: иначе каждая продиктованная фраза оседала бы в журнале.
    for name in [
        windows::core::w!("ExcludeClipboardContentFromMonitorProcessing"),
        windows::core::w!("CanIncludeInClipboardHistory"),
    ] {
        let format = RegisterClipboardFormatW(name);
        if format == 0 {
            continue;
        }
        // Значение — четыре нулевых байта: для первого формата важно само
        // присутствие, для второго ноль значит «не включать».
        if let Ok(flag) = GlobalAlloc(GMEM_MOVEABLE, 4) {
            let bytes = GlobalLock(flag) as *mut u8;
            if !bytes.is_null() {
                std::ptr::write_bytes(bytes, 0, 4);
                let _ = GlobalUnlock(flag);
                let _ = SetClipboardData(format, Some(HANDLE(flag.0)));
            }
        }
    }

    let _ = CloseClipboard();
    placed
}
