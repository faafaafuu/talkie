//! Расшифровка речи: whisper.cpp на этом же компьютере.
//!
//! Сборок две, и выбирается она сама. С видеокартой NVIDIA берётся вариант с
//! CUDA: он тяжелее, но расшифровывает фразу за доли секунды. Без видеокарты
//! качать его незачем — там всё равно считает процессор.
//!
//! Работает сервером, а не запуском программы на каждую фразу: загрузка модели
//! в память занимает секунды, сама расшифровка — доли секунды. Сервер
//! поднимается с первым нажатием клавиши и уходит после простоя.

use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use std::sync::Mutex;

use tauri::{AppHandle, Emitter, Manager};

/// Событие с ходом загрузки для окна.
const EVENT: &str = "install";

/// Версия закреплена: это исполняемый файл, который мы запускаем у человека.
const RELEASE: &str = "https://github.com/ggml-org/whisper.cpp/releases/download/v1.9.2";
const CUDA_ZIP: &str = "whisper-cublas-12.4.0-bin-x64.zip";
const CPU_ZIP: &str = "whisper-blas-bin-x64.zip";

const MODELS_URL: &str = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main";

pub struct Model {
    pub id: &'static str,
    pub file: &'static str,
    pub title: &'static str,
    pub megabytes: u32,
}

pub const MODELS: &[Model] = &[
    Model {
        id: "turbo",
        file: "ggml-large-v3-turbo.bin",
        title: "Точная — large-v3-turbo, для видеокарты",
        megabytes: 1620,
    },
    Model {
        id: "turbo-q5",
        file: "ggml-large-v3-turbo-q5_0.bin",
        title: "Точная сжатая — large-v3-turbo q5",
        megabytes: 574,
    },
    Model {
        id: "small",
        file: "ggml-small.bin",
        title: "Быстрая — small, для процессора",
        megabytes: 488,
    },
    Model {
        id: "base",
        file: "ggml-base.bin",
        title: "Самая лёгкая — base",
        megabytes: 148,
    },
];

/// Порт своего сервера расшифровки. Не ходовой номер: занятый порт означал бы,
/// что мы стучимся в чужую программу и шлём ей записи с микрофона.
const PORT: u16 = 8643;

/// Сервер расшифровки Суфлёра. Если он уже работает, у него в памяти та же
/// модель large-v3-turbo, и поднимать рядом вторую копию — это ещё полтора
/// гигабайта видеопамяти. Когда памяти впритык, обе копии начинают делить её
/// с оперативной, и фраза расшифровывается не за доли секунды, а за двадцать.
const SHARED_PORT: u16 = 8642;

/// Куда сейчас идут запросы: на свой сервер или на сервер Суфлёра.
static ACTIVE_PORT: AtomicU16 = AtomicU16::new(PORT);

/// Когда общий сервер последний раз подвёл, секунд от запуска. Минуту после
/// этого им не пользуемся: Суфлёр, скорее всего, как раз его выгрузил.
static SHARED_FAILED_AT: AtomicU64 = AtomicU64::new(0);

/// Столько видеопамяти нужно большой модели с запасом. Если свободно меньше,
/// сервер на видеокарте не падает, а переполняет её, и загрузка тянется
/// минутами. На процессоре медленнее, но предсказуемо.
const MIN_FREE_VRAM_GB: f64 = 2.3;

