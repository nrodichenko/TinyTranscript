//! "Change work folder…": pick a folder, optionally move transcripts, settings and logs there,
//! and restart; then MacWhisper is to watch the new folder.
//!
//! MacWhisper keeps its watch folders in its preferences (com.goodsnooze.MacWhisper,
//! `watchedFolders`: JSON strings {"id", "isEnabled", "path", "urlBookmark"}). The bookmark is a
//! security-scoped one, which macOS lets only the app that made it resolve: an entry written by
//! any other app (even with a plain bookmark of the right folder) is shown in MacWhisper's
//! settings but never watched ("Could not resolve bookmark for folder" in its log). So
//! TinyTranscript only reads that setting, and adding a watch folder is done in MacWhisper's own
//! Settings > Watch Folder, with the steps shown and the path on the clipboard.

use anyhow::{bail, Result};
use objc2::rc::Retained;
use objc2::{AllocAnyThread, MainThreadMarker};
use objc2_app_kit::{NSAlert, NSAlertFirstButtonReturn, NSAlertSecondButtonReturn, NSApplication, NSModalResponseOK,
                    NSOpenPanel};
use objc2_foundation::{NSData, NSDataBase64DecodingOptions, NSString, NSURL, NSUserDefaults};
use pipeline_common as common;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const MW_ID: &str = "com.goodsnooze.MacWhisper";

fn ns(s: &str) -> Retained<NSString> {
    NSString::from_str(s)
}

fn with_slash(p: &Path) -> String {
    format!("{}/", p.to_string_lossy().trim_end_matches('/'))
}

/// MacWhisper's preferences, or `TT_MW_PREFS_DOMAIN` (a copy, for testing).
fn mw_domain() -> String {
    std::env::var("TT_MW_PREFS_DOMAIN").ok().filter(|d| !d.is_empty()).unwrap_or_else(|| MW_ID.to_string())
}

fn mw_defaults() -> Option<Retained<NSUserDefaults>> {
    NSUserDefaults::initWithSuiteName(NSUserDefaults::alloc(), Some(&ns(&mw_domain())))
}

/// One of MacWhisper's watch folders.
pub struct Watched {
    /// with a trailing '/'
    pub path: String,
    pub enabled: bool,
    /// MacWhisper cannot open it: its bookmark was not made by MacWhisper
    pub unusable: bool,
}

/// MacWhisper's watch folders.
pub fn watched_folders() -> Vec<Watched> {
    let Some(d) = mw_defaults() else { return vec![] };
    let Some(arr) = d.stringArrayForKey(&ns("watchedFolders")) else { return vec![] };
    arr.iter().filter_map(|s| serde_json::from_str::<Value>(&s.to_string()).ok())
        .filter_map(|v| Some(Watched {
            path: with_slash(Path::new(v["path"].as_str()?)),
            enabled: v["isEnabled"].as_bool().unwrap_or(true),
            unusable: v["urlBookmark"].as_str().is_some_and(plain_bookmark),
        }))
        .collect()
}

/// A plain (not security-scoped) bookmark, base64. A plain bookmark made outside a sandbox
/// carries a readable sandbox extension ("…;com.apple.app-sandbox.read-write;…"); a
/// security-scoped one, as MacWhisper makes, carries it encrypted. MacWhisper resolves its watch
/// folders as security-scoped, so a plain one is never watched.
fn plain_bookmark(b64: &str) -> bool {
    let Some(data) = NSData::initWithBase64EncodedString_options(NSData::alloc(), &ns(b64), NSDataBase64DecodingOptions::empty())
        else { return false };
    let bytes = data.to_vec();
    let needle = b"com.apple.app-sandbox.";
    bytes.windows(needle.len()).any(|w| w == needle)
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Watch {
    Yes,
    No,
    /// there is an entry for the folder, but MacWhisper cannot open it
    Unusable,
    /// MacWhisper's settings cannot be read
    Unknown,
}

/// Whether MacWhisper watches `folder`.
pub fn macwhisper_watch(folder: &Path) -> Watch {
    let w = watched_folders();
    if w.is_empty() && mw_defaults().is_none() {
        return Watch::Unknown;
    }
    let want = with_slash(folder);
    let mine: Vec<&Watched> = w.iter().filter(|e| e.enabled && e.path == want).collect();
    if mine.iter().any(|e| !e.unusable) {
        Watch::Yes
    } else if mine.is_empty() {
        Watch::No
    } else {
        Watch::Unusable
    }
}

/// MacWhisper's newest log says it could not open its watch folder `folder` and has not reported
/// a file in it since (for the "stuck" tip; the entry may have been fixed since, so not proof).
pub fn macwhisper_log_cannot_open(folder: &Path) -> bool {
    let dir = common::home().join("Library/Application Support/MacWhisper/Logs");
    let Ok(rd) = std::fs::read_dir(&dir) else { return false };
    let mut logs: Vec<PathBuf> = rd.flatten().map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "log")).collect();
    logs.sort();
    let Some(text) = logs.last().and_then(|p| std::fs::read_to_string(p).ok()) else { return false };
    let want = with_slash(folder);
    let Some(at) = text.rfind(&format!("Could not resolve bookmark for folder: {want}")) else { return false };
    !text[at..].lines().skip(1).any(|l| l.contains("[folderWatcher]") && l.contains(&want))
}

