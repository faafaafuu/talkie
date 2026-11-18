//! Запись с микрофона и подготовка её к распознаванию.
//!
//! Микрофон открыт только пока идёт запись: значок «микрофон используется»
//! в трее Windows не должен гореть у программы, которая просто ждёт клавишу.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

/// Частота, на которой работает whisper.
const WHISPER_RATE: u32 = 16_000;

/// Самая длинная запись. Дольше десяти минут подряд не диктуют, а забытая
/// включённой запись без рук не должна съесть всю память.
const MAX_SECONDS: usize = 600;

/// Ниже этого уровня в записи нет ничего, кроме шума. На пустой записи whisper
/// не молчит, а выдумывает — титры из роликов, на которых его учили.
const SILENCE_LEVEL: f32 = 0.012;

/// До какого пика подтягивать тихую запись и во сколько раз самое большее:
/// дальше усиливается уже шум вентилятора, а не голос.
const TARGET_PEAK: f32 = 0.85;
const MAX_GAIN: f32 = 8.0;

/// Whisper заканчивает последний отрезок по границе окна и охотно теряет
/// конец фразы, если запись обрывается сразу за последним словом.
const SILENCE_TAIL_MS: u64 = 400;

enum Command {
    Start(String, Sender<bool>),
    Stop(Sender<Option<(Vec<f32>, u32)>>),
    Snapshot(usize, Sender<Option<(Vec<f32>, u32)>>),
    Cancel,
}

static COMMANDS: OnceLock<Sender<Command>> = OnceLock::new();

/// Громкость последнего куска записи, 0..1 — для полосок индикатора.
static LEVEL: AtomicU32 = AtomicU32::new(0);

/// Сколько порций звука пришло с начала записи. Ноль через секунду после
/// открытия — микрофон открылся, но звука не отдаёт.
static RECEIVED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Когда микрофон последний раз не открылся.
///
/// Сразу после отказа пробовать снова незачем: устройство, которое Windows
/// не может открыть, не оживёт от повторов, а самодельные USB-микрофоны от
/// частых открытий и закрытий только сильнее зависают.
static FAILED_AT: Mutex<Option<std::time::Instant>> = Mutex::new(None);

/// Сколько не открывать микрофон после отказа.
const RETRY_AFTER: std::time::Duration = std::time::Duration::from_secs(5);

/// Сколько порций звука пришло с начала записи. При подмене файлом — сколько угодно.
pub fn received() -> usize {
    if std::env::var_os(MIC_FILE).is_some() {
        return usize::MAX;
    }
    RECEIVED.load(Ordering::Relaxed)
}

pub fn level() -> f32 {
    f32::from_bits(LEVEL.load(Ordering::Relaxed))
}

fn commands() -> &'static Sender<Command> {
    COMMANDS.get_or_init(|| {
        let (tx, rx) = channel::<Command>();
        // Поток записи свой и один: поток cpal нельзя передавать между
        // потоками, поэтому открывает, держит и закрывает его один и тот же.
        std::thread::Builder::new()
            .name("govor-mic".into())
            .spawn(move || mic_loop(rx))
            .ok();
        tx
    })
}

fn mic_loop(rx: Receiver<Command>) {
    let mut active: Option<(cpal::Stream, Arc<Mutex<Vec<f32>>>, u32)> = None;

    for command in rx {
        match command {
            Command::Start(device, reply) => {
                // Прежний поток, если остался, закрывается до открытия нового.
                drop(active.take());
                LEVEL.store(0, Ordering::Relaxed);
                let started = std::time::Instant::now();
                active = open_input(&device);
                if let Some((stream, _, _)) = &active {
                    if let Err(err) = stream.play() {
                        log::warn!("микрофон не запустился: {err}");
                        active = None;
                    }
                }
                log::info!(
                    "микрофон {} за {} мс",
                    if active.is_some() { "открыт" } else { "не открылся" },
                    started.elapsed().as_millis()
                );
                let _ = reply.send(active.is_some());
            }
            Command::Stop(reply) => {
                let result = active.take().map(|(stream, buffer, rate)| {
                    drop(stream);
                    let samples = std::mem::take(&mut *buffer.lock().unwrap_or_else(|e| e.into_inner()));
                    (samples, rate)
                });
                LEVEL.store(0, Ordering::Relaxed);
                let _ = reply.send(result);
            }
            Command::Snapshot(seconds, reply) => {
                let result = active.as_ref().map(|(_, buffer, rate)| {
                    let samples = buffer.lock().unwrap_or_else(|e| e.into_inner());
                    let keep = (*rate as usize).saturating_mul(seconds).min(samples.len());
                    (samples[samples.len() - keep..].to_vec(), *rate)
                });
                let _ = reply.send(result);
            }
            Command::Cancel => {
                active = None;
                LEVEL.store(0, Ordering::Relaxed);
            }
        }
    }
}

