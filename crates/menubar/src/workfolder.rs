//! "Change work folder…": pick a folder, optionally move transcripts, settings and logs there,
//! point MacWhisper's watch folder at it, and restart.
//!
//! MacWhisper (not sandboxed) keeps its watch folders in its preferences
//! (com.goodsnooze.MacWhisper, `watchedFolders`: JSON strings {"id", "isEnabled", "path",
//! "urlBookmark"}, the bookmark a plain base64 NSURL bookmark). It reads them when it starts, so:
//! MacWhisper is quit (only when it is not transcribing), its preferences are backed up
//! (speaker-matching/macwhisper-prefs-backup-<time>.plist, `defaults import` restores one), the
//! entry for the old work folder is pointed at the new one (other watched folders and settings are
//! left alone), and MacWhisper is started again.

use anyhow::{anyhow, bail, Context, Result};
use objc2::rc::Retained;
use objc2::{AllocAnyThread, MainThreadMarker};
use objc2_app_kit::{NSAlert, NSAlertFirstButtonReturn, NSAlertSecondButtonReturn, NSApplication, NSModalResponseOK,
                    NSOpenPanel, NSRunningApplication};
use objc2_foundation::{NSArray, NSString, NSURL, NSURLBookmarkCreationOptions, NSUserDefaults};
use pipeline_common as common;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;
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

/// MacWhisper's watch folders: (path with a trailing '/', enabled).
pub fn watched_folders() -> Vec<(String, bool)> {
    let Some(d) = mw_defaults() else { return vec![] };
    let Some(arr) = d.stringArrayForKey(&ns("watchedFolders")) else { return vec![] };
    arr.iter().filter_map(|s| serde_json::from_str::<Value>(&s.to_string()).ok())
        .filter_map(|v| Some((v["path"].as_str()?.to_string(), v["isEnabled"].as_bool().unwrap_or(true))))
        .collect()
}

/// Whether MacWhisper watches `folder` (enabled); None if its settings cannot be read.
pub fn macwhisper_watches(folder: &Path) -> Option<bool> {
    let w = watched_folders();
    if w.is_empty() && mw_defaults().is_none() {
        return None;
    }
    let want = with_slash(folder);
    Some(w.iter().any(|(p, on)| *on && with_slash(Path::new(p)) == want))
}

/// A plain bookmark of `folder`, base64 (as MacWhisper stores it).
fn bookmark(folder: &Path) -> Result<String> {
    let url = NSURL::fileURLWithPath_isDirectory(&ns(&folder.to_string_lossy()), true);
    let data = url.bookmarkDataWithOptions_includingResourceValuesForKeys_relativeToURL_error(
        NSURLBookmarkCreationOptions::empty(), None, None)
        .map_err(|e| anyhow!("bookmark for {}: {}", folder.display(), e.localizedDescription()))?;
    Ok(data.base64EncodedStringWithOptions(objc2_foundation::NSDataBase64EncodingOptions::empty()).to_string())
}

/// Point MacWhisper's watch folder `old` (or, if it has none for `old`, a new entry) at `new`.
/// Call with MacWhisper quit. Backs its preferences up into `backup_dir` first.
pub fn retarget_macwhisper(old: &Path, new: &Path, backup_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(backup_dir)?;
    let backup = backup_dir.join(format!("macwhisper-prefs-backup-{}.plist", chrono::Local::now().format("%Y%m%d-%H%M%S")));
    let st = Command::new("/usr/bin/defaults").args(["export", &mw_domain()]).arg(&backup).status()?;
    if !st.success() {
        bail!("could not back up MacWhisper's settings");
    }
    let d = mw_defaults().ok_or_else(|| anyhow!("cannot open MacWhisper's settings"))?;
    let current: Vec<String> = d.stringArrayForKey(&ns("watchedFolders")).map(|a| a.iter().map(|s| s.to_string()).collect()).unwrap_or_default();
    let (old_s, new_s) = (with_slash(old), with_slash(new));
    let mark = bookmark(new)?;
    let mut entries: Vec<String> = Vec::new();
    let mut replaced = false;
    for s in current {
        let mut v: Value = serde_json::from_str(&s).context("MacWhisper's watch folder setting changed format; nothing was written")?;
        let path = v["path"].as_str().map(|p| with_slash(Path::new(p)));
        if path.as_deref() == Some(old_s.as_str()) || path.as_deref() == Some(new_s.as_str()) {
            if replaced {
                continue; // one entry for the new folder
            }
            v["path"] = json!(new_s);
            v["urlBookmark"] = json!(mark);
            v["isEnabled"] = json!(true);
            replaced = true;
        }
        if v.get("id").is_none() || v.get("urlBookmark").is_none() {
            bail!("MacWhisper's watch folder setting changed format; nothing was written");
        }
        entries.push(v.to_string());
    }
    if !replaced {
        let id = uuid4();
        entries.push(json!({"id": id, "isEnabled": true, "path": new_s, "urlBookmark": mark}).to_string());
    }
    let arr = NSArray::from_retained_slice(&entries.iter().map(|s| ns(s)).collect::<Vec<_>>());
    unsafe { d.setObject_forKey(Some(&arr), &ns("watchedFolders")) };
    d.synchronize();
    if macwhisper_watches(new) != Some(true) {
        bail!("MacWhisper's settings did not take the new folder (backup: {})", backup.display());
    }
    Ok(())
}

