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
/// Звук от исправного микрофона приходит через десятки миллисекунд после
/// открытия. Нет его и через секунду с лишним — микрофон завис.
const MIC_SILENT_AFTER: Duration = Duration::from_millis(1200);

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
/// Сколько законченных записей ещё не вставлено. Пока их текст не в окне,
/// следующая запись по ходу речи не печатает — иначе фразы перемешались бы.
static PENDING: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
/// Идёт проверка микрофона из настроек — клавиша диктовки ждёт.
static TESTING_MIC: AtomicBool = AtomicBool::new(false);

/// Дольше этого черновик всей записи расшифровывается слишком долго, и по
/// ходу речи больше не печатаем: остальное вставит окончательная расшифровка.
const STREAM_LIMIT_SECS: usize = 28;

/// Что напечатано в окне по ходу текущей записи.
struct Streamed {
    session: u64,
    /// Окно, куда печатаем. Ушёл человек в другое — печать останавливается.
    window: isize,
    typed: String,
    active: bool,
}

static STREAM: Mutex<Streamed> =
    Mutex::new(Streamed { session: 0, window: 0, typed: String::new(), active: false });

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

    if TESTING_MIC.load(Ordering::SeqCst) {
        return false;
    }
    let session = SESSION.fetch_add(1, Ordering::SeqCst) + 1;
    if !audio::start(&config.microphone) {
        hud(app, "error", "Микрофон не открывается — переподключите его", Some(3200));
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
        let opened_at = Instant::now();
        let mut checked = false;
        while current() {
            let _ = handle.emit_to(HUD_LABEL, "hud:level", audio::level());
            // Микрофон открылся, но звука от него нет: устройство зависло.
            // Дальше держать запись незачем — человек говорит в пустоту.
            if !checked && opened_at.elapsed() >= MIC_SILENT_AFTER {
                checked = true;
                if audio::received() == 0 {
                    log::warn!("микрофон открылся, но за {} мс не отдал звука — запись остановлена", MIC_SILENT_AFTER.as_millis() + HUD_DELAY.as_millis());
                    cancel_recording(&handle, false);
                    hud(&handle, "error", "Микрофон не отдаёт звук — переподключите его", Some(3200));
                    return;
                }
            }
            std::thread::sleep(Duration::from_millis(45));
        }
    });

    // Печать по ходу речи: устоявшееся начало фразы сразу уходит в окно.
    let streaming = config.stream && config.polish.mode == "off";
    *STREAM.lock().unwrap_or_else(|err| err.into_inner()) = Streamed {
        session,
        window: inject::foreground_window(),
        typed: String::new(),
        active: streaming,
    };
    if streaming {
        let handle = app.clone();
        let config = config.clone();
        std::thread::Builder::new()
            .name("govor-stream".into())
            .spawn(move || stream_loop(handle, session, config))
            .ok();
        return true;
    }

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