/// Видеопамять NVIDIA: всего и свободно, в гигабайтах. `None` — карты нет.
pub fn vram() -> Option<(f64, f64)> {
    let mut command = std::process::Command::new("nvidia-smi");
    command
        .args(["--query-gpu=memory.total,memory.free", "--format=csv,noheader,nounits"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    hide_window(&mut command);
    let output = command.output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let mut numbers = text.lines().next()?.split(',').map(|n| n.trim().parse::<f64>());
    let total = numbers.next()?.ok()?;
    let free = numbers.next()?.ok()?;
    Some((total / 1024.0, free / 1024.0))
}

fn hide_window(command: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

/// Есть ли видеокарта, на которой стоит считать.
pub fn has_gpu() -> bool {
    // Один раз на запуск: видеокарта за время работы не меняется, а вопрос
    // этот задаётся при каждом открытии окна.
    static GPU: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *GPU.get_or_init(|| vram().is_some_and(|(total, _)| total >= 3.5))
}

/// Какая модель на самом деле будет работать при таком выборе в настройках.
pub fn resolve(choice: &str) -> &'static Model {
    if let Some(model) = MODELS.iter().find(|m| m.id == choice) {
        return model;
    }
    // «Авто»: с видеокартой — точная, без неё — та, что успевает на процессоре.
    let id = if has_gpu() { "turbo" } else { "small" };
    MODELS.iter().find(|m| m.id == id).unwrap_or(&MODELS[0])
}

pub fn root(app: &AppHandle) -> Result<std::path::PathBuf, String> {
    app.path()
        .app_data_dir()
        .map(|dir| dir.join("whisper"))
        .map_err(|err| format!("не удалось определить каталог данных: {err}"))
}

/// Сервер расшифровки. Часть сборок кладёт всё в подпапку вроде `Release/`.
fn server_exe(app: &AppHandle) -> Option<std::path::PathBuf> {
    let root = root(app).ok()?;
    let direct = root.join("whisper-server.exe");
    if direct.exists() {
        return Some(direct);
    }
    std::fs::read_dir(&root).ok()?.flatten().find_map(|entry| {
        let candidate = entry.path().join("whisper-server.exe");
        candidate.exists().then_some(candidate)
    })
}

fn model_path(app: &AppHandle, model: &Model) -> Result<std::path::PathBuf, String> {
    Ok(root(app)?.join(model.file))
}

/// Скачано ли всё нужное для выбранной модели.
pub fn installed(app: &AppHandle, choice: &str) -> bool {
    server_exe(app).is_some()
        && model_path(app, resolve(choice)).map(|p| p.exists()).unwrap_or(false)
}

static INSTALLING: AtomicBool = AtomicBool::new(false);

pub fn installing() -> bool {
    INSTALLING.load(Ordering::SeqCst)
}

/// Скачивает сервер расшифровки и модель.
pub async fn install(app: AppHandle, choice: String) -> Result<(), String> {
    if INSTALLING.swap(true, Ordering::SeqCst) {
        return Err("загрузка уже идёт".into());
    }
    let result = install_inner(&app, &choice).await;
    INSTALLING.store(false, Ordering::SeqCst);
    match &result {
        Ok(()) => emit(&app, 100, "готово", true, None),
        Err(err) => emit(&app, 0, "не получилось", true, Some(err.clone())),
    }
    result
}

async fn install_inner(app: &AppHandle, choice: &str) -> Result<(), String> {
    let root = root(app)?;
    std::fs::create_dir_all(&root).map_err(|err| err.to_string())?;

    if server_exe(app).is_none() {
        let name = if has_gpu() { CUDA_ZIP } else { CPU_ZIP };
        log::info!("беру сборку whisper: {name}");

        // Под своим именем: недокачанный архив одной сборки нельзя продолжать
        // архивом другой.
        let archive = root.join(name);
        emit(app, 0, "скачиваю распознавание", false, None);
        download(&format!("{RELEASE}/{name}"), &archive, &progress(app, 0, 30)).await?;

        emit(app, 30, "распаковываю", false, None);
        unzip(&archive, &root)?;
        let _ = std::fs::remove_file(&archive);

        if server_exe(app).is_none() {
            return Err("в архиве whisper не нашлось сервера расшифровки".into());
        }
    }

    let model = resolve(choice);
    let path = model_path(app, model)?;
    if !path.exists() {
        emit(app, 32, "скачиваю модель речи", false, None);
        download(&format!("{MODELS_URL}/{}", model.file), &path, &progress(app, 32, 99)).await?;
    }
    Ok(())
}

/// Качает файл с докачкой.
///
/// Модель весит до полутора гигабайт, и на плохом канале соединение рвётся
/// не раз. Начинать каждый раз с нуля значило бы не скачать её никогда,
/// поэтому недокачанное остаётся рядом и следующая попытка продолжает с места
/// обрыва.
///
/// `report` получает, сколько байт уже есть и сколько всего.
async fn download(
    url: &str,
    to: &std::path::Path,
    report: &(dyn Fn(u64, u64) + Sync),
) -> Result<(), String> {
    use std::io::Write;

    const ATTEMPTS: u32 = 40;

    let client = reqwest::Client::builder()
        .user_agent("Govor")
        .connect_timeout(std::time::Duration::from_secs(20))
        // Не общий срок на файл, а срок молчания: большой файл качается долго,
        // но замершее соединение надо бросить и начать новое.
        .read_timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|err| err.to_string())?;

    let temp = to.with_extension("part");
    let mut last_error = String::new();
    let mut complete = false;

    for attempt in 0..ATTEMPTS {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
        let have = std::fs::metadata(&temp).map(|m| m.len()).unwrap_or(0);

        let mut request = client.get(url);
        if have > 0 {
            request = request.header("Range", format!("bytes={have}-"));
        }
        let mut response = match request.send().await {
            Ok(response) => response,
            Err(err) => {
                last_error = format!("загрузка не началась: {err}");
                continue;
            }
        };

        let status = response.status();
        // 416 — просили продолжить с конца файла: он уже скачан целиком.
        if status.as_u16() == 416 && have > 0 {
            complete = true;
            break;
        }
        if !status.is_success() {
            last_error = format!("сервер ответил {status}");
            continue;
        }

        // Сервер мог не понять просьбу о продолжении и отдать файл с начала.
        let resumed = status.as_u16() == 206;
        let offset = if resumed { have } else { 0 };
        let total = response.content_length().map(|rest| rest + offset).unwrap_or(0);

        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .append(resumed)
            .truncate(!resumed)
            .open(&temp)
            .map_err(|err| err.to_string())?;

        let mut written = offset;
        let mut broken = false;
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    file.write_all(&chunk).map_err(|err| err.to_string())?;
                    written += chunk.len() as u64;
                    if total > 0 {
                        report(written, total);
                    }
                }
                Ok(None) => break,
                Err(err) => {
                    last_error = format!("загрузка прервалась: {err}");
                    broken = true;
                    break;
                }
            }
        }
        drop(file);

        if !broken && (total == 0 || written >= total) {
            return std::fs::rename(&temp, to).map_err(|err| err.to_string());
        }
        log::warn!("загрузка оборвалась на {written} из {total} — продолжаю");
    }

    // Вышли по 416: файл целый.
    if complete && temp.exists() {
        return std::fs::rename(&temp, to).map_err(|err| err.to_string());
    }
    Err(if last_error.is_empty() { "файл не скачался".into() } else { last_error })
}

