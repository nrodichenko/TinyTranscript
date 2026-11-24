//! Transcript text (MacWhisper's "grouped by people" txt) and speech spans from word timings.

use crate::py;
use serde_json::Value;
use std::collections::HashMap;

/// One row of MacWhisper's `transcriptline` (hex speaker id, "" if none).
#[derive(Clone, Debug)]
pub struct Line {
    pub spk: String,
    pub text: String,
    pub words: Option<String>,
    pub start: f64,
    pub end: f64,
}

pub fn norm_text(s: &str) -> String {
    py::squash(&py::lower(s))
}

/// Paragraphs of 'Label' line + text line(s): (line index of the label, label, first text line).
pub fn txt_blocks(text: &str) -> Vec<(usize, String, String)> {
    let lines = py::splitlines(text, false);
    let blank = |i: usize| py::strip(lines[i]).is_empty();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if !blank(i) && (i == 0 || blank(i - 1)) && i + 1 < lines.len() && !blank(i + 1) {
            out.push((i, py::strip(lines[i]).to_string(), lines[i + 1].to_string()));
            i += 2;
            while i < lines.len() && !blank(i) {
                i += 1;
            }
        } else {
            i += 1;
        }
    }
    out
}

/// Which MacWhisper speaker each label in the txt stands for, in order of first vote.
///
/// The label is the speaker's name when MacWhisper exported the file; the speaker may have been
/// renamed since. Each paragraph starts with the text of one transcript line, so walk paragraphs and
/// transcript lines in order and let each paragraph vote for its line's speaker.
pub fn label_to_speaker(text: &str, lines: &[Line]) -> Vec<(String, String)> {
    let tl: Vec<(&str, String)> = lines.iter()
        .filter(|l| !l.spk.is_empty() && !l.text.is_empty() && !py::strip(&l.text).is_empty())
        .map(|l| (l.spk.as_str(), norm_text(&l.text)))
        .collect();
    // label -> [(speaker, votes)] in insertion order (Counter.most_common(1) keeps the first on ties)
    let mut votes: Vec<(String, Vec<(String, usize)>)> = Vec::new();
    let mut j = 0;
    for (_, label, first) in txt_blocks(text) {
        let b = norm_text(&first);
        for (k, (spk, t)) in tl.iter().enumerate().skip(j).take(60) {
            if !t.is_empty() && (b.starts_with(t.as_str()) || t.starts_with(b.as_str())) {
                let idx = match votes.iter().position(|(l, _)| *l == label) {
                    Some(i) => i,
                    None => {
                        votes.push((label.clone(), Vec::new()));
                        votes.len() - 1
                    }
                };
                let counter = &mut votes[idx].1;
                match counter.iter_mut().find(|(s, _)| s == spk) {
                    Some(e) => e.1 += 1,
                    None => counter.push((spk.to_string(), 1)),
                }
                j = k + 1;
                break;
            }
        }
    }
    votes.into_iter()
        .map(|(label, c)| {
            let mut best = &c[0];
            for e in &c[1..] {
                if e.1 > best.1 {
                    best = e;
                }
            }
            (label, best.0.clone())
        })
        .collect()
}

/// Replace the label line of every paragraph whose label is in `new_label_for`.
pub fn relabel(text: &str, new_label_for: &HashMap<String, String>) -> String {
    let mut lines: Vec<String> = py::splitlines(text, true).into_iter().map(String::from).collect();
    for (i, label, _) in txt_blocks(text) {
        if let Some(new) = new_label_for.get(&label) {
            let bare = lines[i].trim_end_matches(['\r', '\n']).to_string();
            lines[i] = lines[i].replacen(&bare, new, 1);
        }
    }
    lines.concat()
}

