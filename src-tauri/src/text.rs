//! Правка расшифровки правилами — без языковой модели, мгновенно.
//!
//! Словарь замен, команды голосом («новая строка») и мелкая чистка. Всё, что
//! требует понимания смысла, — в `polish.rs`.

use crate::config::Entry;

/// Слова и фразы, которые распознаванию полезно знать заранее.
///
/// Берётся то, как слово должно выглядеть в тексте. Длина ограничена: подсказка
/// whisper — это не словарь, а «предыдущий текст», и на длинной он начинает
/// повторять её вместо услышанного.
pub fn hint(dictionary: &[Entry]) -> String {
    let mut out = String::new();
    for entry in dictionary {
        let word = if entry.to.trim().is_empty() { entry.from.trim() } else { entry.to.trim() };
        if word.is_empty() || word.contains('\n') {
            continue;
        }
        if out.chars().count() + word.chars().count() > 220 {
            break;
        }
        if !out.is_empty() {
            out.push_str(", ");
        }
        out.push_str(word);
    }
    out
}

/// Команды голосом: что сказали и что должно оказаться в тексте.
const COMMANDS: &[(&str, &str)] = &[
    ("с новой строки", "\n"),
    ("новая строка", "\n"),
    ("новый абзац", "\n\n"),
    ("с нового абзаца", "\n\n"),
    ("new line", "\n"),
    ("new paragraph", "\n\n"),
];

/// Звуки раздумья, которые иногда всё же попадают в расшифровку.
const FILLERS: &[&str] = &["э-э-э", "э-э", "эээ", "эм", "э-м", "м-м", "ммм", "uh", "um", "uhm"];

/// Применяет правила к расшифровке.
pub fn apply(raw: &str, dictionary: &[Entry], voice_commands: bool) -> String {
    let mut text = raw.trim().to_string();

    for filler in FILLERS {
        text = replace_phrase(&text, filler, "", true);
    }
    if voice_commands {
        for (said, written) in COMMANDS {
            text = replace_phrase(&text, said, written, true);
        }
    }
    for entry in dictionary {
        let (from, to) = (entry.from.trim(), entry.to.trim());
        if !from.is_empty() && !to.is_empty() {
            text = replace_phrase(&text, from, to, false);
        }
    }

    tidy(&text)
}

fn same(a: char, b: char) -> bool {
    a == b || a.to_lowercase().eq(b.to_lowercase())
}

/// Заменяет фразу целыми словами, не глядя на регистр.
///
/// `eat` — убрать заодно знаки вокруг: whisper пишет «…готово. Новая строка.
/// Дальше…», и после замены на перенос точка с запятой остались бы висеть.
fn replace_phrase(text: &str, phrase: &str, replacement: &str, eat: bool) -> String {
    let chars: Vec<char> = text.chars().collect();
    let needle: Vec<char> = phrase.chars().collect();
    if needle.is_empty() || chars.len() < needle.len() {
        return text.to_string();
    }

    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let fits = i + needle.len() <= chars.len()
            && chars[i..i + needle.len()].iter().zip(&needle).all(|(a, b)| same(*a, *b))
            && (i == 0 || !chars[i - 1].is_alphanumeric())
            && chars.get(i + needle.len()).is_none_or(|c| !c.is_alphanumeric());
        if !fits {
            out.push(chars[i]);
            i += 1;
            continue;
        }

        i += needle.len();
        if !eat {
            out.push_str(replacement);
            continue;
        }

        // Знаки и пробелы перед фразой: запятая перед «новая строка» не нужна,
        // а точка, закрывающая предыдущее предложение, остаётся.
        while out.ends_with([' ', ',']) {
            out.pop();
        }
        // И после неё — вместе с точкой, которой whisper закрыл «предложение»
        // из одной команды.
        while chars.get(i).is_some_and(|c| matches!(c, ' ' | ',' | '.' | '!' | '…')) {
            i += 1;
        }
        if replacement.is_empty() {
            if !out.is_empty() && i < chars.len() {
                out.push(' ');
            }
        } else {
            out.push_str(replacement);
        }
    }
    out
}