fn uuid4() -> String {
    let mut b = [0u8; 16];
    for x in b.iter_mut() {
        *x = rand_byte();
    }
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02X}")).collect();
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

fn rand_byte() -> u8 {
    let mut b = [0u8; 1];
    let _ = std::fs::File::open("/dev/urandom").and_then(|mut f| std::io::Read::read_exact(&mut f, &mut b));
    b[0]
}

fn macwhisper_apps() -> Vec<Retained<NSRunningApplication>> {
    NSRunningApplication::runningApplicationsWithBundleIdentifier(&ns(MW_ID)).iter().collect()
}

/// Quit MacWhisper; true if it was running (and is now quit).
fn quit_macwhisper() -> Result<bool> {
    let apps = macwhisper_apps();
    if apps.is_empty() {
        return Ok(false);
    }
    for a in &apps {
        a.terminate();
    }
    let t0 = Instant::now();
    while !macwhisper_apps().is_empty() {
        if t0.elapsed() > Duration::from_secs(30) {
            bail!("MacWhisper did not quit; quit it yourself and choose the folder again");
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    Ok(true)
}

fn start_macwhisper() {
    let _ = Command::new("/usr/bin/open").args(["-g", "-b", MW_ID]).status();
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
        return;
    }
    if new_c.starts_with(&old_c) || old_c.starts_with(&new_c) {
        alert(mtm, "Choose another folder", "The new work folder cannot be inside the current one, or contain it.", &["OK"]);
        return;
    }
    if crate::status::macwhisper_status(&old, &common::originals(), &common::home().join("Library/Application Support/MacWhisper/Database/main.sqlite")).state == "transcribing" {
        alert(mtm, "MacWhisper is transcribing", "Change the work folder once it is done.", &["OK"]);
        return;
    }
    let has_data = new.join("speaker-matching").is_dir();
    let mw = if macwhisper_watches(&old) == Some(true) { "\n\nMacWhisper will be restarted to watch the new folder." } else { "" };
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

fn apply(old: &Path, new: &Path, move_data: bool) -> Result<()> {
    if !crate::jobs::hold_and_wait(Duration::from_secs(600)) {
        bail!("a job is still running after 10 minutes; try again later");
    }
    let mw_watched_old = macwhisper_watches(old) == Some(true);
    let mw_was_running = if mw_watched_old { quit_macwhisper()? } else { false };
    let result = (|| -> Result<()> {
        std::fs::create_dir_all(new)?;
        if move_data && old.is_dir() {
            move_contents(old, new)?;
        }
        common::set_work_folder(new)?;
        if mw_watched_old {
            retarget_macwhisper(old, new, &new.join("speaker-matching"))?;
        }
        Ok(())
    })();
    if mw_was_running {
        start_macwhisper();
    }
    result?;
    crate::update::notify("Work folder changed", &format!("Now {}", new.display()));
    std::thread::sleep(Duration::from_secs(4)); // let the notification show (refresh every 3 s)
    crate::update::restart_self();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_foundation::{NSData, NSDataBase64DecodingOptions, NSURLBookmarkResolutionOptions};

    #[test]
    fn bookmark_resolves_to_the_folder() {
        let dir = std::env::temp_dir().join(format!("tt-bookmark-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let b64 = bookmark(&dir).unwrap();
        let data = NSData::initWithBase64EncodedString_options(NSData::alloc(), &ns(&b64), NSDataBase64DecodingOptions::empty()).unwrap();
        let url = unsafe {
            NSURL::URLByResolvingBookmarkData_options_relativeToURL_bookmarkDataIsStale_error(
                &data, NSURLBookmarkResolutionOptions::empty(), None, std::ptr::null_mut())
        }.unwrap();
        let resolved = PathBuf::from(url.path().unwrap().to_string());
        assert_eq!(resolved.canonicalize().unwrap(), dir.canonicalize().unwrap());
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