/// Какие устройства записи видит система.
pub fn devices() -> Vec<String> {
    cpal::default_host()
        .input_devices()
        .map(|list| list.filter_map(|d| d.name().ok()).collect())
        .unwrap_or_default()
}

/// Открывает устройство ввода и начинает складывать отсчёты в общий буфер.
fn open_input(preferred: &str) -> Option<(cpal::Stream, Arc<Mutex<Vec<f32>>>, u32)> {
    let host = cpal::default_host();

    let device = if preferred.trim().is_empty() {
        host.default_input_device()?
    } else {
        // Названное устройство, а если его больше нет (отключили наушники) —
        // основное: молчать из-за пропавшего микрофона хуже, чем писать не с того.
        host.input_devices()
            .ok()?
            .find(|d| d.name().map(|name| name == preferred).unwrap_or(false))
            .or_else(|| host.default_input_device())?
    };

    let name = device.name().unwrap_or_else(|_| "без имени".into());
    let config = device.default_input_config().ok()?;
    let rate = config.sample_rate().0;
    let channels = config.channels() as usize;
    let format = config.sample_format();
    log::info!("пишу с устройства «{name}»: {rate} Гц, каналов {channels}, {format:?}");

    let limit = rate as usize * MAX_SECONDS;
    let buffer = Arc::new(Mutex::new(Vec::<f32>::with_capacity(rate as usize * 10)));

    // Общая часть для любого формата отсчётов: свести в моно, сложить в буфер
    // и запомнить громкость куска.
    let sink = buffer.clone();
    let push = move |frames: &mut dyn Iterator<Item = f32>| {
        let mut samples = sink.lock().unwrap_or_else(|err| err.into_inner());
        let mut peak = 0.0_f32;
        let mut frame = 0.0_f32;
        let mut filled = 0usize;
        for value in frames {
            frame += value;
            filled += 1;
            if filled == channels.max(1) {
                let mono = frame / filled as f32;
                peak = peak.max(mono.abs());
                if samples.len() < limit {
                    samples.push(mono);
                }
                frame = 0.0;
                filled = 0;
            }
        }
        LEVEL.store(peak.min(1.0).to_bits(), Ordering::Relaxed);
        RECEIVED.fetch_add(1, Ordering::Relaxed);
    };

    let on_error = |err| log::warn!("сбой записи: {err}");
    let stream_config: cpal::StreamConfig = config.into();

    let stream = match format {
        cpal::SampleFormat::F32 => device.build_input_stream(
            &stream_config,
            move |data: &[f32], _: &cpal::InputCallbackInfo| push(&mut data.iter().copied()),
            on_error,
            None,
        ),
        cpal::SampleFormat::I16 => device.build_input_stream(
            &stream_config,
            move |data: &[i16], _: &cpal::InputCallbackInfo| {
                push(&mut data.iter().map(|s| f32::from(*s) / 32_768.0))
            },
            on_error,
            None,
        ),
        cpal::SampleFormat::U16 => device.build_input_stream(
            &stream_config,
            move |data: &[u16], _: &cpal::InputCallbackInfo| {
                push(&mut data.iter().map(|s| (f32::from(*s) - 32_768.0) / 32_768.0))
            },
            on_error,
            None,
        ),
        other => {
            log::warn!("формат отсчётов {other:?} не поддержан");
            return None;
        }
    }
    .map_err(|err| log::warn!("поток записи не создался: {err}"))
    .ok()?;

    Some((stream, buffer, rate))
}

