//! История диктовок и счётчики: сколько слов надиктовано и за какое время.
//!
//! Лежит файлом рядом с настройками и никуда не отправляется. Счётчики
//! отдельно от записей: очистка истории или её отключение не обнуляет их.

use serde::{Deserialize, Serialize};

/// Больше хранить незачем: это не архив, а способ вернуть недавно сказанное.
const KEEP: usize = 500;

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct Item {
    pub id: u64,
    /// Время в формате RFC 3339, местное.
    pub at: String,
    pub text: String,
    /// Расшифровка до правки — если правка что-то изменила.
    pub raw: String,
    pub app: String,
    pub seconds: f32,
    pub words: usize,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct Totals {
    pub dictations: u64,
    pub words: u64,
    pub seconds: f64,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct History {
    pub items: Vec<Item>,
    pub totals: Totals,
    next_id: u64,
}

fn path(dir: &std::path::Path) -> std::path::PathBuf {
    dir.join("history.json")
}

impl History {
    pub fn load(dir: &std::path::Path) -> Self {
        std::fs::read_to_string(path(dir))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, dir: &std::path::Path) {
        let Ok(text) = serde_json::to_string(self) else { return };
        let temp = dir.join("history.json.tmp");
        if std::fs::create_dir_all(dir).is_ok() && std::fs::write(&temp, text).is_ok() {
            let _ = std::fs::rename(&temp, path(dir));
        }
    }

    /// Учитывает диктовку. `keep` — хранить ли сам текст.
    pub fn add(&mut self, mut item: Item, keep: bool) {
        self.totals.dictations += 1;
        self.totals.words += item.words as u64;
        self.totals.seconds += f64::from(item.seconds);
        if !keep {
            return;
        }
        self.next_id += 1;
        item.id = self.next_id;
        self.items.insert(0, item);
        self.items.truncate(KEEP);
    }

    pub fn remove(&mut self, id: u64) {
        self.items.retain(|item| item.id != id);
    }

    pub fn clear(&mut self) {
        self.items.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(words: usize) -> Item {
        Item { text: "слово ".repeat(words), words, seconds: 2.0, ..Item::default() }
    }

    #[test]
    fn newest_comes_first_and_ids_do_not_repeat() {
        let mut history = History::default();
        history.add(item(1), true);
        history.add(item(2), true);
        assert_eq!(history.items[0].words, 2);
        assert_ne!(history.items[0].id, history.items[1].id);
    }

    #[test]
    fn totals_survive_clearing_and_disabled_history() {
        let mut history = History::default();
        history.add(item(3), true);
        history.add(item(4), false);
        history.clear();
        assert!(history.items.is_empty());
        assert_eq!(history.totals.words, 7);
        assert_eq!(history.totals.dictations, 2);
    }

    #[test]
    fn history_is_capped() {
        let mut history = History::default();
        for _ in 0..KEEP + 20 {
            history.add(item(1), true);
        }
        assert_eq!(history.items.len(), KEEP);
    }
}