/// Черновики растущей записи и печать их устоявшегося начала.
fn stream_loop(app: AppHandle, session: u64, config: Config) {
    let current = || SESSION.load(Ordering::SeqCst) == session && recording();
    let hint = text::hint(&config.dictionary);
    let mut previous = String::new();

    std::thread::sleep(Duration::from_millis(900));
    while current() {
        // На процессоре черновик отнял бы время у окончательной расшифровки.
        if !whisper::ready_now() || whisper::on_cpu() {
            std::thread::sleep(Duration::from_millis(300));
            continue;
        }
        let Some((samples, rate)) = audio::snapshot(usize::MAX) else { break };
        if samples.len() / (rate.max(1) as usize) > STREAM_LIMIT_SECS {
            log::info!("запись длиннее {STREAM_LIMIT_SECS} с — остальное вставлю по окончании");
            break;
        }
        let Some(wav) = audio::prepare(&samples, rate).filter(|_| samples.len() >= rate as usize) else {
            std::thread::sleep(Duration::from_millis(300));
            continue;
        };
        drop(samples);

        let began = Instant::now();
        let heard = tauri::async_runtime::block_on(whisper::transcribe(&app, wav, &config.language, &hint));
        let took = began.elapsed();
        let Ok(heard) = heard else { break };
        if !current() {
            break;
        }

        let draft = text::apply(&heard, &config.dictionary, config.voice_commands);
        let stable = text::stable_prefix(&previous, &draft);
        previous = draft;
        if !stable.is_empty() {
            type_stable(session, &stable);
        }
        if took > Duration::from_millis(2500) {
            log::info!("черновик расшифровывается {} мс — по ходу речи больше не печатаю", took.as_millis());
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Допечатывает в окно то, чего там ещё нет из устоявшегося начала.
fn type_stable(session: u64, stable: &str) {
    let mut stream = STREAM.lock().unwrap_or_else(|err| err.into_inner());
    if stream.session != session || !stream.active || !recording() || SESSION.load(Ordering::SeqCst) != session {
        return;
    }
    if PENDING.load(Ordering::SeqCst) > 0 {
        return;
    }
    if inject::foreground_window() != stream.window {
        // Человек ушёл в другое окно — туда без спроса не печатаем.
        stream.active = false;
        return;
    }
    // Начало, уже напечатанное, модель переписала — не трогаем его, пока
    // человек говорит: исправит окончательная расшифровка.
    let Some(rest) = stable.strip_prefix(stream.typed.as_str()) else { return };
    if rest.is_empty() {
        return;
    }
    let rest = rest.to_string();
    inject::type_now(&rest);
    stream.typed.push_str(&rest);
}

/// Забирает напечатанное по ходу записи `session`: окно и сам текст.
fn take_streamed(session: u64) -> Option<(isize, String)> {
    let mut stream = STREAM.lock().unwrap_or_else(|err| err.into_inner());
    if stream.session != session || stream.typed.is_empty() {
        return None;
    }
    stream.active = false;
    Some((stream.window, std::mem::take(&mut stream.typed)))
}

fn cancel_recording(app: &AppHandle, announce: bool) {
    let session = SESSION.fetch_add(1, Ordering::SeqCst);
    RECORDING.store(false, Ordering::SeqCst);
    hotkey::set_recording(false, false);
    audio::cancel();
    // Отмена клавишей Esc убирает и то, что успело напечататься.
    if let Some((window, typed)) = take_streamed(session) {
        if announce && inject::foreground_window() == window {
            inject::replace_tail(typed.chars().count(), "");
        }
    }
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
    let session = SESSION.fetch_add(1, Ordering::SeqCst);
    RECORDING.store(false, Ordering::SeqCst);
    hotkey::set_recording(false, false);
    // После смены номера записи черновик уже ничего не допечатает: замок
    // дождётся, если он печатает прямо сейчас.
    let streamed = take_streamed(session);

    // Записи нет — её уже остановили, и индикатор показывает почему.
    let Some((samples, rate)) = audio::stop() else {
        return;
    };
    let config = config_of(app);
    if config.sounds {
        sound::play(sound::Sound::Stop);
    }

    let seconds = samples.len() as f32 / rate.max(1) as f32;
    if seconds < 0.3 && streamed.is_none() {
        hud(app, "hidden", "", None);
        return;
    }

    hud(app, "transcribing", "", None);
    PENDING.fetch_add(1, Ordering::SeqCst);
    // Куда вставлять, запоминаем сейчас: к концу расшифровки человек может
    // уже переключиться, но тон текста зависит от окна, в которое он говорил.
    let target = inject::foreground();
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        process(app, samples, rate, seconds, target, config, streamed).await;
        PENDING.fetch_sub(1, Ordering::SeqCst);
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
    streamed: Option<(isize, String)>,
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
    let mut insert = result.clone();
    if config.trailing_space && !insert.ends_with('\n') {
        insert.push(' ');
    }
    let window = streamed.as_ref().map_or(target.window, |(window, _)| *window);
    let moved = inject::foreground_window() != window;
    if moved {
        inject::copy(&result);
    } else if let Some((_, typed)) = streamed {
        // Часть уже в окне. Где окончательная расшифровка с ней разошлась,
        // напечатанное стирается до расхождения и набирается заново.
        let same = text::common_prefix(&typed, &insert);
        let erase = typed.chars().count() - same;
        let rest: String = insert.chars().skip(same).collect();
        if erase > 0 {
            log::info!("исправляю напечатанное по ходу речи: стираю {erase} зн.");
        }
        let _ = tauri::async_runtime::spawn_blocking(move || inject::replace_tail(erase, &rest)).await;
    } else {
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

/// Проверка микрофона: пять секунд записи и что из неё расслышано. По ней
/// человек сравнивает микрофоны: громкость видна сразу, а качество звука —
/// по тому, сколько слов распознано верно.
#[tauri::command]
async fn test_microphone(app: AppHandle, device: String) -> Result<serde_json::Value, String> {
    if recording() || TESTING_MIC.swap(true, Ordering::SeqCst) {
        return Err("Сейчас идёт диктовка или другая проверка".into());
    }
    let result = test_microphone_inner(&app, device).await;
    TESTING_MIC.store(false, Ordering::SeqCst);
    result
}

async fn test_microphone_inner(app: &AppHandle, device: String) -> Result<serde_json::Value, String> {
    let config = config_of(app);
    if !whisper::installed(app, &config.model) {
        return Err("Сначала скачайте распознавание на главной".into());
    }
    let opened = tauri::async_runtime::spawn_blocking(move || audio::start(&device))
        .await
        .map_err(|err| err.to_string())?;
    if !opened {
        return Err("Микрофон не открывается — Windows не может с ним работать. Переподключите его".into());
    }
    whisper::warm(app);
    tokio::time::sleep(Duration::from_secs(5)).await;
    let (samples, rate) = tauri::async_runtime::spawn_blocking(audio::stop)
        .await
        .map_err(|err| err.to_string())?
        .ok_or("Микрофон ничего не записал")?;
    if samples.is_empty() {
        return Err("Микрофон открылся, но не отдаёт звук — переподключите его".into());
    }

    let peak = samples.iter().fold(0.0_f32, |max, s| max.max(s.abs()));
    let Some(wav) = audio::prepare(&samples, rate) else {
        return Ok(serde_json::json!({ "peak": peak, "text": "" }));
    };
    let heard = whisper::transcribe(app, wav, &config.language, &text::hint(&config.dictionary)).await?;
    log::info!("проверка микрофона: громкость {:.0}%, расслышано «{heard}»", peak * 100.0);
    Ok(serde_json::json!({ "peak": peak, "text": heard }))
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
            test_microphone,
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

#[cfg(test)]
mod tests {
    use crate::{audio, text, whisper};

    /// Печать по ходу речи от начала до конца, без клавиатуры и окон: растущие
    /// куски записи расшифровываются сервером, устоявшееся «печатается» в
    /// строку, в конце она сводится с окончательной расшифровкой. Итог в
    /// «окне» обязан совпасть с тем, что вставила бы обычная диктовка.
    ///
    /// Нужен работающий сервер whisper.cpp и запись речи:
    /// `GOVOR_TEST_WAV=фраза.wav GOVOR_TEST_PORT=8642 cargo test -- --ignored streaming`.
    #[tokio::test]
    #[ignore]
    async fn streaming_ends_with_the_final_text() {
        let path = std::env::var("GOVOR_TEST_WAV").expect("GOVOR_TEST_WAV — путь к записи речи");
        let port = std::env::var("GOVOR_TEST_PORT").unwrap_or_else(|_| "8642".into());
        let (samples, rate) = audio::parse_wav(&std::fs::read(path).unwrap()).unwrap();
        let client = reqwest::Client::builder().no_proxy().build().unwrap();

        let heard = |wav: Vec<u8>| {
            let client = client.clone();
            let url = format!("http://127.0.0.1:{port}/inference");
            async move {
                let form = reqwest::multipart::Form::new()
                    .text("response_format", "text")
                    .text("language", "ru")
                    .text("temperature", "0.0")
                    .part("file", reqwest::multipart::Part::bytes(wav).file_name("audio.wav"));
                let raw = client.post(url).multipart(form).send().await.unwrap().text().await.unwrap();
                text::apply(&whisper::clean(&raw), &[], true)
            }
        };

        let mut window = String::new();
        let mut previous = String::new();
        let step = rate as usize * 9 / 10;
        let mut end = rate as usize;
        while end < samples.len() {
            let Some(wav) = audio::prepare(&samples[..end], rate) else {
                end += step;
                continue;
            };
            let draft = heard(wav).await;
            let stable = text::stable_prefix(&previous, &draft);
            if let Some(rest) = stable.strip_prefix(window.as_str()) {
                window.push_str(rest);
            }
            println!("{:5.1} с  черновик: {draft}\n        в окне:   {window}", end as f32 / rate as f32);
            previous = draft;
            end += step;
        }

        let mut final_text = heard(audio::prepare(&samples, rate).unwrap()).await;
        final_text.push(' ');
        let same = text::common_prefix(&window, &final_text);
        let erase = window.chars().count() - same;
        let typed_before = window.chars().count();
        window = window.chars().take(same).collect();
        window.extend(final_text.chars().skip(same));
        println!("итог: стёрто {erase} из {typed_before}, в окне: {window}");

        assert!(typed_before > 0, "по ходу речи ничего не напечаталось");
        assert_eq!(window, final_text);
    }
}