/// Move every item of `from` into `to` (rename, or copy and delete across disks).
fn move_contents(from: &Path, to: &Path) -> Result<Vec<String>> {
    std::fs::create_dir_all(to)?;
    let mut moved = Vec::new();
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let name = e.file_name();
        if name == ".DS_Store" {
            continue;
        }
        let dst = to.join(&name);
        if dst.exists() {
            bail!("{} already exists in the new folder; nothing more was moved", dst.display());
        }
        if std::fs::rename(e.path(), &dst).is_err() {
            let st = Command::new("/usr/bin/ditto").arg(e.path()).arg(&dst).status()?;
            if !st.success() {
                bail!("copying {} failed", e.path().display());
            }
            if e.path().is_dir() { std::fs::remove_dir_all(e.path())? } else { std::fs::remove_file(e.path())? }
        }
        moved.push(name.to_string_lossy().into_owned());
    }
    Ok(moved)
}

fn alert(mtm: MainThreadMarker, title: &str, text: &str, buttons: &[&str]) -> isize {
    let a = NSAlert::new(mtm);
    a.setMessageText(&ns(title));
    a.setInformativeText(&ns(text));
    for b in buttons {
        a.addButtonWithTitle(&ns(b));
    }
    #[allow(deprecated)]
    NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
    a.runModal()
}

fn pick(mtm: MainThreadMarker, current: &Path) -> Option<PathBuf> {
    let p = NSOpenPanel::openPanel(mtm);
    p.setCanChooseDirectories(true);
    p.setCanChooseFiles(false);
    p.setCanCreateDirectories(true);
    p.setAllowsMultipleSelection(false);
    p.setMessage(Some(&ns("Choose the work folder: recordings arrive there, MacWhisper watches it, transcripts are filed in it")));
    p.setPrompt(Some(&ns("Use This Folder")));
    if let Some(parent) = current.parent() {
        p.setDirectoryURL(Some(&NSURL::fileURLWithPath_isDirectory(&ns(&parent.to_string_lossy()), true)));
    }
    #[allow(deprecated)]
    NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
    if p.runModal() != NSModalResponseOK {
        return None;
    }
    p.URL().and_then(|u| u.path()).map(|s| PathBuf::from(s.to_string()))
}

