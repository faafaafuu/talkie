//! Говор — диктовка голосом в любое окно.
//!
//! Удерживают клавишу и говорят; отпустили — текст появляется там, где стоит
//! курсор. Два быстрых нажатия — запись без рук до следующего нажатия.
//! Распознаёт whisper.cpp на этом же компьютере, поэтому программе не нужны
//! ни учётная запись, ни подписка, а сказанное не уходит в сеть.
//!
//! Здесь — связка: клавиша → запись → расшифровка → правка → вставка, и окна.

mod audio;
mod config;
mod history;
mod hotkey;
mod inject;
mod instance;
mod jobs;
mod polish;
mod secret;
mod sound;
mod text;
mod whisper;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

use config::Config;
use history::History;

const MAIN_LABEL: &str = "main";
const HUD_LABEL: &str = "hud";
const HUD_WIDTH: f64 = 460.0;
const HUD_HEIGHT: f64 = 132.0;

/// Нажатие короче этого — «тап», а не диктовка: за четверть секунды слова
/// не сказать.
const TAP: Duration = Duration::from_millis(260);
/// Два тапа ближе этого друг к другу — запись без рук.
const DOUBLE_TAP: Duration = Duration::from_millis(450);
/// Индикатор появляется не сразу: правый Ctrl нажимают и ради Ctrl+C, и
/// мигать при каждом таком нажатии незачем.
const HUD_DELAY: Duration = Duration::from_millis(170);

pub struct AppState {
    dir: std::path::PathBuf,
    config: Mutex<Config>,
    history: Mutex<History>,
}

/// Настройки на этот момент — копией, чтобы не держать замок.
pub fn config_of(app: &AppHandle) -> Config {
    app.state::<AppState>().config.lock().unwrap_or_else(|err| err.into_inner()).clone()
}

static RECORDING: AtomicBool = AtomicBool::new(false);
/// Номер записи. Растёт с каждой начатой, законченной и отменённой: по нему
/// фоновые потоки (полоски громкости, текст «на лету») узнают, что их запись
/// уже не текущая.
static SESSION: AtomicU64 = AtomicU64::new(0);
/// Номер показа индикатора — чтобы отложенное скрытие не спрятало новый показ.
static HUD_SHOWN: AtomicU64 = AtomicU64::new(0);
/// Расшифровки идут по одной и в том порядке, в каком были сказаны.
static PIPELINE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub fn recording() -> bool {
    RECORDING.load(Ordering::SeqCst)
}

/* ── Индикатор ────────────────────────────────────────────────────────────── */

fn build_hud(app: &AppHandle) -> tauri::Result<WebviewWindow> {
    let window = WebviewWindowBuilder::new(app, HUD_LABEL, WebviewUrl::App("hud.html".into()))
        .title("Говор — индикатор")
        .inner_size(HUD_WIDTH, HUD_HEIGHT)
        .decorations(false)
        .transparent(true)
        .always_on_top(true)
        .skip_taskbar(true)
        .resizable(false)
        .shadow(false)
        // Не забирать фокус: курсор должен остаться в поле, куда диктуют.
        .focused(false)
        .visible(false)
        .build()?;

    // Сквозь него можно щёлкать: это картинка, а не орган управления.
    let _ = window.set_ignore_cursor_events(true);
    make_passive(&window);
    Ok(window)
}

/// Делает индикатор картинкой поверх экрана, а не окном программы: его нет на
/// панели задач и в Alt+Tab, и он не становится передним при показе — иначе
/// текст вставлялся бы в него самого.
fn make_passive(window: &WebviewWindow) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, SetWindowLongPtrW, SetWindowPos, GWL_EXSTYLE, SWP_FRAMECHANGED,
        SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, WS_EX_APPWINDOW, WS_EX_NOACTIVATE,
        WS_EX_TOOLWINDOW,
    };

    let Ok(raw) = window.hwnd() else { return };
    let hwnd = HWND(raw.0 as _);
    // SAFETY: окно наше и живое; меняются только биты стиля.
    unsafe {
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let ex = (ex | (WS_EX_TOOLWINDOW.0 | WS_EX_NOACTIVATE.0) as isize) & !(WS_EX_APPWINDOW.0 as isize);
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex);
        let _ = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

