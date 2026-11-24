//! What the menu shows: job states from launchd, log lines, MacWhisper and Claude status.
//! No AppKit here, so it can be tested on its own.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, UNIX_EPOCH};

use chrono::{Local, NaiveDateTime, TimeZone, Utc};
use pipeline_common::{friendly_time, last_line, now_ts};

pub const MEDIA_EXT: [&str; 12] =
    ["m4a", "mp3", "wav", "aac", "mp4", "mov", "ogg", "flac", "aiff", "caf", "webm", "m4v"];

pub fn uid() -> u32 {
    unsafe { libc::getuid() }
}

// ------------------------------------------------------------------ text helpers (by character)

/// The first `n` characters.
pub fn head(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Everything after the first `n` characters.
pub fn tail(s: &str, n: usize) -> String {
    s.chars().skip(n).collect()
}

/// `s` if it is at most `max` characters, else its first `max - 1` and "…".
pub fn shorten(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", head(s, max - 1))
    }
}

/// A log line starting with a date (`2026-11-24 ...`).
pub fn is_stamped(line: &str) -> bool {
    let h: Vec<char> = line.chars().take(4).collect();
    h.len() == 4 && h.iter().all(|c| c.is_ascii_digit())
}

fn mtime(p: &Path) -> Option<f64> {
    let m = std::fs::metadata(p).ok()?.modified().ok()?;
    Some(m.duration_since(UNIX_EPOCH).ok()?.as_secs_f64())
}

fn local_stamp(ts: f64) -> String {
    Local.timestamp_opt(ts as i64, 0).single().map(|t| t.format("%Y-%m-%d %H:%M").to_string()).unwrap_or_default()
}

// ------------------------------------------------------------------ launchd

fn output(cmd: &str, args: &[&str]) -> Option<(bool, String)> {
    let out = Command::new(cmd).args(args).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    Some((out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned()))
}

fn quiet(cmd: &str, args: &[&str]) {
    let _ = Command::new(cmd).args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status();
}

/// None: not loaded (or paused); Some(true): running now. The pipeline's own jobs run inside this
/// app (jobs.rs); anything else is asked of launchd.
pub fn job_running(label: &str) -> Option<bool> {
    if crate::jobs::is_job(label) {
        return crate::jobs::running(label);
    }
    let (ok, out) = output("/bin/launchctl", &["print", &format!("gui/{}/{label}", uid())])?;
    ok.then(|| out.contains("state = running"))
}

/// Disabled in launchd (paused from the menu: stays paused after a restart).
pub fn job_paused(label: &str) -> bool {
    if crate::jobs::is_job(label) {
        return crate::jobs::paused(label);
    }
    let out = output("/bin/launchctl", &["print-disabled", &format!("gui/{}", uid())]).map(|o| o.1).unwrap_or_default();
    out.contains(&format!("\"{label}\" => disabled")) || out.contains(&format!("\"{label}\" => true"))
}

/// Pause: disable the launch agent (kept across restarts) and unload it. Resume: the reverse.
pub fn set_job_paused(label: &str, paused: bool) {
    if crate::jobs::is_job(label) {
        return crate::jobs::set_paused(label, paused);
    }
    let target = format!("gui/{}/{label}", uid());
    if paused {
        quiet("/bin/launchctl", &["disable", &target]);
        quiet("/bin/launchctl", &["bootout", &target]);
    } else {
        quiet("/bin/launchctl", &["enable", &target]);
        let plist = pipeline_common::home().join("Library/LaunchAgents").join(format!("{label}.plist"));
        quiet("/bin/launchctl", &["bootstrap", &format!("gui/{}", uid()), &plist.to_string_lossy()]);
    }
}

/// Run a job now.
pub fn kick(label: &str) {
    if crate::jobs::is_job(label) {
        return crate::jobs::kick(label);
    }
    quiet("/bin/launchctl", &["kickstart", &format!("gui/{}/{label}", uid())]);
}

// ------------------------------------------------------------------ keychain (existence only)

thread_local! {
    static KEYCHAIN: RefCell<HashMap<String, (bool, f64)>> = RefCell::new(HashMap::new());
    static DURATIONS: RefCell<HashMap<(PathBuf, u128), f64>> = RefCell::new(HashMap::new());
}

/// Whether a keychain item exists (its secret is not read). Cached for a minute.
pub fn in_keychain(service: &str) -> bool {
    let now = now_ts();
    if let Some((ok, at)) = KEYCHAIN.with(|k| k.borrow().get(service).copied()) {
        if now - at < 60.0 {
            return ok;
        }
    }
    let ok = pipeline_common::in_keychain(service);
    KEYCHAIN.with(|k| k.borrow_mut().insert(service.to_string(), (ok, now)));
    ok
}