/// Speech spans per speaker (seconds) from MacWhisper's word timings, merged across gaps <= `gap`.
pub fn speaker_spans(lines: &[Line], gap: f64) -> Vec<(String, Vec<[f64; 2]>)> {
    let mut spans: Vec<(String, Vec<[f64; 2]>)> = Vec::new();
    for l in lines {
        if l.spk.is_empty() {
            continue;
        }
        let parsed: Value = match &l.words {
            Some(w) if !w.is_empty() => serde_json::from_str(w).unwrap_or(Value::Array(vec![])),
            _ => Value::Array(vec![]),
        };
        let words: Vec<(f64, f64)> = if py::truthy(Some(&parsed)) {
            match &parsed {
                Value::Array(a) => a.iter()
                    .filter_map(|w| w.as_object())
                    .map(|w| {
                        let g = |k: &str| w.get(k).and_then(Value::as_f64).unwrap_or(0.0);
                        (g("startTime"), g("endTime"))
                    })
                    .collect(),
                _ => vec![],
            }
        } else {
            vec![(l.start, l.end)]
        };
        let idx = match spans.iter().position(|(s, _)| *s == l.spk) {
            Some(i) => i,
            None => {
                spans.push((l.spk.clone(), Vec::new()));
                spans.len() - 1
            }
        };
        let lst = &mut spans[idx].1;
        for (s, e) in words {
            let (a, b) = (s / 1000.0, e / 1000.0);
            if b <= a {
                continue;
            }
            match lst.last_mut() {
                Some(last) if a - last[1] <= gap => last[1] = last[1].max(b),
                _ => lst.push([a, b]),
            }
        }
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(spk: &str, text: &str, words: Option<&str>, start: f64, end: f64) -> Line {
        Line { spk: spk.into(), text: text.into(), words: words.map(String::from), start, end }
    }

    const TXT: &str = "Speaker 1\nHello there, how are you\ndoing today\n\nSpeaker 2\nFine thanks.\n\n  Max  \nOK then\n\nlonely line\n\nSpeaker 1\nAnd one more\n";

    #[test]
    fn blocks_match_python() {
        // list(match_speakers.txt_blocks(TXT))
        let want = vec![(0, "Speaker 1", "Hello there, how are you"), (4, "Speaker 2", "Fine thanks."),
                        (7, "Max", "OK then"), (12, "Speaker 1", "And one more")];
        let got = txt_blocks(TXT);
        assert_eq!(got.len(), want.len());
        for ((i, l, f), (wi, wl, wf)) in got.iter().zip(want) {
            assert_eq!((*i, l.as_str(), f.as_str()), (wi, wl, wf));
        }
    }

    #[test]
    fn relabel_matches_python() {
        // match_speakers.relabel(TXT, {"Speaker 1": "Boris", "Max": "Max R?"})
        let m: HashMap<String, String> = [("Speaker 1", "Boris"), ("Max", "Max R?")].iter()
            .map(|(a, b)| (a.to_string(), b.to_string())).collect();
        assert_eq!(relabel(TXT, &m),
                   "Boris\nHello there, how are you\ndoing today\n\nSpeaker 2\nFine thanks.\n\nMax R?\nOK then\n\nlonely line\n\nBoris\nAnd one more\n");
        assert_eq!(relabel("Speaker 1\r\nHi\r\n", &m), "Boris\r\nHi\r\n");
    }

    #[test]
    fn labels_vote_like_python() {
        let lines = vec![
            line("AA", "Hello there, how are you doing today", None, 0.0, 1.0),
            line("BB", " Fine thanks.", None, 0.0, 1.0),
            line("", "ignored", None, 0.0, 1.0),
            line("CC", "OK then", None, 0.0, 1.0),
            line("AA", "And one more", None, 0.0, 1.0),
        ];
        // match_speakers.label_to_speaker(TXT, rows) -> {'Speaker 1': 'AA', 'Speaker 2': 'BB', 'Max': 'CC'}
        let got = label_to_speaker(TXT, &lines);
        let want = [("Speaker 1", "AA"), ("Speaker 2", "BB"), ("Max", "CC")];
        assert_eq!(got.len(), 3);
        for ((l, s), (wl, ws)) in got.iter().zip(want) {
            assert_eq!((l.as_str(), s.as_str()), (wl, ws));
        }
    }

    #[test]
    fn spans_match_python() {
        let lines = vec![
            line("AA", "x", Some(r#"[{"startTime":1000,"endTime":1500},{"startTime":1700,"endTime":2000},{"startTime":2400,"endTime":2400},{"startTime":2500,"endTime":3000}]"#), 1000.0, 3000.0),
            line("BB", "y", Some("not json"), 3000.0, 4200.0),
            line("", "z", None, 0.0, 9.0),
            line("AA", "w", Some("[]"), 3100.0, 3500.0),
            line("CC", "v", Some(r#"[{"startTime":5000,"endTime":4000}]"#), 0.0, 0.0),
        ];
        // match_speakers.speaker_spans(rows):
        // {'AA': [[1.0, 2.0], [2.5, 3.5]], 'BB': [[3.0, 4.2]], 'CC': []}
        let got = speaker_spans(&lines, 0.3);
        assert_eq!(got.len(), 3);
        assert_eq!(got[0], ("AA".to_string(), vec![[1.0, 2.0], [2.5, 3.5]]));
        assert_eq!(got[1], ("BB".to_string(), vec![[3.0, 4.2]]));
        assert_eq!(got[2], ("CC".to_string(), vec![]));
    }
}