/// Ставит индикатор внизу по центру того экрана, где сейчас указатель мыши.
fn place_hud(app: &AppHandle, window: &WebviewWindow) {
    let monitors = window.available_monitors().unwrap_or_default();
    let cursor = app.cursor_position().ok();
    let monitor = cursor
        .and_then(|at| {
            monitors.iter().find(|m| {
                let (pos, size) = (m.position(), m.size());
                at.x >= f64::from(pos.x)
                    && at.x < f64::from(pos.x) + f64::from(size.width)
                    && at.y >= f64::from(pos.y)
                    && at.y < f64::from(pos.y) + f64::from(size.height)
            })
        })
        .cloned()
        .or_else(|| window.primary_monitor().ok().flatten());
    let Some(monitor) = monitor else { return };

    let scale = monitor.scale_factor();
    let (width, height) = ((HUD_WIDTH * scale) as i32, (HUD_HEIGHT * scale) as i32);
    let area = monitor.work_area();
    let x = area.position.x + (area.size.width as i32 - width) / 2;
    let y = area.position.y + area.size.height as i32 - height - (18.0 * scale) as i32;

    let _ = window.set_size(tauri::PhysicalSize::new(width, height));
    let _ = window.set_position(tauri::PhysicalPosition::new(x, y));
}

/// Показывает состояние в индикаторе. `hide_after` — спрятать через столько
/// миллисекунд, если за это время не показали что-то новое.
fn hud(app: &AppHandle, state: &str, text: &str, hide_after: Option<u64>) {
    let Some(window) = app.get_webview_window(HUD_LABEL) else { return };
    let shown = HUD_SHOWN.fetch_add(1, Ordering::SeqCst) + 1;

    if state == "hidden" {
        let _ = window.emit("hud", serde_json::json!({ "state": "hidden", "text": "" }));
        let _ = window.hide();
        return;
    }

    if !window.is_visible().unwrap_or(false) {
        place_hud(app, &window);
        let _ = window.show();
    }
    let _ = window.emit("hud", serde_json::json!({ "state": state, "text": text }));

    if let Some(ms) = hide_after {
        let app = app.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(ms));
            if HUD_SHOWN.load(Ordering::SeqCst) == shown {
                hud(&app, "hidden", "", None);
            }
        });
    }
}

/* ── Запись ───────────────────────────────────────────────────────────────── */

#[derive(Clone, Copy, PartialEq)]
enum Phase {
    Idle,
    /// Клавишу держат с этого момента.
    Holding(Instant),
    /// Запись без рук: клавиша отпущена, запись идёт до следующего нажатия.
    Locked,
    /// Запись уже решена (закончена или отменена), ждём отпускания клавиши.
    Released,
}

/// Разбирает нажатия клавиши диктовки: удержание, двойной тап, отмена.
fn controller(app: AppHandle, events: Receiver<hotkey::Event>) {
    use hotkey::Event;

    let mut phase = Phase::Idle;
    let mut last_tap: Option<Instant> = None;

    for event in events {
        // Назначение своей клавиши к записи отношения не имеет: итог — окну.
        if matches!(event, Event::Captured | Event::CaptureCancelled) {
            let payload = match (event, hotkey::take_capture()) {
                (Event::Captured, Ok(id)) => serde_json::json!({ "id": id, "title": hotkey::title(&id) }),
                (Event::Captured, Err(reason)) => serde_json::json!({ "error": reason }),
                _ => serde_json::json!({ "cancelled": true }),
            };
            log::info!("назначение клавиши: {payload}");
            let _ = app.emit_to(MAIN_LABEL, "hotkey:captured", payload);
            continue;
        }
        phase = match (event, phase) {
            (Event::Down, Phase::Idle) => {
                if start_recording(&app) {
                    Phase::Holding(Instant::now())
                } else {
                    Phase::Released
                }
            }
            (Event::Down, Phase::Locked) => {
                finish_recording(&app);
                Phase::Released
            }
            (Event::Up, Phase::Holding(since)) => {
                if since.elapsed() >= TAP {
                    finish_recording(&app);
                    Phase::Idle
                } else if last_tap.is_some_and(|at| at.elapsed() < DOUBLE_TAP) {
                    // Второй тап подряд: запись, начатая им, продолжается без рук.
                    last_tap = None;
                    hotkey::set_recording(true, true);
                    // Если индикатор уже на экране, сигнал начала уже звучал.
                    let shown = app
                        .get_webview_window(HUD_LABEL)
                        .is_some_and(|window| window.is_visible().unwrap_or(false));
                    if !shown && config_of(&app).sounds {
                        sound::play(sound::Sound::Start);
                    }
                    hud(&app, "locked", "", None);
                    Phase::Locked
                } else {
                    last_tap = Some(Instant::now());
                    cancel_recording(&app, false);
                    Phase::Idle
                }
            }
            (Event::Up, Phase::Released) => Phase::Idle,
            (Event::Chord, Phase::Holding(_)) => {
                cancel_recording(&app, false);
                Phase::Released
            }
            (Event::Cancel, Phase::Holding(_)) => {
                cancel_recording(&app, true);
                Phase::Released
            }
            (Event::Cancel, Phase::Locked) => {
                cancel_recording(&app, true);
                Phase::Idle
            }
            (_, phase) => phase,
        };
    }
}