/// (connected, state to show if not): a source without its login has nothing to run or pause.
pub fn job_connected(label: &str, meta: &Path) -> (bool, &'static str) {
    if label == "local.zoom-recordings" && !in_keychain("zoom-s2s-client-secret") {
        return (false, "not set up");
    }
    if label == "local.drive-recordings"
        && (!in_keychain("google-drive-recordings") || last_line(&meta.join("drive.log")).contains("login expired"))
    {
        return (false, "not logged in");
    }
    (true, "")
}

// ------------------------------------------------------------------ heartbeats

/// "checked just now" / "checked 4 min ago" / "checked Today 09:10" from a heartbeat file.
pub fn checked_ago(beat: &Path) -> Option<String> {
    let ts = mtime(beat)?;
    let mins = ((now_ts() - ts) / 60.0).floor() as i64;
    Some(if mins < 1 {
        "checked just now".to_string()
    } else if mins < 60 {
        format!("checked {mins} min ago")
    } else {
        format!("checked {}", friendly_time(&local_stamp(ts)))
    })
}

// ------------------------------------------------------------------ MacWhisper

/// Length of a recording (ffprobe, or macOS's afinfo without Homebrew), cached per file and
/// modification time; 1 hour if unknown.
pub fn audio_seconds(path: &Path) -> f64 {
    let stamp = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .map(|t| t.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO).as_nanos())
        .unwrap_or(0);
    let key = (path.to_path_buf(), stamp);
    if let Some(d) = DURATIONS.with(|c| c.borrow().get(&key).copied()) {
        return d;
    }
    let p = path.to_string_lossy();
    let d = match pipeline_common::tool("ffprobe") {
        Some(ffprobe) => output(&ffprobe.to_string_lossy(), &["-v", "error", "-show_entries", "format=duration", "-of", "default=nw=1:nk=1", &p])
            .and_then(|(_, out)| out.trim().parse::<f64>().ok()),
        // "estimated duration: 1234.567 sec"
        None => output("/usr/bin/afinfo", &[&p]).and_then(|(_, out)| {
            out.lines().find_map(|l| l.trim().strip_prefix("estimated duration:"))
                .and_then(|r| r.split_whitespace().next()?.parse::<f64>().ok())
        }),
    }
    .unwrap_or(3600.0);
    DURATIONS.with(|c| c.borrow_mut().insert(key, d));
    d
}

fn has_ext(p: &Path, exts: &[&str]) -> bool {
    p.extension().map(|e| e.to_string_lossy().to_lowercase()).is_some_and(|e| exts.contains(&e.as_str()))
}

fn stem(p: &Path) -> String {
    p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}

fn hidden(p: &Path) -> bool {
    p.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.'))
}

/// Visible files directly in a folder (none if it is missing).
pub fn files_in(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.is_file() && !hidden(p)).collect())
        .unwrap_or_default()
}

/// Inbox audio with no transcript yet, oldest first: what MacWhisper is working on or has queued.
pub fn untranscribed(recordings: &Path, originals: &Path) -> Vec<(PathBuf, f64)> {
    let done: HashSet<String> = [recordings, originals]
        .iter()
        .flat_map(|d| files_in(d))
        .filter(|p| has_ext(p, &["txt"]))
        .map(|p| stem(&p))
        .collect();
    let mut todo: Vec<(PathBuf, f64)> = files_in(recordings)
        .into_iter()
        .filter(|p| has_ext(p, &MEDIA_EXT) && !done.contains(&stem(p)))
        .filter_map(|p| mtime(&p).map(|t| (p, t)))
        .collect();
    todo.sort_by(|a, b| a.1.total_cmp(&b.1));
    todo
}

/// Its last watch-folder session, from its database opened read-only: "YYYY-MM-DD HH:MM" local.
pub fn macwhisper_last_session(db: &Path) -> Option<String> {
    use rusqlite::{Connection, OpenFlags};
    let con = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX).ok()?;
    con.busy_timeout(Duration::from_secs(2)).ok()?;
    let v: Option<String> = con
        .query_row(
            "select max(dateCreated) from session where isFromWatchFolder = 1 and dateDeleted is null",
            [],
            |r| r.get(0),
        )
        .ok()?;
    utc_to_local(&v?)
}

/// `2026-11-24 09:02:58.334` (UTC) -> `2026-11-24 10:02` (local).
pub fn utc_to_local(s: &str) -> Option<String> {
    let s = head(s, 19).replace('T', " ");
    let naive = NaiveDateTime::parse_from_str(&s, "%Y-%m-%d %H:%M:%S").ok()?;
    Some(Utc.from_utc_datetime(&naive).with_timezone(&Local).format("%Y-%m-%d %H:%M").to_string())
}

