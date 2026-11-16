//! Настройки программы: один файл `config.json` рядом с остальными данными.

use serde::{Deserialize, Serialize};

/// Правка расшифровки языковой моделью.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default, rename_all = "camelCase")]
pub struct Polish {
    /// `off` — только правила; `ollama` — модель на этом компьютере;
    /// `openai` — любой сервис с интерфейсом OpenAI (адрес и ключ свои).
    pub mode: String,
    pub url: String,
    /// На диске лежит зашифрованным (`secret.rs`), в памяти — как есть.
    pub key: String,
    pub model: String,
    /// Пожелания человека к тексту: «на ты», «без буквы ё» и подобное.
    pub instructions: String,
}

impl Default for Polish {
    fn default() -> Self {
        Self {
            mode: "off".into(),
            url: "http://127.0.0.1:11434".into(),
            key: String::new(),
            model: String::new(),
            instructions: String::new(),
        }
    }
}

/// Строка словаря. С пустым `to` — слово, которое надо расслышать верно
/// (имя, термин): оно уходит распознаванию подсказкой. С заполненным —
/// замена: сказали `from`, в тексте окажется `to`.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct Entry {
    pub from: String,
    pub to: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default, rename_all = "camelCase")]
pub struct Config {
    /// Клавиша диктовки — одно из имён в `hotkey::PRESETS`.
    pub hotkey: String,
    /// Язык речи: `auto` или двухбуквенный код.
    pub language: String,
    /// Имя микрофона; пусто — основной в системе.
    pub microphone: String,
    /// Модель распознавания: `auto` или имя из `whisper::MODELS`.
    pub model: String,
    pub sounds: bool,
    /// Показывать ли текст в индикаторе, пока человек ещё говорит.
    pub live_preview: bool,
    /// `paste` — через буфер обмена, `type` — посимвольно.
    pub insert: String,
    /// Дописывать пробел после вставленного: следующая фраза встанет ровно.
    pub trailing_space: bool,
    /// «Новая строка» и «новый абзац» голосом.
    pub voice_commands: bool,
    pub polish: Polish,
    pub dictionary: Vec<Entry>,
    pub keep_history: bool,
    /// Через сколько минут простоя выгружать модель из памяти.
    pub unload_minutes: u32,
    pub autostart: bool,
    /// Пройдено ли первое знакомство (модель скачана, клавиша показана).
    pub onboarded: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            hotkey: "rctrl".into(),
            language: "auto".into(),
            microphone: String::new(),
            model: "auto".into(),
            sounds: true,
            live_preview: true,
            insert: "paste".into(),
            trailing_space: true,
            voice_commands: true,
            polish: Polish::default(),
            dictionary: Vec::new(),
            keep_history: true,
            unload_minutes: 5,
            autostart: false,
            onboarded: false,
        }
    }
}

fn path(dir: &std::path::Path) -> std::path::PathBuf {
    dir.join("config.json")
}

pub fn load(dir: &std::path::Path) -> Config {
    let mut config: Config = std::fs::read_to_string(path(dir))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    config.polish.key = crate::secret::reveal(&config.polish.key);
    config
}

/// Пишет через временный файл: оборванная запись не должна оставить половину
/// настроек, которую при следующем запуске прочитают как «настроек нет».
pub fn save(dir: &std::path::Path, config: &Config) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|err| err.to_string())?;
    let mut stored = config.clone();
    stored.polish.key = crate::secret::protect(&stored.polish.key);
    let text = serde_json::to_string_pretty(&stored).map_err(|err| err.to_string())?;
    let temp = dir.join("config.json.tmp");
    std::fs::write(&temp, text).map_err(|err| err.to_string())?;
    std::fs::rename(&temp, path(dir)).map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_file_without_new_fields_still_loads() {
        let config: Config = serde_json::from_str(r#"{"hotkey":"f9"}"#).unwrap();
        assert_eq!(config.hotkey, "f9");
        assert_eq!(config.language, "auto");
        assert!(config.sounds);
    }
}