/// Переводит байты одного файла в отрезок общей полосы загрузки и сообщает
/// окну — не чаще, чем меняется процент.
fn progress(app: &AppHandle, from: u8, to: u8) -> impl Fn(u64, u64) + Sync + '_ {
    let last = std::sync::atomic::AtomicU8::new(u8::MAX);
    move |written, total| {
        let span = f64::from(to.saturating_sub(from));
        let percent = from + ((written as f64 / total.max(1) as f64) * span) as u8;
        if last.swap(percent, Ordering::Relaxed) != percent {
            let status = format!("скачиваю: {} из {} МБ", written / 1_048_576, total / 1_048_576);
            emit(app, percent, &status, false, None);
        }
    }
}

/// Распаковка архива. Имена внутри проверяем: путь с двумя точками в архиве
/// означает запись куда угодно на диске.
fn unzip(archive: &std::path::Path, into: &std::path::Path) -> Result<(), String> {
    let file = std::fs::File::open(archive).map_err(|err| err.to_string())?;
    let mut zip = zip::ZipArchive::new(file).map_err(|err| format!("архив не читается: {err}"))?;

    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).map_err(|err| err.to_string())?;
        let Some(name) = entry.enclosed_name() else {
            return Err("в архиве есть запись с недопустимым путём".into());
        };
        let target = into.join(name);

        if entry.is_dir() {
            std::fs::create_dir_all(&target).map_err(|err| err.to_string())?;
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|err| err.to_string())?;
        }
        let mut out = std::fs::File::create(&target).map_err(|err| err.to_string())?;
        std::io::copy(&mut entry, &mut out).map_err(|err| err.to_string())?;
    }
    Ok(())
}

