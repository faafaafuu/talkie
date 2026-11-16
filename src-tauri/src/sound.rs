//! Сигналы начала и конца записи.
//!
//! Звуки не лежат файлами, а считаются при первом обращении: два коротких тона
//! с плавным началом и концом. Так у программы нет ни одного звукового файла,
//! который надо было бы где-то взять.

use std::sync::OnceLock;

const RATE: u32 = 44_100;

#[derive(Clone, Copy)]
pub enum Sound {
    Start,
    Stop,
    Cancel,
}

/// Собирает WAV из тонов: частота, длительность в миллисекундах.
fn tones(notes: &[(f32, u32)], volume: f32) -> Vec<u8> {
    let mut samples: Vec<i16> = Vec::new();
    for (frequency, ms) in notes {
        let count = (RATE * ms / 1000) as usize;
        for i in 0..count {
            let t = i as f32 / RATE as f32;
            // Плавный вход и выход: без них тон щёлкает на краях.
            let edge = (count / 6).max(1) as f32;
            let envelope = (i as f32 / edge).min(1.0).min((count - i) as f32 / edge);
            let value = (t * frequency * std::f32::consts::TAU).sin() * envelope * volume;
            samples.push((value * f32::from(i16::MAX)) as i16);
        }
    }

    let data_len = (samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&RATE.to_le_bytes());
    out.extend_from_slice(&(RATE * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for sample in samples {
        out.extend_from_slice(&sample.to_le_bytes());
    }
    out
}

/// Готовые звуки. Живут до конца работы: система играет их из нашей памяти
/// уже после возврата из вызова.
fn bank() -> &'static [Vec<u8>; 3] {
    static BANK: OnceLock<[Vec<u8>; 3]> = OnceLock::new();
    BANK.get_or_init(|| {
        [
            tones(&[(587.3, 55), (880.0, 75)], 0.16),
            tones(&[(880.0, 55), (587.3, 75)], 0.16),
            tones(&[(330.0, 120)], 0.14),
        ]
    })
}

pub fn play(sound: Sound) {
    use windows::core::PCWSTR;
    use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_MEMORY, SND_NODEFAULT};

    let wav = &bank()[sound as usize];
    // SAFETY: при SND_MEMORY первый аргумент — указатель на WAV в памяти, а не
    // строка; память статическая и переживает асинхронное воспроизведение.
    unsafe {
        let _ = PlaySoundW(PCWSTR(wav.as_ptr().cast()), None, SND_MEMORY | SND_ASYNC | SND_NODEFAULT);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tone_is_a_valid_wav() {
        let wav = tones(&[(440.0, 100)], 0.2);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(wav.len(), 44 + (RATE as usize / 10) * 2);
    }
}
