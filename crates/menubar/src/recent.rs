//! "Recent transcripts": the latest finished transcripts, each with its speakers; a speaker can be
//! named from the list of people MacWhisper knows, or with a new name. The name is written into
//! MacWhisper's library with the speaker's voiceprint (match_speakers::mw_write), then the speaker
//! job re-checks the transcript, which relabels it and renames its files ('_with_...').

use crate::app::{menu_item, Controller};
use objc2::rc::Retained;
use objc2::{sel, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSAlert, NSAlertFirstButtonReturn, NSApplication, NSControlStateValueOn, NSMenu, NSMenuItem, NSTextField};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};
use serde_json::{Map, Value};
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const SHOWN: usize = 10;

/// One "name this speaker" menu item.
struct Target {
    session: String,
    speaker: String,
    transcript: String,
    title: String,
    from: String,
    /// None: ask for a new name
    name: Option<String>,
}

thread_local! {
    static TARGETS: RefCell<Vec<Target>> = const { RefCell::new(Vec::new()) };
    static FILES: RefCell<Vec<PathBuf>> = const { RefCell::new(Vec::new()) };
}

/// (notification title, text) from renames running on their own thread.
static RESULTS: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

fn mtime(p: &Path) -> String {
    std::fs::metadata(p).and_then(|m| m.modified()).map(|t| format!("{t:?}")).unwrap_or_default()
}

/// Changes when the list could change (a transcript processed, re-checked or renamed).
pub fn signature(meta: &Path) -> String {
    format!("{}|{}", mtime(&meta.join("state.json")), mtime(&meta.join("reports")))
}

/// "20261124_-_Планёрка_with_Boris" -> ("24 Nov", "Планёрка")
fn describe(stem: &str) -> (String, String) {
    let base = stem.split("_with_").next().unwrap_or(stem);
    let (date, title) = match base.split_once("_-_") {
        Some((d, t)) if d.len() == 8 && d.bytes().all(|b| b.is_ascii_digit()) => (d, t),
        _ => ("", base),
    };
    let day = chrono::NaiveDate::parse_from_str(date, "%Y%m%d").map(|d| d.format("%-d %b").to_string()).unwrap_or_default();
    (day, title.replace('_', " "))
}

fn minutes(secs: f64) -> String {
    if secs < 60.0 { format!("{}s", secs.round() as i64) } else { format!("{} min", (secs / 60.0).round() as i64) }
}

fn sep(mtm: MainThreadMarker) -> Retained<NSMenuItem> {
    NSMenuItem::separatorItem(mtm)
}