pub struct MacWhisper {
    pub state: &'static str,
    pub detail: String,
    pub tip: Option<String>,
}

/// MacWhisper must be running for its watch folder to transcribe the inbox. Audio in the inbox
/// with no transcript yet is what it is working on (oldest first) or has queued.
pub fn macwhisper_status(recordings: &Path, originals: &Path, db: &Path) -> MacWhisper {
    let running = Command::new("/usr/bin/pgrep").args(["-x", "MacWhisper"]).stdout(Stdio::null())
        .stderr(Stdio::null()).status().map(|s| s.success()).unwrap_or(false);
    macwhisper_status_with(running, recordings, originals, db, audio_seconds)
}

pub fn macwhisper_status_with(
    running: bool,
    recordings: &Path,
    originals: &Path,
    db: &Path,
    seconds: impl Fn(&Path) -> f64,
) -> MacWhisper {
    let todo = untranscribed(recordings, originals);
    if !running {
        return MacWhisper {
            state: "not running",
            detail: if todo.is_empty() { String::new() } else { format!("{} waiting", todo.len()) },
            tip: Some("Nothing is transcribed until MacWhisper runs".into()),
        };
    }
    if let Some((first, first_t)) = todo.first() {
        let name = stem(first);
        let more = if todo.len() > 1 { format!(" +{}", todo.len() - 1) } else { String::new() };
        let short = format!("{}{more}", shorten(&name, 16));
        // stuck: the oldest file has waited longer than everything queued could take, plus 15 minutes
        let waited = now_ts() - first_t;
        if waited > todo.iter().map(|(p, _)| seconds(p)).sum::<f64>() + 900.0 {
            let since = Local.timestamp_opt(*first_t as i64, 0).single().map(|t| t.format("%H:%M").to_string()).unwrap_or_default();
            return MacWhisper {
                state: "stuck",
                detail: short,
                tip: Some(format!(
                    "{name} has waited since {since} without a transcript. Check MacWhisper (is it busy, or did the file fail?)."
                )),
            };
        }
        return MacWhisper { state: "transcribing", detail: short, tip: None };
    }
    let detail = macwhisper_last_session(db).map(|t| format!("last {}", friendly_time(&t))).unwrap_or_default();
    MacWhisper { state: "idle", detail, tip: None }
}

// ------------------------------------------------------------------ Claude

fn alive(pid: i64) -> bool {
    pid > 0 && pid <= i32::MAX as i64 && unsafe { libc::kill(pid as i32, 0) } == 0
}

