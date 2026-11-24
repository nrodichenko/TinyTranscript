//! Titles (from Claude) and the people part of file names ('_with_Boris_Sofia').

use crate::py;
use crate::Cfg;
use anyhow::Result;
use pipeline_common as common;
use serde_json::{Map, Value};
use std::collections::HashMap;
use unicode_normalization::char::canonical_combining_class;
use unicode_normalization::UnicodeNormalization;

// ------------------------------------------------------------------ titles

const DANGLING: &[&str] = &["и", "в", "во", "на", "с", "со", "по", "для", "о", "об", "к", "у", "а", "но", "из", "от", "за",
                            "and", "or", "of", "the", "for", "to", "in", "on", "with", "a", "an", "&", "-", "–", "—"];

/// First line of Claude's reply, without characters file names cannot have or quotes around it.
pub fn clean_title(reply: &str) -> String {
    let first = py::splitlines(py::strip(reply), false).first().copied().unwrap_or("").to_string();
    let mut t = String::new();
    let mut in_run = false;
    for c in first.chars() {
        if matches!(c, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\n' | '\r' | '\t') {
            if !in_run {
                t.push(' ');
            }
            in_run = true;
        } else {
            t.push(c);
            in_run = false;
        }
    }
    py::squash(py::strip(&t).trim_matches(['.', '\'', '"', '`']))
}

fn has_word_char(s: &str) -> bool {
    s.chars().any(|c| c.is_alphanumeric() || c == '_')
}

/// Cut at a word boundary and drop a dangling connector ('и', 'and', ...) or punctuation.
pub fn fit_title(t: &str, max_chars: i64) -> String {
    let mut t = t.to_string();
    if py::char_len(&t) as i64 > max_chars {
        let head = py::prefix(&t, max_chars + 1);
        // the title alone; the date and the '_with_' names come on top
        let cut = match head.rfind(' ') {
            Some(i) => &head[..i],
            None => "", // no space: cut at the limit
        };
        t = if py::char_len(cut) as i64 >= py::floor_div(max_chars, 2) {
            cut.to_string()
        } else {
            py::prefix(&t, max_chars).to_string()
        };
    }
    let mut words = py::split(&t);
    while words.len() > 1 {
        let last = words[words.len() - 1];
        if DANGLING.contains(&py::lower(last).as_str()) || !has_word_char(last) {
            words.pop();
        } else {
            break;
        }
    }
    words.join(" ").trim_end_matches([' ', ',', ';', '-', '–', '—']).to_string()
}

/// A title of at most `max_chars` written by Claude from the full transcript.
pub fn claude_title(transcript: &str, max_chars: i64, name: &str) -> Result<String> {
    let text = if py::char_len(transcript) > 150_000 {
        format!("{}\n[...]\n{}", py::prefix(transcript, 100_000), py::suffix_chars(transcript, 50_000))
    } else {
        transcript.to_string()
    };
    let prompt = format!(
        "Below is the full transcript of a work meeting, speakers named where known. Reply with a short \
         title for the meeting, at most {max_chars} characters, in the language most of the meeting is \
         in, naming the subject (and the company if obvious), like 'Budget review' or 'Supplier PCB \
         discussion'. Do not put people's names in it; they are added to the file name separately. \
         Reply with the title only.");
    let reply = common::claude::ask(&prompt, &text, format!("title {name}").trim(), 300)?;
    let mut t = clean_title(&reply);
    if py::char_len(&t) as i64 > max_chars {
        // Haiku often runs over, above all in Russian: ask it to shorten just the title
        let p2 = format!("Shorten this meeting title to at most {max_chars} characters, same language, keeping \
                          its meaning. Reply with the title only.");
        if let Ok(r) = common::claude::ask(&p2, &t, format!("shorten title {name}").trim(), 60) {
            let t2 = clean_title(&r);
            if !t2.is_empty() && py::char_len(&t2) < py::char_len(&t) {
                t = t2;
            }
        }
    }
    Ok(fit_title(&t, max_chars))
}

// ------------------------------------------------------------------ names in file names

const CYR_FROM: &str = "абвгдеёжзийклмнопрстуфхцчшщъыьэюяіїєґ";
const CYR_TO: [&str; 37] = ["a", "b", "v", "g", "d", "e", "yo", "zh", "z", "i", "y", "k", "l", "m", "n", "o", "p",
                            "r", "s", "t", "u", "f", "kh", "ts", "ch", "sh", "shch", "", "y", "", "e", "yu", "ya",
                            "i", "yi", "ye", "g"];