/// Мелкая чистка: двойные пробелы, пробелы у переносов, заглавная после
/// переноса и в начале.
fn tidy(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut capital = true;
    let mut previous_space = false;

    for ch in text.trim().chars() {
        if ch == '\n' {
            while out.ends_with(' ') {
                out.pop();
            }
            out.push('\n');
            capital = true;
            previous_space = true;
            continue;
        }
        if ch == ' ' || ch == '\t' {
            if !previous_space {
                out.push(' ');
            }
            previous_space = true;
            continue;
        }
        previous_space = false;
        if capital && ch.is_alphabetic() {
            out.extend(ch.to_uppercase());
        } else {
            out.push(ch);
        }
        if ch.is_alphanumeric() {
            capital = false;
        }
    }
    out.trim().to_string()
}

/// Начало расшифровки, которое уже не изменится, — его можно печатать, пока
/// человек ещё говорит.
///
/// Черновая расшифровка растущей записи меняется: последнее слово бывает
/// недосказанным, а модель, услышав продолжение, переписывает и соседние.
/// Устоявшимся считается то, что совпало в двух черновиках подряд, кроме
/// последнего слова каждого. Слова берутся вместе с пробелом или переносом
/// после них — так напечатанное сразу готово к продолжению.
pub fn stable_prefix(previous: &str, current: &str) -> String {
    let before: Vec<&str> = previous.split_inclusive(char::is_whitespace).collect();
    let now: Vec<&str> = current.split_inclusive(char::is_whitespace).collect();
    let mut same = 0;
    while same < before.len() && same < now.len() && before[same].trim_end() == now[same].trim_end() {
        same += 1;
    }
    let same = same.min(before.len().saturating_sub(1)).min(now.len().saturating_sub(1));
    now[..same].concat()
}

/// Сколько первых знаков у двух строк совпадает.
pub fn common_prefix(a: &str, b: &str) -> usize {
    a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count()
}

pub fn words(text: &str) -> usize {
    text.split_whitespace().filter(|w| w.chars().any(char::is_alphanumeric)).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(from: &str, to: &str) -> Entry {
        Entry { from: from.into(), to: to.into() }
    }

    #[test]
    fn dictionary_replaces_whole_words_only() {
        let dict = [entry("говор", "Govor")];
        assert_eq!(apply("поставь говор", &dict, true), "Поставь Govor");
        assert_eq!(apply("уговорил его", &dict, true), "Уговорил его");
    }

    #[test]
    fn dictionary_ignores_case() {
        let dict = [entry("постгрес", "PostgreSQL")];
        assert_eq!(apply("База на Постгрес.", &dict, true), "База на PostgreSQL.");
    }

    #[test]
    fn new_line_is_a_command() {
        assert_eq!(
            apply("Первый пункт. Новая строка. Второй пункт.", &[], true),
            "Первый пункт.\nВторой пункт."
        );
        assert_eq!(apply("привет, новый абзац, как дела", &[], true), "Привет\n\nКак дела");
    }

    #[test]
    fn commands_can_be_turned_off() {
        assert_eq!(apply("это новая строка кода", &[], false), "Это новая строка кода");
    }

    #[test]
    fn fillers_disappear() {
        assert_eq!(apply("Ну, эм, давай завтра", &[], true), "Ну давай завтра");
        // А слово, в котором звук только часть, остаётся.
        assert_eq!(apply("эмблема клуба", &[], true), "Эмблема клуба");
    }

    #[test]
    fn hint_is_what_should_appear_in_text() {
        let dict = [entry("постгрес", "PostgreSQL"), entry("Tauri", ""), entry("", "")];
        assert_eq!(hint(&dict), "PostgreSQL, Tauri");
    }

    #[test]
    fn only_words_both_drafts_agree_on_are_stable() {
        assert_eq!(stable_prefix("", "Привет, как"), "");
        assert_eq!(stable_prefix("Привет, как", "Привет, как дела"), "Привет, ");
        assert_eq!(stable_prefix("Привет, как дела", "Привет, как дела у"), "Привет, как ");
        // Модель переписала середину — устоявшимся остаётся то, что до неё.
        assert_eq!(stable_prefix("Завтра в пять вечера", "Завтра в шесть вечера мы"), "Завтра в ");
        // Перенос строки сохраняется.
        assert_eq!(stable_prefix("Пункт.\nДругой пункт", "Пункт.\nДругой пункт и"), "Пункт.\nДругой ");
    }

    #[test]
    fn common_prefix_counts_letters_not_bytes() {
        assert_eq!(common_prefix("Привет, как ", "Привет. Как дела "), 6);
        assert_eq!(common_prefix("abc", "abc"), 3);
        assert_eq!(common_prefix("", "abc"), 0);
    }

    #[test]
    fn words_are_counted_without_punctuation() {
        assert_eq!(words("Привет, мир — это я."), 4);
    }
}