/// (state, detail) of the pipeline's Claude calls: what it is doing now, or when it last answered
/// and whether that worked.
pub fn claude_status(busy_file: &Path, log_file: &Path) -> (&'static str, String) {
    if let Some(busy) = pipeline_common::load_json(busy_file) {
        if busy.get("pid").and_then(|p| p.as_i64()).is_some_and(alive) {
            let task = busy.get("task").and_then(|t| t.as_str()).unwrap_or("");
            let task = match task.strip_prefix("read name ") {
                Some(rest) => format!("reading {rest}"),
                None => task.to_string(),
            };
            let task = match task.strip_prefix("title ") {
                Some(rest) => format!("titling {rest}"),
                None => task,
            };
            return ("working", shorten(&task, 20));
        }
    }
    let line = last_line(log_file);
    if !is_stamped(&line) {
        return ("idle", String::new());
    }
    (if line.contains("ERROR") { "error" } else { "idle" }, format!("last {}", friendly_time(&head(&line, 16))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::SystemTime;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("menubar-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn set_mtime(p: &Path, secs_ago: u64) {
        let t = SystemTime::now() - Duration::from_secs(secs_ago);
        fs::File::options().write(true).open(p).unwrap().set_modified(t).unwrap();
    }

    #[test]
    fn text_helpers() {
        assert_eq!(shorten("short", 16), "short");
        assert_eq!(shorten("20261124_-_Telegram_1054", 16), "20261124_-_Tele…");
        assert_eq!(shorten("Ромашка-встреча-по-плану", 5), "Рома…");
        assert!(is_stamped("2026-11-24 10:00:00 x") && !is_stamped("Traceback"));
        assert_eq!(tail("2026-11-24 10:00:00 ERROR boom", 20), "ERROR boom");
    }

    #[test]
    fn utc_conversion() {
        let local = utc_to_local("2026-11-24 09:02:58.334").unwrap();
        let expect = Utc.with_ymd_and_hms(2026, 11, 24, 9, 2, 58).unwrap().with_timezone(&Local);
        assert_eq!(local, expect.format("%Y-%m-%d %H:%M").to_string());
        assert!(utc_to_local("garbage").is_none());
    }

    #[test]
    fn heartbeat() {
        let d = scratch("beat");
        let b = d.join(".zoom-checked");
        assert!(checked_ago(&b).is_none());
        fs::write(&b, "").unwrap();
        assert_eq!(checked_ago(&b).unwrap(), "checked just now");
        set_mtime(&b, 4 * 60 + 5);
        assert_eq!(checked_ago(&b).unwrap(), "checked 4 min ago");
        set_mtime(&b, 3 * 3600);
        assert!(checked_ago(&b).unwrap().starts_with("checked "));
    }

    #[test]
    fn macwhisper_queue_and_stuck() {
        let d = scratch("mw");
        let orig = d.join("speaker-matching/originals");
        fs::create_dir_all(&orig).unwrap();
        let db = d.join("none.sqlite");
        assert_eq!(macwhisper_status_with(true, &d, &orig, &db, |_| 60.0).state, "idle");
        let nr = macwhisper_status_with(false, &d, &orig, &db, |_| 60.0);
        assert_eq!((nr.state, nr.detail.as_str()), ("not running", ""));

        fs::write(d.join("20261124_-_Telegram_1054.m4a"), "").unwrap();
        fs::write(d.join("b.mp3"), "").unwrap();
        fs::write(d.join("done.wav"), "").unwrap();
        fs::write(orig.join("done.txt"), "").unwrap();
        fs::write(d.join(".hidden.m4a"), "").unwrap();
        fs::write(d.join("notes.pdf"), "").unwrap();
        set_mtime(&d.join("20261124_-_Telegram_1054.m4a"), 600);
        let s = macwhisper_status_with(true, &d, &orig, &db, |_| 60.0);
        assert_eq!((s.state, s.detail.as_str()), ("transcribing", "20261124_-_Tele… +1"));
        assert_eq!(macwhisper_status_with(false, &d, &orig, &db, |_| 60.0).detail, "2 waiting");
        set_mtime(&d.join("20261124_-_Telegram_1054.m4a"), 2000);
        let s = macwhisper_status_with(true, &d, &orig, &db, |_| 60.0);
        assert_eq!(s.state, "stuck");
        assert!(s.tip.unwrap().starts_with("20261124_-_Telegram_1054 has waited since "));
    }

    #[test]
    fn macwhisper_db_read_only() {
        let d = scratch("db");
        let db = d.join("main.sqlite");
        let con = rusqlite::Connection::open(&db).unwrap();
        con.execute_batch(
            "create table session (dateCreated text, isFromWatchFolder int, dateDeleted text);
             insert into session values ('2026-11-20 08:00:00.000', 1, null), ('2026-11-21 08:00:00.000', 0, null),
                                        ('2026-11-22 08:00:00.000', 1, '2026-11-22');",
        )
        .unwrap();
        drop(con);
        assert_eq!(macwhisper_last_session(&db), utc_to_local("2026-11-20 08:00:00"));
        let s = macwhisper_status_with(true, &d, &d, &db, |_| 1.0);
        assert_eq!(s.state, "idle");
        assert!(s.detail.starts_with("last 2026-11-20 "));
    }

    #[test]
    fn claude() {
        let d = scratch("claude");
        let (busy, log) = (d.join("claude_busy.json"), d.join("claude.log"));
        assert_eq!(claude_status(&busy, &log), ("idle", String::new()));
        fs::write(&log, "2026-11-20 10:00:01 title x: ok in 2.0s\n").unwrap();
        assert_eq!(claude_status(&busy, &log), ("idle", "last 2026-11-20 10:00".to_string()));
        fs::write(&log, "2026-11-20 10:00:01 title x: ok\n2026-11-20 11:00:01 title y: ERROR boom\n").unwrap();
        assert_eq!(claude_status(&busy, &log).0, "error");
        let me = std::process::id();
        fs::write(&busy, format!(r#"{{"task": "title 20261124_-_Zoom_meeting_long", "pid": {me}}}"#)).unwrap();
        assert_eq!(claude_status(&busy, &log), ("working", "titling 20261124_-_…".to_string()));
        fs::write(&busy, format!(r#"{{"task": "read name 'Ann'", "pid": {me}}}"#)).unwrap();
        assert_eq!(claude_status(&busy, &log), ("working", "reading 'Ann'".to_string()));
        fs::write(&busy, r#"{"task": "title x", "pid": 999999999}"#).unwrap();
        assert_eq!(claude_status(&busy, &log).0, "error");
    }
}
