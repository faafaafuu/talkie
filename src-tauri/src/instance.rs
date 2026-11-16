//! Единственный экземпляр программы.
//!
//! Именованный мьютекс — признак «я уже здесь», именованные события — способ
//! попросить первую копию показать окно или закрыться. Вторая копия ставит
//! событие и тихо уходит: со стороны ярлык просто возвращает уже запущенную
//! программу.
//!
//! При любой неясности считаем, что мы первые: ошибочный отказ страшнее лишней
//! копии — программа просто не откроется.

use windows::core::w;
use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, SetEvent, WaitForMultipleObjects, INFINITE,
};

// `Local\` — в пределах сеанса: на общем компьютере два человека вправе
// пользоваться программой одновременно.
const MUTEX_NAME: windows::core::PCWSTR = w!("Local\\app.govor.dictation.instance");
const SHOW_EVENT: windows::core::PCWSTR = w!("Local\\app.govor.dictation.show");
const QUIT_EVENT: windows::core::PCWSTR = w!("Local\\app.govor.dictation.quit");

/// Занимает право быть единственной копией. `false` — программа уже запущена
/// и её уже попросили показать окно.
pub fn claim() -> bool {
    // SAFETY: создание именованных объектов ядра; дескриптор мьютекса
    // намеренно не закрывается — он должен жить, пока жива программа.
    unsafe {
        let Ok(mutex) = CreateMutexW(None, true, MUTEX_NAME) else {
            return true;
        };
        if GetLastError() != ERROR_ALREADY_EXISTS {
            let _ = mutex;
            return true;
        }
        if let Ok(event) = CreateEventW(None, false, false, SHOW_EVENT) {
            let _ = SetEvent(event);
        }
        false
    }
}

/// Просит работающую копию закрыться — так установщик останавливает программу
/// перед заменой файлов, и значок в трее не остаётся призраком.
pub fn request_quit() {
    // SAFETY: открывает событие по имени и ставит его.
    unsafe {
        if let Ok(event) = CreateEventW(None, false, false, QUIT_EVENT) {
            let _ = SetEvent(event);
        }
    }
}

/// Слушает просьбы от других копий.
pub fn listen(app: tauri::AppHandle) {
    std::thread::spawn(move || unsafe {
        let (Ok(show), Ok(quit)) = (
            CreateEventW(None, false, false, SHOW_EVENT),
            CreateEventW(None, false, false, QUIT_EVENT),
        ) else {
            log::warn!("события единственной копии не создались — повторный запуск ничего не покажет");
            return;
        };

        let events = [show, quit];
        loop {
            let signaled = WaitForMultipleObjects(&events, false, INFINITE);
            if signaled == WAIT_OBJECT_0 {
                let handle = app.clone();
                let _ = app.run_on_main_thread(move || crate::show_main(&handle));
            } else if signaled.0 == WAIT_OBJECT_0.0 + 1 {
                log::info!("получена просьба закрыться — выхожу");
                let handle = app.clone();
                let _ = app.run_on_main_thread(move || handle.exit(0));
                return;
            } else {
                return;
            }
        }
    });
}