/* ── Сервер ───────────────────────────────────────────────────────────────── */

static STARTING: AtomicBool = AtomicBool::new(false);
/// Прежний сервер ещё снимается: порт в это время открыт, а сервера уже нет.
static STOPPING: AtomicBool = AtomicBool::new(false);
/// Сервер считает на процессоре — текст «на лету» тогда не показываем.
static ON_CPU: AtomicBool = AtomicBool::new(false);
static SPAWNING: Mutex<()> = Mutex::new(());
/// Запущенный сервер и модель, с которой он поднят.
static SERVER: Mutex<Option<(std::process::Child, &'static str)>> = Mutex::new(None);
/// Когда сервером пользовались последний раз, секунд от запуска программы.
static LAST_USED: AtomicU64 = AtomicU64::new(0);

fn now_secs() -> u64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START.get_or_init(std::time::Instant::now).elapsed().as_secs()
}

fn touch() {
    LAST_USED.store(now_secs(), Ordering::Relaxed);
}

/// Останавливает сервер и освобождает память.
///
/// В отдельном потоке: зовут это и из окна настроек, а процесс с полутора
/// гигабайтами в видеопамяти уходит не мгновенно — окно всё это время висело бы.
pub fn shutdown() {
    let child = SERVER.lock().unwrap_or_else(|err| err.into_inner()).take();
    let Some((mut child, _)) = child else { return };
    STOPPING.store(true, Ordering::SeqCst);

    let started = std::thread::Builder::new()
        .name("govor-whisper-stop".into())
        .spawn(move || {
            let _ = child.kill();
            let _ = child.wait();
            STOPPING.store(false, Ordering::SeqCst);
            log::info!("сервер расшифровки остановлен, память свободна");
        });
    if started.is_err() {
        STOPPING.store(false, Ordering::SeqCst);
    }
}

/// Следит за простоем: модель, которой не пользуются, память не занимает.
pub fn watch_idle(app: AppHandle) {
    std::thread::Builder::new()
        .name("govor-whisper-idle".into())
        .spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_secs(20));
            let minutes = crate::config_of(&app).unload_minutes;
            if minutes == 0 {
                continue;
            }
            let running = SERVER.lock().unwrap_or_else(|err| err.into_inner()).is_some();
            let idle = now_secs().saturating_sub(LAST_USED.load(Ordering::Relaxed));
            if running && idle >= u64::from(minutes) * 60 && !crate::recording() {
                log::info!("распознаванием не пользовались {minutes} мин — выгружаю модель");
                shutdown();
            }
        })
        .ok();
}

/// Поднимает сервер заранее — пока человек ещё говорит.
pub fn warm(app: &AppHandle) {
    touch();
    if STARTING.swap(true, Ordering::SeqCst) {
        return;
    }
    let app = app.clone();
    std::thread::Builder::new()
        .name("govor-whisper-warm".into())
        .spawn(move || {
            if let Err(err) = ensure_server(&app) {
                log::warn!("сервер расшифровки не поднялся заранее: {err}");
            }
            STARTING.store(false, Ordering::SeqCst);
        })
        .ok();
}

/// Готов ли сервер прямо сейчас — без ожидания и без запуска.
pub fn ready_now() -> bool {
    if ACTIVE_PORT.load(Ordering::Relaxed) == SHARED_PORT {
        return port_open(SHARED_PORT);
    }
    !STOPPING.load(Ordering::SeqCst) && server_alive()
}