fn start_recording(app: &AppHandle) -> bool {
    let config = config_of(app);

    if !whisper::installed(app, &config.model) {
        show_main(app);
        let _ = app.emit("need-install", ());
        return false;
    }

    let session = SESSION.fetch_add(1, Ordering::SeqCst) + 1;
    if !audio::start(&config.microphone) {
        hud(app, "error", "Микрофон недоступен", Some(2400));
        return false;
    }
    RECORDING.store(true, Ordering::SeqCst);
    hotkey::set_recording(true, false);
    // Модели грузятся, пока человек ещё говорит.
    whisper::warm(app);
    polish::warm(&config.polish);

    let current = move || SESSION.load(Ordering::SeqCst) == session && recording();

    // Индикатор и полоски громкости.
    let handle = app.clone();
    let sounds = config.sounds;
    std::thread::spawn(move || {
        std::thread::sleep(HUD_DELAY);
        if !current() {
            return;
        }
        // Запись без рук показала себя сама — второй сигнал и смена подписи
        // были бы лишними.
        let already = handle
            .get_webview_window(HUD_LABEL)
            .is_some_and(|window| window.is_visible().unwrap_or(false));
        if !already {
            if sounds {
                sound::play(sound::Sound::Start);
            }
            hud(&handle, "listening", "", None);
        }
        while current() {
            let _ = handle.emit_to(HUD_LABEL, "hud:level", audio::level());
            std::thread::sleep(Duration::from_millis(45));
        }
    });

    // Текст «на лету»: пока человек говорит, уже сказанное расшифровывается
    // и показывается в индикаторе. Только на видеокарте — на процессоре это
    // отняло бы время у окончательной расшифровки.
    if config.live_preview {
        let handle = app.clone();
        let language = config.language.clone();
        let hint = text::hint(&config.dictionary);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(1300));
            while current() {
                if whisper::ready_now() && !whisper::on_cpu() {
                    let piece = audio::snapshot(25)
                        .filter(|(samples, rate)| samples.len() as u32 >= *rate)
                        .and_then(|(samples, rate)| audio::prepare(&samples, rate));
                    if let Some(wav) = piece {
                        let heard = tauri::async_runtime::block_on(whisper::transcribe(
                            &handle, wav, &language, &hint,
                        ));
                        if let (Ok(heard), true) = (heard, current()) {
                            if !heard.is_empty() {
                                let _ = handle.emit_to(HUD_LABEL, "hud:partial", heard);
                            }
                        }
                    }
                }
                std::thread::sleep(Duration::from_millis(700));
            }
        });
    }

    true
}

fn cancel_recording(app: &AppHandle, announce: bool) {
    SESSION.fetch_add(1, Ordering::SeqCst);
    RECORDING.store(false, Ordering::SeqCst);
    hotkey::set_recording(false, false);
    audio::cancel();
    if announce {
        if config_of(app).sounds {
            sound::play(sound::Sound::Cancel);
        }
        hud(app, "cancelled", "Отменено", Some(700));
    } else {
        hud(app, "hidden", "", None);
    }
}