/// Начать запись с названного устройства (пусто — с основного).
pub fn start(device: &str) -> bool {
    if std::env::var_os(MIC_FILE).is_none() {
        let failed = *FAILED_AT.lock().unwrap_or_else(|e| e.into_inner());
        if failed.is_some_and(|at| at.elapsed() < RETRY_AFTER) {
            log::info!("микрофон только что не открылся — повторю не раньше чем через {} с", RETRY_AFTER.as_secs());
            return false;
        }
    }
    RECEIVED.store(0, Ordering::Relaxed);
    let (reply, answer) = channel();
    if commands().send(Command::Start(device.to_string(), reply)).is_err() {
        return false;
    }
    let opened = answer.recv_timeout(std::time::Duration::from_secs(5)).unwrap_or(false);
    *FAILED_AT.lock().unwrap_or_else(|e| e.into_inner()) = (!opened).then(std::time::Instant::now);
    if std::env::var_os(MIC_FILE).is_some() {
        *STAND_IN_FROM.lock().unwrap_or_else(|e| e.into_inner()) = Some(std::time::Instant::now());
        return true;
    }
    opened
}

/// Закончить запись и отдать её: отсчёты и частоту устройства.
pub fn stop() -> Option<(Vec<f32>, u32)> {
    let (reply, answer) = channel();
    commands().send(Command::Stop(reply)).ok()?;
    let recorded = answer.recv_timeout(std::time::Duration::from_secs(5)).ok()?;
    stand_in().or(recorded)
}

/// Переменная окружения с путём к WAV, который подменяет запись с микрофона.
///
/// Так программу проверяют целиком — от клавиши до вставленного текста — без
/// человека у микрофона: запись «получается» всегда одна и та же.
const MIC_FILE: &str = "GOVOR_MIC_FILE";

/// Когда началась подменённая запись: черновики видят из файла столько,
/// сколько человек «успел бы сказать», а конец записи — файл целиком.
static STAND_IN_FROM: Mutex<Option<std::time::Instant>> = Mutex::new(None);

fn stand_in_so_far() -> Option<(Vec<f32>, u32)> {
    let (samples, rate) = stand_in()?;
    let since = (*STAND_IN_FROM.lock().unwrap_or_else(|e| e.into_inner()))?;
    let keep = ((since.elapsed().as_secs_f64() * f64::from(rate)) as usize).min(samples.len());
    Some((samples[..keep].to_vec(), rate))
}

fn stand_in() -> Option<(Vec<f32>, u32)> {
    let path = std::env::var_os(MIC_FILE)?;
    let bytes = std::fs::read(path).ok()?;
    parse_wav(&bytes)
}

/// Разбирает WAV: 16 бит, любое число каналов — сводится в моно.
pub(crate) fn parse_wav(bytes: &[u8]) -> Option<(Vec<f32>, u32)> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return None;
    }
    let (mut at, mut rate, mut channels, mut bits) = (12usize, 0u32, 1usize, 16u16);
    while at + 8 <= bytes.len() {
        let id = &bytes[at..at + 4];
        let size = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().ok()?) as usize;
        let body = &bytes[at + 8..(at + 8 + size).min(bytes.len())];
        if id == b"fmt " && body.len() >= 16 {
            channels = usize::from(u16::from_le_bytes([body[2], body[3]])).max(1);
            rate = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
            bits = u16::from_le_bytes([body[14], body[15]]);
        } else if id == b"data" {
            if bits != 16 || rate == 0 {
                return None;
            }
            let samples = body
                .chunks_exact(2 * channels)
                .map(|frame| {
                    let sum: f32 = frame
                        .chunks_exact(2)
                        .map(|s| f32::from(i16::from_le_bytes([s[0], s[1]])) / 32_768.0)
                        .sum();
                    sum / channels as f32
                })
                .collect();
            return Some((samples, rate));
        }
        // Блоки выровнены по чётной границе.
        at += 8 + size + (size & 1);
    }
    None
}

