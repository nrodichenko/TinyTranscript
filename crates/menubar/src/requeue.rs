//! Hand recordings MacWhisper missed to it again.
//!
//! MacWhisper's watch folder reacts only to files arriving in it: it does not look at what is
//! already there when it starts or when the folder is added. A recording that arrived while
//! MacWhisper was not running, or could not open the folder, waits forever. Such a recording is
//! moved out of the work folder and back, which MacWhisper sees as a new arrival.
//!
//! A recording is handed again only if MacWhisper runs and watches the work folder, it has no
//! transcript, it has been in the folder for 2 minutes, and MacWhisper's log of its current run
//! never named it ("File was added: <path>"), so anything MacWhisper has queued or is transcribing
//! is left alone. The automatic check (every minute) hands each recording at most once per
//! MacWhisper run; "Hand waiting recordings to MacWhisper" in the menu skips that limit. The jobs
//! are held meanwhile, so no fetcher looks for the recording while it is out of the folder.

use crate::workfolder::{macwhisper_watch, Watch};
use pipeline_common as common;
use std::collections::HashSet;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};
use unicode_normalization::UnicodeNormalization;

const SETTLE: f64 = 120.0; // seconds in the folder before a recording counts as missed

fn nfc(s: &str) -> String {
    s.nfc().collect()
}

fn macwhisper_running() -> bool {
    std::process::Command::new("/usr/bin/pgrep").args(["-x", "MacWhisper"]).stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null()).status().map(|s| s.success()).unwrap_or(false)
}

/// MacWhisper's newest log (its current run, when it runs) and its text.
fn current_log() -> Option<(PathBuf, String)> {
    let dir = common::home().join("Library/Application Support/MacWhisper/Logs");
    let mut logs: Vec<PathBuf> = std::fs::read_dir(&dir).ok()?.flatten().map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "log")).collect();
    logs.sort();
    let p = logs.pop()?;
    let text = std::fs::read_to_string(&p).ok()?;
    Some((p, text))
}

/// Paths MacWhisper's log names as added to a watched folder (NFC).
fn added_in(log: &str) -> HashSet<String> {
    log.lines().filter_map(|l| l.split_once("[folderWatcher] File was added: ").map(|(_, p)| nfc(p.trim()))).collect()
}

/// Seconds since `p` arrived in its folder (its inode change time: a move sets it, a copy too).
fn in_folder_for(p: &Path) -> f64 {
    let Ok(m) = std::fs::metadata(p) else { return 0.0 };
    common::now_ts() - m.ctime() as f64
}

/// Recordings in `folder` MacWhisper has missed, given its log of the current run.
pub fn missed(folder: &Path, originals: &Path, log: &str) -> Vec<PathBuf> {
    let added = added_in(log);
    crate::status::untranscribed(folder, originals).into_iter().map(|(p, _)| p)
        .filter(|p| in_folder_for(p) >= SETTLE)
        .filter(|p| !added.contains(&nfc(&p.to_string_lossy())))
        .collect()
}

/// (log file, recording) pairs handed again automatically: once per MacWhisper run each.
static HANDED: Mutex<Vec<(PathBuf, PathBuf)>> = Mutex::new(Vec::new());
static RUNNING: Mutex<()> = Mutex::new(());

/// Hand MacWhisper the recordings it missed. `all`: also those already handed once in this run
/// (the menu). Returns how many were handed.
pub fn run(all: bool) -> usize {
    let Ok(_one) = RUNNING.try_lock() else { return 0 };
    let folder = common::recordings();
    if !macwhisper_running() || macwhisper_watch(&folder) != Watch::Yes {
        return 0;
    }
    let Some((log_path, log)) = current_log() else { return 0 };
    // let MacWhisper finish starting (its log is created at launch)
    let started = std::fs::metadata(&log_path).and_then(|m| m.created()).ok();
    if started.is_some_and(|t| SystemTime::now().duration_since(t).unwrap_or_default() < Duration::from_secs(60)) {
        return 0;
    }
    let mut todo = missed(&folder, &common::originals(), &log);
    if !all {
        let handed = HANDED.lock().unwrap();
        todo.retain(|p| !handed.iter().any(|(l, q)| *l == log_path && q == p));
    }
    if todo.is_empty() || !crate::jobs::try_hold_and_wait(Duration::from_secs(120)) {
        return 0;
    }
    let n = hand(&folder, &todo);
    crate::jobs::release_hold();
    HANDED.lock().unwrap().extend(todo.iter().map(|p| (log_path.clone(), p.clone())));
    n
}