fn finish_recording(app: &AppHandle) {
    SESSION.fetch_add(1, Ordering::SeqCst);
    RECORDING.store(false, Ordering::SeqCst);
    hotkey::set_recording(false, false);

    let Some((samples, rate)) = audio::stop() else {
        hud(app, "hidden", "", None);
        return;
    };
    let config = config_of(app);
    if config.sounds {
        sound::play(sound::Sound::Stop);
    }

    let seconds = samples.len() as f32 / rate.max(1) as f32;
    if seconds < 0.3 {
        hud(app, "hidden", "", None);
        return;
    }

    hud(app, "transcribing", "", None);
    // Куда вставлять, запоминаем сейчас: к концу расшифровки человек может
    // уже переключиться, но тон текста зависит от окна, в которое он говорил.
    let target = inject::foreground();
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        process(app, samples, rate, seconds, target, config).await;
    });
}

/// Показывает состояние обработки, если за это время не началась новая запись:
/// её индикатор важнее хвоста предыдущей.
fn hud_progress(app: &AppHandle, state: &str, text: &str, hide_after: Option<u64>) {
    if !recording() {
        hud(app, state, text, hide_after);
    }
}

async fn process(
    app: AppHandle,
    samples: Vec<f32>,
    rate: u32,
    seconds: f32,
    target: inject::Target,
    config: Config,
) {
    let _turn = PIPELINE.lock().await;
    let started = Instant::now();

    let Some(wav) = audio::prepare(&samples, rate) else {
        log::warn!("в записи тишина ({seconds:.1} с) — проверьте, тот ли микрофон выбран");
        hud_progress(&app, "error", "Тишина — проверьте микрофон", Some(2400));
        return;
    };
    drop(samples);

    let hint = text::hint(&config.dictionary);
    let raw = match whisper::transcribe(&app, wav, &config.language, &hint).await {
        Ok(raw) => raw,
        Err(err) => {
            log::error!("расшифровка не удалась: {err}");
            hud_progress(&app, "error", "Распознавание не ответило", Some(2800));
            return;
        }
    };
    let heard_ms = started.elapsed().as_millis();
    if raw.is_empty() {
        hud_progress(&app, "error", "Не расслышал", Some(1800));
        return;
    }

    let ruled = text::apply(&raw, &config.dictionary, config.voice_commands);
    let mut result = ruled.clone();

    if polish::wanted(&config.polish, &ruled) {
        hud_progress(&app, "polishing", "", None);
        match polish::polish(&config.polish, &ruled, &target.app, &hint).await {
            Ok(polished) => result = polished,
            Err(err) => log::warn!("правка моделью не удалась, оставляю как расслышал: {err}"),
        }
    }
    if result.trim().is_empty() {
        hud_progress(&app, "error", "Не расслышал", Some(1800));
        return;
    }

    // Вставляем только в то окно, в которое диктовали. Расшифровка длинной
    // записи или правка занимают секунды, и человек мог уйти в другую
    // программу: текст, набранный туда без спроса, хуже текста, оставленного
    // в буфере обмена.
    let moved = inject::foreground_window() != target.window;
    if moved {
        inject::copy(&result);
    } else {
        let mut insert = result.clone();
        if config.trailing_space && !insert.ends_with('\n') {
            insert.push(' ');
        }
        let by_typing = config.insert == "type";
        let _ = tauri::async_runtime::spawn_blocking(move || inject::insert(&insert, by_typing)).await;
    }

    let words = text::words(&result);
    log::info!(
        "диктовка: {seconds:.1} с речи, {words} слов, расшифровка {heard_ms} мс, всего {} мс, в «{}»",
        started.elapsed().as_millis(),
        target.app
    );

    {
        let state = app.state::<AppState>();
        let mut history = state.history.lock().unwrap_or_else(|err| err.into_inner());
        history.add(
            history::Item {
                id: 0,
                at: chrono::Local::now().to_rfc3339(),
                raw: if raw == result { String::new() } else { raw },
                text: result,
                app: target.app,
                seconds,
                words,
            },
            config.keep_history,
        );
        history.save(&state.dir);
    }
    let _ = app.emit_to(MAIN_LABEL, "history", ());
    if moved {
        hud_progress(&app, "done", "Окно сменилось — текст в буфере, Ctrl+V", Some(3200));
    } else {
        hud_progress(&app, "done", "", Some(650));
    }
}

