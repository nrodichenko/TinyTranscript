//! The pipeline's jobs, run by the menu bar app itself: one process for everything, instead of a
//! launch agent per job. Each job has its own thread and runs as its program would
//! (`zoom_fetch::job` etc.: same locks, logs, state and heartbeats).
//!
//! - Zoom: at start, then every 10 minutes.
//! - Drive: at start, then every minute.
//! - Speakers: at start, every 5 minutes, and when the recordings folder changes (at most every
//!   20 s), as the launch agent's WatchPaths did.
//!
//! A job whose login is not in the keychain (Zoom or Drive not set up) does not run.
//! "Check now" wakes a job at once. Pausing is kept in `.paused-jobs` (one name per line), so a job
//! stays paused after a restart. A job that panics is logged and runs again at its next turn; the
//! menu bar keeps running.

use pipeline_common as common;
use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};

struct Spec {
    label: &'static str,
    every: Duration,
    /// Also run when the recordings folder changes, at most this often.
    on_change: Option<Duration>,
    log: &'static str,
    /// Whether the job is set up (its login is in the keychain); if not, it does not run.
    ready: fn() -> bool,
    run: fn() -> i32,
}

const SPECS: [Spec; 3] = [
    Spec {
        label: "local.zoom-recordings",
        every: Duration::from_secs(600),
        on_change: None,
        log: "zoom.log",
        ready: || common::in_keychain("zoom-s2s-client-secret"),
        run: || zoom_fetch::job(vec!["zoom_fetch".into()]),
    },
    Spec {
        label: "local.drive-recordings",
        every: Duration::from_secs(60),
        on_change: None,
        log: "drive.log",
        ready: || common::in_keychain("google-drive-recordings"),
        run: || drive_fetch::job(vec!["drive_fetch".into()]),
    },
    Spec {
        label: "local.speaker-matching",
        every: Duration::from_secs(300),
        on_change: Some(Duration::from_secs(20)),
        log: "match.log",
        ready: || true,
        run: || match_speakers::job(vec!["match_speakers".into()]),
    },
];

#[derive(Default)]
struct State {
    /// no new runs (the work folder is being changed)
    hold: bool,
    running: HashSet<&'static str>,
    kicked: HashSet<String>,
    paused: HashSet<String>,
}

fn state() -> &'static Mutex<State> {
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(State { paused: load_paused(), ..State::default() }))
}

fn with<T>(f: impl FnOnce(&mut State) -> T) -> T {
    f(&mut state().lock().unwrap_or_else(|e| e.into_inner()))
}

fn paused_file() -> PathBuf {
    common::meta().join(".paused-jobs")
}

/// Jobs paused earlier; on the first start, those paused as launch agents (launchctl disable).
fn load_paused() -> HashSet<String> {
    if let Ok(text) = std::fs::read_to_string(paused_file()) {
        return text.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect();
    }
    let out = std::process::Command::new("/bin/launchctl")
        .args(["print-disabled", &format!("gui/{}", crate::status::uid())])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    SPECS.iter()
        .filter(|s| out.contains(&format!("\"{}\" => disabled", s.label)) || out.contains(&format!("\"{}\" => true", s.label)))
        .map(|s| s.label.to_string())
        .collect()
}

fn save_paused(paused: &HashSet<String>) {
    let mut v: Vec<&String> = paused.iter().collect();
    v.sort();
    let text: String = v.iter().map(|l| format!("{l}\n")).collect();
    let _ = common::write_atomic(&paused_file(), &text);
}

pub fn is_job(label: &str) -> bool {
    SPECS.iter().any(|s| s.label == label)
}

/// None: paused (not running and not scheduled); Some(true): running now.
pub fn running(label: &str) -> Option<bool> {
    with(|s| if s.paused.contains(label) { None } else { Some(s.running.contains(label)) })
}

pub fn paused(label: &str) -> bool {
    with(|s| s.paused.contains(label))
}