pub fn on_cpu() -> bool {
    ON_CPU.load(Ordering::Relaxed)
}

fn server_alive() -> bool {
    port_open(PORT)
}

fn port_open(port: u16) -> bool {
    std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        std::time::Duration::from_millis(300),
    )
    .is_ok()
}

/// Работает ли на порту Суфлёра именно сервер whisper.cpp — по его странице.
fn shared_alive() -> bool {
    use std::io::{Read, Write};

    let failed = SHARED_FAILED_AT.load(Ordering::Relaxed);
    if failed != 0 && now_secs().saturating_sub(failed) < 60 {
        return false;
    }
    let Ok(mut stream) = std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], SHARED_PORT)),
        std::time::Duration::from_millis(200),
    ) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(700)));
    if stream.write_all(b"GET / HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n").is_err() {
        return false;
    }
    let mut page = vec![0u8; 2048];
    let mut got = 0;
    while got < page.len() {
        match stream.read(&mut page[got..]) {
            Ok(0) | Err(_) => break,
            Ok(read) => got += read,
        }
    }
    String::from_utf8_lossy(&page[..got]).contains("Whisper.cpp Server")
}

/// Готовит сервер и отвечает, на каком порту он слушает.
fn ensure_server(app: &AppHandle) -> Result<u16, String> {
    let model = resolve(&crate::config_of(app).model);
    if model.id == "turbo" && shared_alive() {
        if SERVER.lock().unwrap_or_else(|err| err.into_inner()).is_some() {
            shutdown();
        }
        if ACTIVE_PORT.swap(SHARED_PORT, Ordering::SeqCst) != SHARED_PORT {
            log::info!("пользуюсь сервером расшифровки Суфлёра — модель та же, вторая копия не нужна");
        }
        ON_CPU.store(false, Ordering::Relaxed);
        touch();
        return Ok(SHARED_PORT);
    }
    ACTIVE_PORT.store(PORT, Ordering::SeqCst);
    ensure_own(app).map(|()| PORT)
}