/* ── Окна и трей ──────────────────────────────────────────────────────────── */

pub fn show_main(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(MAIN_LABEL) {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
        return;
    }
    let built = WebviewWindowBuilder::new(app, MAIN_LABEL, WebviewUrl::App("index.html".into()))
        .title("Говор")
        .inner_size(940.0, 660.0)
        .min_inner_size(780.0, 540.0)
        .center()
        .build();
    if let Err(err) = built {
        log::error!("главное окно не открылось: {err}");
    }
}

fn setup_tray(app: &AppHandle) -> tauri::Result<()> {
    use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
    use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

    let open = MenuItem::with_id(app, "open", "Открыть Говор", true, None::<&str>)?;
    let pause = CheckMenuItem::with_id(app, "pause", "Пауза — не слушать клавишу", true, false, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Выйти", true, None::<&str>)?;
    let separator = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(app, &[&open, &pause, &separator, &quit])?;
    let pause_item = pause.clone();

    let mut tray = TrayIconBuilder::with_id("govor-tray")
        .tooltip("Говор — диктовка")
        .menu(&menu)
        .show_menu_on_left_click(false);
    // Без явной иконки значок в трее пустой, то есть его не видно.
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }

    tray.on_menu_event(move |app, event| match event.id().as_ref() {
        "open" => show_main(app),
        "pause" => {
            let paused = hotkey::enabled();
            hotkey::set_enabled(!paused);
            let _ = pause_item.set_checked(paused);
            if paused && recording() {
                cancel_recording(app, false);
            }
            let _ = app.emit_to(MAIN_LABEL, "history", ());
        }
        "quit" => app.exit(0),
        _ => {}
    })
    .on_tray_icon_event(|tray, event| {
        if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
            show_main(tray.app_handle());
        }
    })
    .build(app)?;
    Ok(())
}

/* ── Команды окна ─────────────────────────────────────────────────────────── */

#[tauri::command]
fn get_state(app: AppHandle, state: tauri::State<AppState>) -> serde_json::Value {
    let mut config = state.config.lock().unwrap_or_else(|err| err.into_inner()).clone();
    // Ключ в окно не отдаём: ему достаточно знать, что ключ есть.
    let has_key = !config.polish.key.is_empty();
    config.polish.key = String::new();

    let history = state.history.lock().unwrap_or_else(|err| err.into_inner()).clone();
    let model = whisper::resolve(&config.model);
    let root = whisper::root(&app).unwrap_or_default();

    serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "config": config,
        "hasKey": has_key,
        "installed": whisper::installed(&app, &config.model),
        "installing": whisper::installing(),
        "gpu": whisper::has_gpu(),
        "loaded": whisper::ready_now(),
        "paused": !hotkey::enabled(),
        "model": { "id": model.id, "title": model.title, "megabytes": model.megabytes },
        "models": whisper::MODELS.iter().map(|m| serde_json::json!({
            "id": m.id,
            "title": m.title,
            "megabytes": m.megabytes,
            "installed": root.join(m.file).exists(),
        })).collect::<Vec<_>>(),
        "hotkeys": hotkey::PRESETS.iter().map(|p| serde_json::json!({ "id": p.id, "title": p.title })).collect::<Vec<_>>(),
        "hotkeyTitle": hotkey::title(&config.hotkey),
        "devices": audio::devices(),
        "history": history.items,
        "totals": history.totals,
    })
}

/// Сохраняет настройки. `key` — новый ключ сервиса правки; `None` — оставить
/// прежний (окно его не знает и прислать обратно не может).
#[tauri::command]
fn save_config(
    app: AppHandle,
    state: tauri::State<AppState>,
    mut config: Config,
    key: Option<String>,
) -> Result<(), String> {
    let previous = state.config.lock().unwrap_or_else(|err| err.into_inner()).clone();
    config.polish.key = key.unwrap_or_else(|| previous.polish.key.clone());

    config::save(&state.dir, &config)?;
    *state.config.lock().unwrap_or_else(|err| err.into_inner()) = config.clone();

    hotkey::configure(&config.hotkey);
    if previous.model != config.model {
        // Сервер поднят с прежней моделью — следующая диктовка поднимет новый.
        whisper::shutdown();
    }
    if previous.autostart != config.autostart {
        use tauri_plugin_autostart::ManagerExt;
        let launcher = app.autolaunch();
        let changed = if config.autostart { launcher.enable() } else { launcher.disable() };
        if let Err(err) = changed {
            log::warn!("автозапуск не переключился: {err}");
        }
    }
    Ok(())
}