/// Rebuild the submenu from state.json, the reports and MacWhisper's people.
pub(crate) fn rebuild(menu: &NSMenu, mtm: MainThreadMarker, controller: &Controller, meta: &Path, processed: &Path) {
    menu.removeAllItems();
    TARGETS.with(|t| t.borrow_mut().clear());
    FILES.with(|f| f.borrow_mut().clear());
    let state: Map<String, Value> = pipeline_common::load_json(&meta.join("state.json"))
        .and_then(|v| v.as_object().cloned()).unwrap_or_default();
    let mut done: Vec<(&String, &Value)> = state.iter()
        .filter(|(k, e)| e.get("status").and_then(Value::as_str) == Some("done")
            && e.get("session").and_then(Value::as_str).is_some_and(|s| !s.is_empty())
            && processed.join(k).is_file())
        .collect();
    // newest recording first (the date in the name), then the latest processed
    done.sort_by(|a, b| {
        let k = |(n, e): &(&String, &Value)| (n.get(..8).unwrap_or("").to_string(),
                                               e.get("processed_at").and_then(Value::as_str).unwrap_or("").to_string());
        k(b).cmp(&k(a))
    });
    if done.is_empty() {
        menu.addItem(&menu_item(mtm, "No transcripts yet", None, controller));
        return;
    }
    let people = match_speakers::mw_write::known_people().unwrap_or_default();
    for (key, entry) in done.into_iter().take(SHOWN) {
        let stem = key.strip_suffix(".txt").unwrap_or(key);
        let (day, title) = describe(stem);
        let session = entry["session"].as_str().unwrap_or("").to_string();
        let item = menu_item(mtm, &if day.is_empty() { title.clone() } else { format!("{day} · {title}") }, None, controller);
        let sub = NSMenu::new(mtm);
        let open = menu_item(mtm, "Open transcript", Some(sel!(openTranscript:)), controller);
        FILES.with(|f| {
            open.setTag(f.borrow().len() as isize);
            f.borrow_mut().push(processed.join(key));
        });
        sub.addItem(&open);
        sub.addItem(&sep(mtm));
        let report = pipeline_common::load_json(&meta.join("reports").join(format!("{stem}.json")));
        let mut speakers: Vec<Value> = report.as_ref().and_then(|r| r["speakers"].as_array().cloned()).unwrap_or_default();
        speakers.retain(|s| s["seconds"].as_f64().unwrap_or(0.0) >= 1.0 && s["speaker_id"].as_str().is_some());
        speakers.sort_by(|a, b| b["seconds"].as_f64().partial_cmp(&a["seconds"].as_f64()).unwrap_or(std::cmp::Ordering::Equal));
        let mut unknown = 0;
        for sp in &speakers {
            let current = sp["current"].as_str().unwrap_or("").to_string(); // the name in MacWhisper
            let label = sp["label"].as_str().unwrap_or("").to_string(); // the name in the transcript
            let resolved = sp["resolved"].as_bool().unwrap_or(false);
            let shown = if label.is_empty() { current.clone() } else { label.clone() };
            if !resolved {
                unknown += 1;
            }
            let sp_title = format!("{shown}{} · {}", if resolved { "" } else { "  ⚠︎" }, minutes(sp["seconds"].as_f64().unwrap_or(0.0)));
            let sp_item = menu_item(mtm, &sp_title, None, controller);
            let sp_menu = NSMenu::new(mtm);
            let target = |name: Option<String>| Target {
                session: session.clone(), speaker: sp["speaker_id"].as_str().unwrap_or("").to_string(),
                transcript: key.clone(), title: title.clone(), from: shown.clone(), name,
            };
            let new = menu_item(mtm, "New name…", Some(sel!(renameSpeakerNew:)), controller);
            TARGETS.with(|t| {
                new.setTag(t.borrow().len() as isize);
                t.borrow_mut().push(target(None));
            });
            sp_menu.addItem(&new);
            sp_menu.addItem(&sep(mtm));
            // a guess ('Ivan?') first, to confirm it in one click
            let guess = label.strip_suffix('?').filter(|g| people.iter().any(|p| p == g)).map(str::to_string);
            let mut ordered: Vec<&String> = people.iter().collect();
            if let Some(g) = &guess {
                ordered.retain(|p| *p != g);
                ordered.insert(0, people.iter().find(|p| *p == g).unwrap());
            }
            for (i, p) in ordered.into_iter().enumerate() {
                // ✓ on the name in MacWhisper (nothing to do) and on a voice match not yet in
                // MacWhisper (choosing it writes it there)
                let is_current = *p == current;
                let is_label = resolved && *p == label;
                let shown = if i == 0 && guess.is_some() { format!("{p}  (likely)") } else { p.clone() };
                let it = menu_item(mtm, &shown,
                                   if is_current { None } else { Some(sel!(renameSpeaker:)) }, controller);
                if is_current || is_label {
                    it.setState(NSControlStateValueOn);
                }
                if !is_current {
                    TARGETS.with(|t| {
                        it.setTag(t.borrow().len() as isize);
                        t.borrow_mut().push(target(Some(p.clone())));
                    });
                }
                sp_menu.addItem(&it);
            }
            sp_item.setSubmenu(Some(&sp_menu));
            sub.addItem(&sp_item);
        }
        if speakers.is_empty() {
            sub.addItem(&menu_item(mtm, "No speakers", None, controller));
        }
        if unknown > 0 {
            item.setTitle(&NSString::from_str(&format!("{}  ⚠︎", item.title())));
        }
        item.setSubmenu(Some(&sub));
        menu.addItem(&item);
    }
}

pub fn transcript_path(tag: isize) -> Option<PathBuf> {
    FILES.with(|f| f.borrow().get(tag as usize).cloned())
}

/// Ask for a new name (the app comes to the front for the dialog).
fn ask_name(mtm: MainThreadMarker, from: &str, title: &str) -> Option<String> {
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str(&format!("Name for {from}")));
    alert.setInformativeText(&NSString::from_str(&format!("In “{title}”. Saved in MacWhisper too, with this voice.")));
    alert.addButtonWithTitle(&NSString::from_str("Save"));
    alert.addButtonWithTitle(&NSString::from_str("Cancel"));
    let field = NSTextField::initWithFrame(NSTextField::alloc(mtm), NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(280.0, 24.0)));
    field.setPlaceholderString(Some(&NSString::from_str("First and last name")));
    alert.setAccessoryView(Some(&field));
    alert.window().setInitialFirstResponder(Some(&field));
    #[allow(deprecated)]
    NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
    if alert.runModal() != NSAlertFirstButtonReturn {
        return None;
    }
    let name = field.stringValue().to_string();
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// The menu item `tag` was chosen: name the speaker (asking for the name for "New name…").
pub fn rename(mtm: MainThreadMarker, tag: isize) {
    let Some((session, speaker, transcript, title, from, name)) = TARGETS.with(|t| {
        t.borrow().get(tag as usize).map(|x| (x.session.clone(), x.speaker.clone(), x.transcript.clone(), x.title.clone(),
                                               x.from.clone(), x.name.clone()))
    }) else { return };
    let Some(name) = name.or_else(|| ask_name(mtm, &from, &title)) else { return };
    // computing the voiceprint takes a few seconds: off the main thread
    std::thread::spawn(move || {
        let msg = match match_speakers::mw_write::rename_speaker(&session, &speaker, &name, &transcript) {
            Ok(r) => {
                crate::jobs::kick("local.speaker-matching"); // relabel and rename the transcript now
                ("Speaker named".to_string(), format!("{} is now {} in “{title}”{}", r.from, r.to,
                                                      if r.new_person { " (new person)" } else { "" }))
            }
            Err(e) => ("Could not name the speaker".to_string(), format!("{e:#}")),
        };
        RESULTS.lock().unwrap_or_else(|e| e.into_inner()).push(msg);
    });
}

/// Results of finished renames, for notifications.
pub fn take_results() -> Vec<(String, String)> {
    std::mem::take(&mut *RESULTS.lock().unwrap_or_else(|e| e.into_inner()))
}