fn cyrillic(c: char) -> Option<&'static str> {
    CYR_FROM.chars().position(|x| x == c).map(|i| CYR_TO[i])
}

/// 'Борис' -> 'Boris', 'José' -> 'Jose'. Characters with no Latin form are dropped.
pub fn transliterate(name: &str) -> String {
    let mut out = String::new();
    for ch in name.chars() {
        let low: String = ch.to_lowercase().collect();
        let mut it = low.chars();
        let single = match (it.next(), it.next()) {
            (Some(c), None) => Some(c),
            _ => None,
        };
        match single.and_then(cyrillic) {
            Some(t) if low != ch.to_string() => {
                let mut cs = t.chars();
                if let Some(f) = cs.next() {
                    out.extend(f.to_uppercase());
                    out.push_str(cs.as_str());
                }
            }
            Some(t) => out.push_str(t),
            None => out.push(ch),
        }
    }
    let s: String = out.nfkd().filter(|c| canonical_combining_class(*c) == 0).collect();
    let s: String = s.chars().map(|c| if c.is_ascii_alphanumeric() || c == ' ' || c == '-' { c } else { ' ' }).collect();
    py::squash(&s)
}

/// The other people named for sure, most speech first: ['Peter Novak'] or first names
/// ['Boris', 'Sofia'] when there are several, at most with_max_names, then '+N'.
/// `seconds` is each label's speech in the report.
pub fn people_from(labels: &Map<String, Value>, seconds: &HashMap<String, f64>, cfg: &Cfg) -> Vec<String> {
    let me: Vec<String> = cfg.self_names.iter().map(|n| py::casefold(n)).collect();
    let short: Vec<(String, String)> = cfg.short_names.iter().map(|(k, v)| (py::casefold(k), v.clone())).collect();
    let short_get = |k: &str| short.iter().rev().find(|(a, _)| a == k).map(|(_, v)| v.clone());
    let mut labs: Vec<&str> = Vec::new();
    for v in labels.values() {
        if let Some(s) = v.as_str() {
            if !labs.contains(&s) {
                labs.push(s);
            }
        }
    }
    // Equal speech times keep the order the speakers first appear in (as the Python does since 2026-11-24).
    labs.sort_by(|a, b| {
        let (x, y) = (seconds.get(*a).copied().unwrap_or(0.0), seconds.get(*b).copied().unwrap_or(0.0));
        y.partial_cmp(&x).unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut names: Vec<String> = Vec::new();
    for lab in labs {
        let cf = py::casefold(lab);
        if lab.is_empty() || lab.ends_with('?') || crate::is_speaker_label(lab) || me.contains(&cf)
            || !cfg.is_person(lab) {
            continue;
        }
        let sh = short_get(&cf);
        let mut t = transliterate(sh.as_deref().unwrap_or(lab));
        let words = py::split(&t);
        if words.len() >= 3 && sh.is_none() {
            t = words[0].to_string();
        }
        if !t.is_empty() && !names.contains(&t) {
            names.push(t);
        }
    }
    if names.len() > 1 {
        let firsts: Vec<String> = names.iter().map(|n| py::split(n)[0].to_string()).collect();
        names = firsts.iter().zip(&names)
            .map(|(f, n)| if firsts.iter().filter(|x| *x == f).count() == 1 { f.clone() } else { n.clone() })
            .collect();
    }
    let max = cfg.with_max_names;
    let len = names.len() as i64;
    let extra = len - max;
    let keep = if max < 0 { (len + max).max(0) } else { max.min(len) } as usize;
    names.truncate(keep);
    if extra > 0 {
        names.push(format!("+{extra}"));
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn transliterate_matches_python() {
        // [match_speakers.transliterate(x) for x in ...]
        for (src, want) in [("Борис", "Boris"), ("José", "Jose"), ("Мария Ромашка Групп", "Mariya Romashka Grupp"),
                            ("Щукин Ёжик", "Shchukin Yozhik"), ("Объект Ъ", "Obekt"), ("Їжак Єва Ґанок", "Yizhak Yeva Ganok"),
                            ("Boris Ivanov?", "Boris Ivanov"), ("Zoë O'Brien-Smith", "Zoe O Brien-Smith"),
                            ("李小龍", ""), ("Ærøskøbing ﬁ", "r sk bing fi"), ("Анна Иванова", "Anna Ivanova")] {
            assert_eq!(transliterate(src), want, "{src}");
        }
    }

    #[test]
    fn titles_match_python() {
        // match_speakers.clean_title(...)
        assert_eq!(clean_title("  \"Acme: PCB / review.\"\nsecond line"), "Acme PCB review");
        assert_eq!(clean_title("`Atlas debrief`"), "Atlas debrief");
        assert_eq!(clean_title(""), "");
        assert_eq!(clean_title("'Встреча с Ромашка Групп...'"), "Встреча с Ромашка Групп");
        // match_speakers.fit_title(t, 30)
        assert_eq!(fit_title("Обсуждение производства дронов и логистики", 30), "Обсуждение производства дронов");
        assert_eq!(fit_title("Acme PCB discussion and the", 30), "Acme PCB discussion");
        // no space in the first 31 characters: Python keeps 31 (one over the limit); kept for parity
        assert_eq!(fit_title("Abcdefghijklmnopqrstuvwxyzabcdefghij", 30), "Abcdefghijklmnopqrstuvwxyzabcd");
        assert_eq!(fit_title("A Bcdefghijklmnopqrstuvwxyzabcdefghij", 30), "A Bcdefghijklmnopqrstuvwxyzabc");
        assert_eq!(fit_title("Short title -", 30), "Short title");
        assert_eq!(fit_title("Planning, ", 30), "Planning");
        assert_eq!(fit_title("Q3 plan — budget и", 30), "Q3 plan — budget");
    }

    fn cfg() -> Cfg {
        // the reference values were made with self_names ["Max"] (the default is empty now)
        let mut v = crate::default_config();
        v["self_names"] = serde_json::json!(["Max"]);
        Cfg::from_value(&v).unwrap()
    }

    #[test]
    fn people_match_python() {
        let mut c = cfg();
        // match_speakers.people_in with a made-up entry/report (reference values from Python)
        let labels = json!({"Speaker 1": "Max", "Speaker 2": "Sofia Weber", "Speaker 3": "Boris",
                            "Speaker 4": "Speaker 4", "Speaker 5": "Grigoriy?"});
        let secs: HashMap<String, f64> = [("Max", 500.0), ("Sofia Weber", 120.0), ("Boris", 300.0)]
            .iter().map(|(a, b)| (a.to_string(), *b)).collect();
        assert_eq!(people_from(labels.as_object().unwrap(), &secs, &c), vec!["Boris", "Sofia"]);

        let labels = json!({"Speaker 1": "Peter Novak", "Speaker 2": "max"});
        assert_eq!(people_from(labels.as_object().unwrap(), &HashMap::new(), &c), vec!["Peter Novak"]);

        let labels = json!({"a": "Анна Иванова", "b": "Олег Смирнов", "c": "Мария Ромашка Групп",
                            "d": "Ivan Orlov", "e": "Ivan Petrov", "f": "Unknown 2"});
        let secs: HashMap<String, f64> = [("Анна Иванова", 50.0), ("Олег Смирнов", 90.0), ("Мария Ромашка Групп", 40.0),
                                          ("Ivan Orlov", 30.0), ("Ivan Petrov", 20.0)]
            .iter().map(|(a, b)| (a.to_string(), *b)).collect();
        assert_eq!(people_from(labels.as_object().unwrap(), &secs, &c), vec!["Oleg", "Anna", "Mariya", "+2"]);
        c.with_max_names = 5;
        assert_eq!(people_from(labels.as_object().unwrap(), &secs, &c),
                   vec!["Oleg", "Anna", "Mariya", "Ivan Orlov", "Ivan Petrov"]);
        c.short_names = vec![("мария ромашка групп".into(), "Mariya R G".into())];
        assert_eq!(people_from(labels.as_object().unwrap(), &secs, &c),
                   vec!["Oleg", "Anna", "Mariya", "Ivan Orlov", "Ivan Petrov"]);
        c.short_names = vec![("Анна Иванова".into(), "Anna I".into())];
        let labels = json!({"a": "Анна Иванова", "b": "Peter Novak"});
        let secs: HashMap<String, f64> = [("Анна Иванова", 10.0), ("Peter Novak", 5.0)]
            .iter().map(|(a, b)| (a.to_string(), *b)).collect();
        assert_eq!(people_from(labels.as_object().unwrap(), &secs, &c), vec!["Anna", "Peter"]);
    }
}