#[tauri::command]
async fn install_engine(app: AppHandle) -> Result<(), String> {
    let model = config_of(&app).model;
    whisper::install(app, model).await
}

#[tauri::command]
fn history_delete(app: AppHandle, state: tauri::State<AppState>, id: u64) {
    let mut history = state.history.lock().unwrap_or_else(|err| err.into_inner());
    history.remove(id);
    history.save(&state.dir);
    let _ = app.emit_to(MAIN_LABEL, "history", ());
}

#[tauri::command]
fn history_clear(app: AppHandle, state: tauri::State<AppState>) {
    let mut history = state.history.lock().unwrap_or_else(|err| err.into_inner());
    history.clear();
    history.save(&state.dir);
    let _ = app.emit_to(MAIN_LABEL, "history", ());
}

#[tauri::command]
fn copy_text(text: String) -> bool {
    inject::copy(&text)
}

#[tauri::command]
async fn ollama_models(url: String) -> Result<Vec<String>, String> {
    polish::ollama_models(&url).await
}

#[tauri::command]
fn unload_model() {
    whisper::shutdown();
}

/// Начинает назначение своей клавиши; итог придёт событием `hotkey:captured`.
#[tauri::command]
fn capture_hotkey() {
    log::info!("жду нажатия для назначения клавиши");
    hotkey::start_capture();
}

#[tauri::command]
fn cancel_capture() {
    hotkey::cancel_capture();
}

/* ── Запуск ───────────────────────────────────────────────────────────────── */

pub fn run() {
    // Просьба закрыться — от установщика: передать работающей копии и уйти.
    if std::env::args().any(|arg| arg == "--quit") {
        instance::request_quit();
        return;
    }
    if !instance::claim() {
        return;
    }
    // С автозапуска программа поднимается молча, без окна.
    let background = std::env::args().any(|arg| arg == "--background");

    tauri::Builder::default()
        .plugin(
            tauri_plugin_log::Builder::new()
                .clear_targets()
                .target(tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::LogDir {
                    file_name: Some("govor".into()),
                }))
                .target(tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::Stdout))
                .level(log::LevelFilter::Info)
                .max_file_size(2 * 1024 * 1024)
                .rotation_strategy(tauri_plugin_log::RotationStrategy::KeepSome(2))
                .build(),
        )
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--background"]),
        ))
        .invoke_handler(tauri::generate_handler![
            get_state,
            save_config,
            install_engine,
            history_delete,
            history_clear,
            copy_text,
            ollama_models,
            unload_model,
            capture_hotkey,
            cancel_capture,
        ])
        .on_window_event(|window, event| {
            // Крестик прячет окно, а не закрывает программу: она живёт в трее.
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if window.label() == MAIN_LABEL {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .setup(move |app| {
            let handle = app.handle().clone();
            let dir = handle.path().app_data_dir()?;
            let config = config::load(&dir);
            log::info!("Говор {} запущен, клавиша «{}»", env!("CARGO_PKG_VERSION"), hotkey::title(&config.hotkey));

            hotkey::configure(&config.hotkey);
            let first_run = !config.onboarded;
            app.manage(AppState {
                history: Mutex::new(History::load(&dir)),
                config: Mutex::new(config),
                dir,
            });

            build_hud(&handle)?;
            setup_tray(&handle)?;
            instance::listen(handle.clone());
            whisper::watch_idle(handle.clone());
            // Видеокарта определяется заранее: первый вопрос о ней — запуск
            // сторонней программы, и ждать его в момент нажатия клавиши незачем.
            std::thread::spawn(whisper::has_gpu);

            let events = hotkey::install();
            let controller_handle = handle.clone();
            std::thread::Builder::new()
                .name("govor-controller".into())
                .spawn(move || controller(controller_handle, events))?;

            if !background || first_run {
                show_main(&handle);
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("программа не запустилась")
        .run(|_, event| {
            if let tauri::RunEvent::Exit = event {
                whisper::shutdown();
            }
        });
}