fn ensure_own(app: &AppHandle) -> Result<(), String> {
    let begun = std::time::Instant::now();
    let model = resolve(&crate::config_of(app).model);

    // Прежний сервер могли только что попросить уйти. Пока он уходит, порт
    // открыт, и проверка «жив ли» соврала бы.
    for _ in 0..100 {
        if !STOPPING.load(Ordering::SeqCst) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    let same_model = || {
        SERVER
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .as_ref()
            .is_some_and(|(_, id)| *id == model.id)
    };
    // Сначала учёт, потом сеть: когда сервера у нас нет, стучаться в порт
    // незачем — каждая такая проверка стоит треть секунды до нажатия клавиши.
    if same_model() && server_alive() {
        return Ok(());
    }

    // Поднимать сервер имеет право только один поток за раз: прогрев и
    // расшифровка легко случаются одновременно, и оба загрузили бы модель.
    let _spawning = SPAWNING.lock().unwrap_or_else(|err| err.into_inner());
    // Сначала учёт, потом сеть: когда сервера у нас нет, стучаться в порт
    // незачем — каждая такая проверка стоит треть секунды до нажатия клавиши.
    if same_model() && server_alive() {
        return Ok(());
    }

    // Что бы ни осталось в учёте — оно мертво или поднято с другой моделью.
    let stale = SERVER.lock().unwrap_or_else(|err| err.into_inner()).take();
    if let Some((mut stale, _)) = stale {
        let _ = stale.kill();
        let _ = stale.wait();
    }
    if server_alive() {
        // Порт держит кто-то чужой — возможно, вторая копия программы.
        return Err(format!("порт {PORT} занят другой программой"));
    }

    let exe = server_exe(app).ok_or("распознавание речи ещё не скачано")?;
    let path = model_path(app, model)?;
    if !path.exists() {
        return Err("модель распознавания ещё не скачана".into());
    }

    let on_cpu = !has_gpu() || vram().is_none_or(|(_, free)| free < MIN_FREE_VRAM_GB);
    ON_CPU.store(on_cpu, Ordering::Relaxed);

    // Вывод сервера — в файл: иначе причина, по которой он не поднялся, теряется.
    let log_path = root(app)?.join("server.log");
    let log_file = std::fs::File::create(&log_path)
        .map_err(|err| format!("журнал сервера расшифровки не создался: {err}"))?;

    let mut command = std::process::Command::new(&exe);
    command
        .arg("-m")
        .arg(&path)
        .arg("--host")
        .arg("127.0.0.1")
        .arg("--port")
        .arg(PORT.to_string())
        // Лучевой поиск вместо жадного разбора: модель держит несколько
        // вариантов фразы и выбирает лучший. На процессоре луч уже: там каждая
        // лишняя ветка — заметное время.
        .arg("--beam-size")
        .arg(if on_cpu { "2" } else { "5" })
        .arg("--best-of")
        .arg(if on_cpu { "2" } else { "5" })
        .arg("--no-speech-thold")
        .arg("0.6")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(log_file);
    if let Some(dir) = exe.parent() {
        // Рядом с сервером лежат его библиотеки (CUDA, BLAS).
        command.current_dir(dir);
    }
    if on_cpu {
        let threads = std::thread::available_parallelism().map(|n| n.get().min(16)).unwrap_or(4);
        command.arg("--no-gpu").arg("--threads").arg(threads.to_string());
        log::info!("распознавание пойдёт на процессоре");
    }
    hide_window(&mut command);

    let child = command
        .spawn()
        .map_err(|err| format!("сервер расшифровки не запустился: {err}"))?;
    // Сервер уходит вместе с программой, как бы её ни закрыли.
    crate::jobs::adopt(&child);
    *SERVER.lock().unwrap_or_else(|err| err.into_inner()) = Some((child, model.id));
    log::info!("поднимаю сервер расшифровки, модель {} (подготовка {} мс)", model.id, begun.elapsed().as_millis());

    // Первый запуск на новой видеокарте дольше: драйвер компилирует под неё ядра.
    for _ in 0..300 {
        std::thread::sleep(std::time::Duration::from_millis(300));
        if server_alive() {
            log::info!("сервер расшифровки готов за {} мс", begun.elapsed().as_millis());
            touch();
            return Ok(());
        }
        let exited = SERVER
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .as_mut()
            .and_then(|(child, _)| child.try_wait().ok().flatten());
        if let Some(status) = exited {
            let output = std::fs::read_to_string(&log_path).unwrap_or_default();
            let tail: Vec<&str> = output.lines().filter(|l| !l.trim().is_empty()).collect();
            let tail = tail[tail.len().saturating_sub(3)..].join(" / ");
            SERVER.lock().unwrap_or_else(|err| err.into_inner()).take();
            return Err(format!("сервер расшифровки завершился ({status}): {tail}"));
        }
    }
    Err("сервер расшифровки не ответил".into())
}

/// Расшифровывает запись. На входе — готовый WAV, на выходе — текст.
///
/// `prompt` — слова из словаря человека: названное заранее перестаёт быть для
/// модели редким, и она не подменяет его похожим по звучанию.
pub async fn transcribe(
    app: &AppHandle,
    wav: Vec<u8>,
    language: &str,
    prompt: &str,
) -> Result<String, String> {
    let first = request(app, wav.clone(), language, prompt).await;
    let text = match first {
        Ok(text) => text,
        Err((port, err)) if port == SHARED_PORT => {
            // Суфлёр выгрузил свой сервер между проверкой и запросом — дальше
            // своим, у него та же модель на диске.
            log::warn!("сервер Суфлёра не ответил ({err}) — поднимаю свой");
            SHARED_FAILED_AT.store(now_secs().max(1), Ordering::Relaxed);
            request(app, wav, language, prompt).await.map_err(|(_, err)| err)?
        }
        Err((_, err)) => return Err(err),
    };
    touch();
    let text = clean(&text);
    if echoes_prompt(&text, prompt) || is_hallucination(&text) {
        let short: String = text.chars().take(60).collect();
        log::info!("расшифровка похожа на выдумку модели — пропускаю: «{short}»");
        return Ok(String::new());
    }
    Ok(text)
}

/// Один запрос к серверу расшифровки. Ошибка несёт порт, на котором случилась.
async fn request(app: &AppHandle, wav: Vec<u8>, language: &str, prompt: &str) -> Result<String, (u16, String)> {
    let handle = app.clone();
    // Запуск сервера ждёт его готовности — секунды, а то и десятки. В потоке
    // для блокирующей работы, чтобы не занимать поток асинхронных задач.
    let port = tauri::async_runtime::spawn_blocking(move || ensure_server(&handle))
        .await
        .map_err(|err| (PORT, err.to_string()))?
        .map_err(|err| (PORT, err))?;
    touch();

    let form = reqwest::multipart::Form::new()
        .text("response_format", "text")
        .text("language", language.to_string())
        .text("prompt", prompt.to_string())
        // Без перебора температур: он спасает на плохой записи, но там же
        // и выдумывает.
        .text("temperature", "0.0")
        // Имя файла обязательно: без него сервер не признаёт часть формы файлом.
        .part(
            "file",
            reqwest::multipart::Part::bytes(wav)
                .file_name("audio.wav")
                .mime_str("audio/wav")
                .map_err(|err| (port, err.to_string()))?,
        );

    let client = reqwest::Client::builder()
        // Локальный адрес: системный прокси ему только мешает.
        .no_proxy()
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .map_err(|err| (port, err.to_string()))?;

    let response = client
        .post(format!("http://127.0.0.1:{port}/inference"))
        .multipart(form)
        .send()
        .await
        .map_err(|err| (port, format!("сервер расшифровки не ответил: {err}")))?;

    if !response.status().is_success() {
        return Err((port, format!("расшифровка ответила {}", response.status())));
    }
    response
        .text()
        .await
        .map_err(|err| (port, format!("ответ расшифровки не прочитался: {err}")))
}

/// Не пересказала ли модель подсказку вместо услышанного — так она делает
/// на тишине.
fn echoes_prompt(text: &str, prompt: &str) -> bool {
    if prompt.trim().is_empty() || text.trim().is_empty() {
        return false;
    }
    let letters = |s: &str| -> String { s.to_lowercase().chars().filter(|c| c.is_alphanumeric()).collect() };
    let (said, hinted) = (letters(text), letters(prompt));
    said.chars().count() >= 20 && hinted.contains(&said)
}

/// Подписи, которые модель выдумывает на шуме: её учили на субтитрах к
/// роликам. Только то, чего в живой диктовке не бывает, — «спасибо за
/// внимание» человек может продиктовать и сам.
const HALLUCINATIONS: &[&str] = &[
    "субтитры сделал",
    "субтитры создавал",
    "субтитры подготов",
    "редактор субтитров",
    "dimatorzok",
    "amara.org",
    "subtitles by",
    "thanks for watching",
    "thank you for watching",
];

fn is_hallucination(text: &str) -> bool {
    let lower = text.to_lowercase();
    if HALLUCINATIONS.iter().any(|mark| lower.contains(mark)) {
        return true;
    }

    // Целиком в скобках — «[музыка]», «(смех)» — подпись к звуку, а не слова.
    let bare = lower.trim().trim_end_matches(['.', '!', '?', '…']);
    let wrapped = [('[', ']'), ('(', ')'), ('*', '*')]
        .iter()
        .any(|(open, close)| bare.len() > 1 && bare.starts_with(*open) && bare.ends_with(*close));
    if wrapped || lower.contains('♪') {
        return true;
    }

    // Заклинившая модель повторяет одно слово: «Пока, пока, пока, пока».
    let words: Vec<&str> = lower.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect();
    if words.len() >= 5 {
        let mut unique = words.clone();
        unique.sort_unstable();
        unique.dedup();
        if unique.len() == 1 {
            return true;
        }
    }
    false
}

/// Приводит вывод сервера к тексту: он печатает строками по фразам, а на
/// тишине выдаёт пометки вроде `[BLANK_AUDIO]`.
pub(crate) fn clean(raw: &str) -> String {
    raw.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| !(line.starts_with('[') && line.ends_with(']')))
        .filter(|line| !(line.starts_with('(') && line.ends_with(')')))
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string()
}