/// The menu item: ask, then change the work folder on a background thread and restart.
pub fn change(mtm: MainThreadMarker) {
    if std::env::var_os("RECORDINGS_ROOT").is_some() {
        alert(mtm, "The work folder is set by RECORDINGS_ROOT", "Unset it to choose one here.", &["OK"]);
        return;
    }
    let old = common::recordings();
    let Some(new) = pick(mtm, &old) else { return };
    let (old_c, new_c) = (old.canonicalize().unwrap_or(old.clone()), new.canonicalize().unwrap_or(new.clone()));
    if old_c == new_c {
        // the same folder: make sure MacWhisper watches it
        if macwhisper_watch(&old) == Watch::Yes {
            alert(mtm, "Nothing to change", &format!("{} is the work folder, and MacWhisper watches it.", old.display()), &["OK"]);
        } else {
            watch_work_folder(mtm);
        }
        return;
    }
    if new_c.starts_with(&old_c) || old_c.starts_with(&new_c) {
        alert(mtm, "Choose another folder", "The new work folder cannot be inside the current one, or contain it.", &["OK"]);
        return;
    }
    if transcribing() {
        alert(mtm, "MacWhisper is transcribing", "Change the work folder once it is done.", &["OK"]);
        return;
    }
    let has_data = new.join("speaker-matching").is_dir();
    let mw = if macwhisper_watch(&old) != Watch::Unknown { "\n\nThen add it to MacWhisper's watch folders: TinyTranscript shows how." } else { "" };
    let text = format!("From {}\nto {}.{mw}", old.display(), new.display());
    let choice = if has_data {
        // the new folder already is a work folder: switch to it as it is
        let r = alert(mtm, "Switch to this work folder?", &format!("{text}\n\nIt already has transcripts and settings; nothing is moved."),
                      &["Switch", "Cancel"]);
        if r != NSAlertFirstButtonReturn {
            return;
        }
        false
    } else {
        let r = alert(mtm, "Move your recordings to the new work folder?",
                      &format!("{text}\n\nMove moves the waiting recordings, transcripts, settings and logs. Start empty leaves them where they are."),
                      &["Move", "Start Empty", "Cancel"]);
        if r == NSAlertFirstButtonReturn {
            true
        } else if r == NSAlertSecondButtonReturn {
            false
        } else {
            return;
        }
    };
    let (move_data, old2, new2) = (choice, old.clone(), new.clone());
    std::thread::spawn(move || {
        if let Err(e) = apply(&old2, &new2, move_data) {
            crate::update::notify("Work folder not changed", &format!("{e:#}"));
            crate::jobs::release_hold();
        }
    });
}

fn transcribing() -> bool {
    crate::status::macwhisper_status(&common::recordings(), &common::originals(),
        &common::home().join("Library/Application Support/MacWhisper/Database/main.sqlite")).state == "transcribing"
}

/// Make MacWhisper watch the work folder (menu: "Make MacWhisper watch the work folder…", the
/// same folder chosen again, or after a change of work folder): MacWhisper accepts watch folders
/// only from its own settings, so show the steps, put the path on the clipboard, open MacWhisper,
/// and say when it watches the folder.
pub fn watch_work_folder(mtm: MainThreadMarker) {
    let folder = common::recordings();
    let state = macwhisper_watch(&folder);
    if state == Watch::Unknown {
        alert(mtm, "MacWhisper's settings cannot be read", "Is MacWhisper installed?", &["OK"]);
        return;
    }
    if state == Watch::Yes {
        alert(mtm, "MacWhisper watches the work folder", &folder.display().to_string(), &["OK"]);
        return;
    }
    let want = with_slash(&folder);
    let mut remove: Vec<String> = watched_folders().into_iter()
        .filter(|e| e.unusable || (e.path == want && !e.enabled)).map(|e| e.path).collect();
    remove.dedup();
    let remove_step = if remove.is_empty() { String::new() } else {
        format!("Remove {} (select it, then −): {}.\n",
                if remove.len() == 1 { "this entry" } else { "these entries" },
                if remove.iter().all(|p| *p == want) { "MacWhisper cannot open it".to_string() }
                else { remove.join(", ") })
    };
    let text = format!(
        "MacWhisper accepts watch folders only from its own settings.\n\n\
         In MacWhisper, open Settings (⌘,) > Watch Folder.\n{remove_step}\
         Click +, press ⇧⌘G, paste the path (⌘V; it is on the clipboard) and choose the folder:\n{}\n\
         Keep the subfolder depth at 0.\n\nTinyTranscript says when MacWhisper watches it.",
        folder.display());
    let r = alert(mtm, "Add the work folder in MacWhisper", &text, &["Open MacWhisper", "Cancel"]);
    if r != NSAlertFirstButtonReturn {
        return;
    }
    copy_to_clipboard(&folder.to_string_lossy());
    let _ = Command::new("/usr/bin/open").args(["-b", MW_ID]).status();
    wait_for_watch(folder);
}

fn copy_to_clipboard(text: &str) {
    if let Ok(mut c) = Command::new("/usr/bin/pbcopy").stdin(std::process::Stdio::piped()).spawn() {
        if let Some(mut i) = c.stdin.take() {
            let _ = std::io::Write::write_all(&mut i, text.as_bytes());
        }
        let _ = c.wait();
    }
}