/// Move `files` out of `folder` (next to it, on the same disk) and back.
fn hand(folder: &Path, files: &[PathBuf]) -> usize {
    let log = common::meta().join("match.log");
    let Some(parent) = folder.parent() else { return 0 };
    let side = parent.join(".tinytranscript-requeue");
    if std::fs::create_dir_all(&side).is_err() {
        return 0;
    }
    let mut out = Vec::new();
    for p in files {
        let Some(name) = p.file_name() else { continue };
        let tmp = side.join(name);
        if tmp.exists() {
            continue;
        }
        match std::fs::rename(p, &tmp) {
            Ok(()) => out.push((p.clone(), tmp)),
            Err(e) => common::log_to(&log, &format!("could not hand {} to MacWhisper again: {e}", name.to_string_lossy())),
        }
    }
    std::thread::sleep(Duration::from_secs(3));
    let mut n = 0;
    for (p, tmp) in &out {
        let name = p.file_name().unwrap_or_default().to_string_lossy().into_owned();
        if p.exists() {
            common::log_to(&log, &format!("ERROR {name} could not be moved back: a file of that name appeared; it is in {}", tmp.display()));
            crate::update::notify("Recording left aside", &format!("{name} is in {}", side.display()));
            continue;
        }
        match std::fs::rename(tmp, p) {
            Ok(()) => {
                n += 1;
                common::log_to(&log, &format!("{name}: handed to MacWhisper again (it had not picked it up)"));
            }
            Err(e) => {
                common::log_to(&log, &format!("ERROR {name} could not be moved back ({e}); it is in {}", tmp.display()));
                crate::update::notify("Recording left aside", &format!("{name} is in {}", side.display()));
            }
        }
    }
    let _ = std::fs::remove_dir(&side);
    n
}

/// `TinyTranscript requeue --check`: what would be handed to MacWhisper now (ignoring its start time).
pub fn check() -> Vec<PathBuf> {
    let folder = common::recordings();
    println!("MacWhisper running: {}, watches the work folder: {:?}", macwhisper_running(), macwhisper_watch(&folder));
    let Some((log_path, log)) = current_log() else { return vec![] };
    println!("MacWhisper's log: {}", log_path.display());
    missed(&folder, &common::originals(), &log)
}

/// Check every minute.
pub fn start() {
    std::thread::Builder::new().name("requeue".into()).spawn(|| loop {
        std::thread::sleep(Duration::from_secs(60));
        let _ = std::panic::catch_unwind(|| run(false));
    }).expect("starting the requeue thread");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_only_missed_recordings() {
        let root = std::env::temp_dir().join(format!("tt-requeue-{}", std::process::id()));
        let (inbox, orig) = (root.join("in"), root.join("orig"));
        std::fs::create_dir_all(&inbox).unwrap();
        std::fs::create_dir_all(&orig).unwrap();
        for f in ["a.m4a", "b.m4a", "c.m4a", "c.txt", "d.m4a"] {
            std::fs::write(inbox.join(f), "x").unwrap();
        }
        std::fs::write(orig.join("d.txt"), "x").unwrap();
        // decomposed й in the log, composed on disk: still the same file
        let decomposed: String = "й".nfd().collect();
        std::fs::write(inbox.join("й.m4a"), "x").unwrap();
        let log = format!("[10:00:00.000] [INFO] [folderWatcher] File was added: {}\n\
                           [10:00:01.000] [INFO] [folderWatcher] File was added: {}/{decomposed}.m4a\n",
                          inbox.join("b.m4a").display(), inbox.display());
        // just arrived: not missed yet
        assert!(missed(&inbox, &orig, &log).is_empty());
        let names = |v: Vec<PathBuf>| { let mut v: Vec<String> = v.iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect(); v.sort(); v };
        // with no settle time: a (never added) only; b added, c and d transcribed, й added
        let todo: Vec<PathBuf> = crate::status::untranscribed(&inbox, &orig).into_iter().map(|(p, _)| p)
            .filter(|p| !added_in(&log).contains(&nfc(&p.to_string_lossy()))).collect();
        assert_eq!(names(todo), ["a.m4a"]);
        let _ = std::fs::remove_dir_all(&root);
    }
}