/// Pause: no more runs (one already running finishes). Resume: runs at once.
pub fn set_paused(label: &str, pause: bool) {
    with(|s| {
        if pause {
            s.paused.insert(label.to_string());
        } else {
            s.paused.remove(label);
            s.kicked.insert(label.to_string());
        }
        save_paused(&s.paused);
    });
}

/// Run a job now (if it is running already, once more right after).
pub fn kick(label: &str) {
    with(|s| s.kicked.insert(label.to_string()));
}

/// A cheap fingerprint of the recordings folder's top level (names, sizes, times).
fn folder_print() -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    if let Ok(rd) = std::fs::read_dir(common::recordings()) {
        let mut items: Vec<(String, u64, i128)> = rd.flatten().map(|e| {
            let md = e.metadata().ok();
            let t = md.as_ref().and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos() as i128);
            (e.file_name().to_string_lossy().into_owned(), md.map_or(0, |m| m.len()), t)
        }).collect();
        items.sort();
        items.hash(&mut h);
    }
    h.finish()
}

fn since(t: SystemTime) -> Duration {
    SystemTime::now().duration_since(t).unwrap_or_default()
}

fn worker(spec: &'static Spec) {
    // wall-clock times, so a job that came due while the Mac slept runs on waking
    let mut last_run: Option<SystemTime> = None;
    let mut seen = folder_print();
    let mut last_look = SystemTime::now();
    loop {
        std::thread::sleep(Duration::from_secs(1));
        let (paused, kicked) = with(|s| (s.paused.contains(spec.label) || s.hold, s.kicked.remove(spec.label)));
        if paused {
            continue;
        }
        let mut due = kicked || last_run.is_none_or(|t| since(t) >= spec.every);
        if let Some(min_gap) = spec.on_change {
            if !due && since(last_look) >= Duration::from_secs(2) {
                last_look = SystemTime::now();
                let now = folder_print();
                if now != seen && last_run.is_none_or(|t| since(t) >= min_gap) {
                    due = true;
                }
            }
        }
        if !due {
            continue;
        }
        if !(spec.ready)() {
            last_run = Some(SystemTime::now()); // not set up (Zoom on a Drive-only Mac): look again next turn
            continue;
        }
        // checked and marked running in one step: a hold (a change of work folder, an update)
        // that came meanwhile wins, and hold_and_wait sees every job that starts
        if !with(|s| !(s.hold || s.paused.contains(spec.label)) && s.running.insert(spec.label)) {
            continue;
        }
        last_run = Some(SystemTime::now());
        // what changes in the folder from now on, also while the job runs (MacWhisper writing a
        // transcript), starts the next run
        seen = folder_print();
        let outcome = catch_unwind(AssertUnwindSafe(spec.run));
        with(|s| s.running.remove(spec.label));
        if outcome.is_err() {
            eprintln!("recordings: the {} job crashed (panic above)", spec.label);
            common::log_to(&common::meta().join(spec.log), "ERROR the job crashed (details in launchd-errors.log); retrying next time");
        }
    }
}

/// Stop starting jobs and wait until none runs (up to `wait`); false if one is still running.
pub fn hold_and_wait(wait: Duration) -> bool {
    with(|s| s.hold = true);
    let t0 = SystemTime::now();
    while with(|s| !s.running.is_empty()) {
        if since(t0) > wait {
            return false;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    true
}

/// Like hold_and_wait, but only if no one else holds the jobs (a change of work folder); on a
/// timeout the hold is released again.
pub fn try_hold_and_wait(wait: Duration) -> bool {
    if !with(|s| if s.hold { false } else { s.hold = true; true }) {
        return false;
    }
    if hold_and_wait(wait) {
        return true;
    }
    release_hold();
    false
}

pub fn release_hold() {
    with(|s| s.hold = false);
}

/// Start every job's thread (once).
pub fn start() {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        let _ = state();
        for spec in &SPECS {
            std::thread::Builder::new()
                .name(spec.label.to_string())
                .spawn(move || worker(spec))
                .expect("starting a job thread");
        }
    });
}