fn emit(app: &AppHandle, percent: u8, status: &str, done: bool, error: Option<String>) {
    let _ = app.emit(
        EVENT,
        serde_json::json!({
            "percent": percent,
            "status": status,
            "done": done,
            "error": error,
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_marks_do_not_become_text() {
        assert_eq!(clean("[BLANK_AUDIO]"), "");
        assert_eq!(clean("  Привет.\n\n [BLANK_AUDIO] \n"), "Привет.");
        assert_eq!(clean("Первая фраза.\nВторая фраза."), "Первая фраза. Вторая фраза.");
    }

    #[test]
    fn invented_subtitles_are_dropped() {
        assert!(is_hallucination("Субтитры сделал DimaTorzok"));
        assert!(is_hallucination("Редактор субтитров А.Синецкая"));
        assert!(is_hallucination("[музыка]"));
        assert!(is_hallucination("Пока, пока, пока, пока, пока."));
    }

    #[test]
    fn real_dictation_survives() {
        assert!(!is_hallucination("Да."));
        assert!(!is_hallucination("Спасибо за внимание, жду ответа."));
        assert!(!is_hallucination("Открой текстовый редактор"));
        assert!(!is_hallucination("да, да, конечно"));
    }

    #[test]
    fn the_hint_does_not_come_back_as_speech() {
        let hint = "Kubernetes, Говор, Tauri, whisper.cpp, PostgreSQL";
        assert!(echoes_prompt("Kubernetes Говор Tauri whisper.cpp", hint));
        assert!(!echoes_prompt("Поставь Kubernetes на сервер", hint));
    }

    /// Настоящая загрузка из сети — запускается отдельно:
    /// `cargo test -- --ignored download`.
    ///
    /// Качает сборку для процессора (20 МБ) дважды: целиком и с середины,
    /// будто соединение оборвалось. Файлы обязаны совпасть, а в архиве должен
    /// найтись сервер расшифровки.
    #[tokio::test]
    #[ignore]
    async fn download_resumes_where_it_stopped() {
        let dir = std::env::temp_dir().join(format!("govor-download-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let url = format!("{RELEASE}/{CPU_ZIP}");

        let whole = dir.join("whole.zip");
        download(&url, &whole, &|_, _| {}).await.unwrap();
        let bytes = std::fs::read(&whole).unwrap();
        assert!(bytes.len() > 1_000_000);

        let resumed = dir.join("resumed.zip");
        std::fs::write(resumed.with_extension("part"), &bytes[..bytes.len() / 2]).unwrap();
        let seen_from = std::sync::atomic::AtomicU64::new(u64::MAX);
        download(&url, &resumed, &|written, _| {
            seen_from.fetch_min(written, Ordering::Relaxed);
        })
        .await
        .unwrap();
        assert!(std::fs::read(&resumed).unwrap() == bytes, "докачанный файл отличается");
        assert!(
            seen_from.load(Ordering::Relaxed) > bytes.len() as u64 / 2,
            "загрузка началась с начала, а не с места обрыва"
        );

        unzip(&whole, &dir).unwrap();
        let found = dir.join("whisper-server.exe").exists()
            || std::fs::read_dir(&dir)
                .unwrap()
                .flatten()
                .any(|entry| entry.path().join("whisper-server.exe").exists());
        assert!(found, "в архиве нет whisper-server.exe");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_model_has_a_file() {
        for model in MODELS {
            assert!(model.file.ends_with(".bin"), "{}", model.id);
        }
    }
}