/// Копия последних секунд идущей записи — для текста «на лету».
pub fn snapshot(seconds: usize) -> Option<(Vec<f32>, u32)> {
    if let Some((samples, rate)) = stand_in_so_far() {
        let keep = (rate as usize).saturating_mul(seconds).min(samples.len());
        return Some((samples[samples.len() - keep..].to_vec(), rate));
    }
    let (reply, answer) = channel();
    commands().send(Command::Snapshot(seconds, reply)).ok()?;
    answer.recv_timeout(std::time::Duration::from_secs(2)).ok()?
}

/// Бросить запись, ничего не расшифровывая.
pub fn cancel() {
    let _ = commands().send(Command::Cancel);
}

/// Приводит запись к виду, в котором её лучше всего разбирает whisper:
/// 16 кГц, моно, выровненная громкость. `None` — в записи тишина.
pub fn prepare(samples: &[f32], rate: u32) -> Option<Vec<u8>> {
    let peak = samples.iter().fold(0.0_f32, |max, s| max.max(s.abs()));
    if peak < SILENCE_LEVEL {
        return None;
    }

    let mut samples = resample(samples, rate, WHISPER_RATE);

    // Только вверх и только тихие записи: громкая уже на пределе.
    if peak < TARGET_PEAK {
        let gain = (TARGET_PEAK / peak).min(MAX_GAIN);
        for sample in &mut samples {
            *sample *= gain;
        }
    }

    samples.resize(
        samples.len() + (WHISPER_RATE as u64 * SILENCE_TAIL_MS / 1000) as usize,
        0.0,
    );
    Some(wav(&samples, WHISPER_RATE))
}

/// Приведение к нужной частоте.
///
/// При понижении частоты сначала усредняем соседние отсчёты: без этого всё,
/// что выше новой половины частоты, отражается вниз шумом — на записи 48 кГц
/// шипящие превращаются в помеху, и распознавание путает «с» и «ш».
fn resample(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || from == 0 || to == 0 || input.is_empty() {
        return input.to_vec();
    }
    let ratio = f64::from(from) / f64::from(to);
    let len = (input.len() as f64 / ratio).floor() as usize;
    let mut out = Vec::with_capacity(len);

    if ratio > 1.0 {
        let window = ratio.ceil() as usize;
        for i in 0..len {
            let start = (i as f64 * ratio) as usize;
            let end = (start + window).min(input.len());
            let sum: f32 = input[start..end].iter().sum();
            out.push(sum / (end - start).max(1) as f32);
        }
        return out;
    }

    for i in 0..len {
        let position = i as f64 * ratio;
        let left = position.floor() as usize;
        let right = (left + 1).min(input.len() - 1);
        let weight = (position - left as f64) as f32;
        out.push(input[left] * (1.0 - weight) + input[right] * weight);
    }
    out
}

/// Собирает WAV: 16 бит, моно.
fn wav(samples: &[f32], rate: u32) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + data_len as usize);

    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());

    for sample in samples {
        let value = (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
        out.extend_from_slice(&value.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_is_not_sent_to_recognition() {
        assert!(prepare(&vec![0.001; 16_000], 16_000).is_none());
    }

    #[test]
    fn quiet_speech_is_brought_up() {
        let wav = prepare(&vec![0.1; 16_000], 16_000).unwrap();
        let first = i16::from_le_bytes([wav[44], wav[45]]);
        assert!(first > 20_000, "получилось {first}");
    }

    #[test]
    fn resampling_keeps_the_duration() {
        let out = resample(&vec![0.5; 48_000], 48_000, 16_000);
        assert_eq!(out.len(), 16_000);
        assert!((out[100] - 0.5).abs() < 1e-6);
    }

    #[test]
    fn a_written_wav_reads_back() {
        let (samples, rate) = parse_wav(&wav(&[0.5, -0.5, 0.25], 22_050)).unwrap();
        assert_eq!(rate, 22_050);
        assert_eq!(samples.len(), 3);
        assert!((samples[0] - 0.5).abs() < 0.001);
        assert!(parse_wav(b"not a wav").is_none());
    }

    #[test]
    fn wav_header_matches_the_data() {
        let bytes = wav(&[0.0; 10], 16_000);
        assert_eq!(bytes.len(), 44 + 20);
        assert_eq!(&bytes[0..4], b"RIFF");
    }
}