static WAITING: AtomicBool = AtomicBool::new(false);

/// Notify once MacWhisper watches `folder` (checked every 2 s for 20 minutes).
fn wait_for_watch(folder: PathBuf) {
    if WAITING.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(move || {
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(1200) {
            if macwhisper_watch(&folder) == Watch::Yes {
                crate::update::notify("MacWhisper watches the work folder", &folder.display().to_string());
                break;
            }
            std::thread::sleep(Duration::from_secs(2));
        }
        WAITING.store(false, Ordering::SeqCst);
    });
}

/// Set by a change of work folder just before the app restarts: show the MacWhisper steps once
/// it is up again.
fn watch_help_marker() -> PathBuf {
    common::settings_file().with_file_name("show-watch-help")
}

static AFTER_START: AtomicBool = AtomicBool::new(false);

/// Called by the menu's timer: once per run, after a change of work folder, show the steps for
/// MacWhisper if it does not watch the new folder.
pub fn after_start(mtm: MainThreadMarker) {
    if AFTER_START.swap(true, Ordering::SeqCst) {
        return;
    }
    let marker = watch_help_marker();
    if marker.exists() {
        let _ = std::fs::remove_file(&marker);
        if !matches!(macwhisper_watch(&common::recordings()), Watch::Yes | Watch::Unknown) {
            watch_work_folder(mtm);
        }
    }
}

fn apply(old: &Path, new: &Path, move_data: bool) -> Result<()> {
    if !crate::jobs::hold_and_wait(Duration::from_secs(600)) {
        bail!("a job is still running after 10 minutes; try again later");
    }
    if transcribing() {
        bail!("MacWhisper started transcribing; try again once it is done");
    }
    std::fs::create_dir_all(new)?;
    if move_data && old.is_dir() {
        move_contents(old, new)?;
    }
    common::set_work_folder(new)?;
    if !matches!(macwhisper_watch(new), Watch::Yes | Watch::Unknown) {
        let _ = std::fs::write(watch_help_marker(), "");
    }
    crate::update::notify("Work folder changed", &format!("Now {}", new.display()));
    std::thread::sleep(Duration::from_secs(4)); // let the notification show (refresh every 3 s)
    crate::update::restart_self();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tells_plain_bookmarks() {
        use objc2_foundation::{NSDataBase64EncodingOptions, NSURLBookmarkCreationOptions};
        let dir = std::env::temp_dir().join(format!("tt-bookmark-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let url = NSURL::fileURLWithPath_isDirectory(&ns(&dir.to_string_lossy()), true);
        let b64 = |o| url.bookmarkDataWithOptions_includingResourceValuesForKeys_relativeToURL_error(o, None, None)
            .unwrap().base64EncodedStringWithOptions(NSDataBase64EncodingOptions::empty()).to_string();
        assert!(plain_bookmark(&b64(NSURLBookmarkCreationOptions::empty())));
        assert!(!plain_bookmark(&b64(NSURLBookmarkCreationOptions::WithSecurityScope)));
        assert!(!plain_bookmark("not base64"));
        assert_eq!(with_slash(Path::new("/a/b")), "/a/b/");
        assert_eq!(with_slash(Path::new("/a/b/")), "/a/b/");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn moves_contents() {
        let root = std::env::temp_dir().join(format!("tt-move-{}", std::process::id()));
        let (a, b) = (root.join("a"), root.join("b"));
        std::fs::create_dir_all(a.join("processed")).unwrap();
        std::fs::write(a.join("x.m4a"), "1").unwrap();
        std::fs::write(a.join("processed/t.txt"), "2").unwrap();
        let mut moved = move_contents(&a, &b).unwrap();
        moved.sort();
        assert_eq!(moved, ["processed", "x.m4a"]);
        assert_eq!(std::fs::read_to_string(b.join("processed/t.txt")).unwrap(), "2");
        assert_eq!(std::fs::read_dir(&a).unwrap().count(), 0);
        // never over an existing item
        std::fs::write(a.join("x.m4a"), "3").unwrap();
        assert!(move_contents(&a, &b).is_err());
        assert_eq!(std::fs::read_to_string(b.join("x.m4a")).unwrap(), "1");
        let _ = std::fs::remove_dir_all(&root);
    }
}
