//! Pull new Zoom cloud recordings (audio only) into the work folder (~/Downloads/recordings unless changed in the menu) for MacWhisper.
//! Rust port of zoom_fetch.py; same flags, config, state, lock, cache folder and log lines.
//!
//! Each finished cloud recording that started after `zoom_since` (zoom_config.json) is downloaded
//! as soon as Zoom has the audio, and moved in whole to
//!     ~/Downloads/recordings/<YYYYMMDD>_-_<title>.m4a      (local date, spaces as '_')
//! where MacWhisper's watch folder transcribes it and match_speakers names the speakers.
//! <title> is the Zoom topic when it means something; personal-room meetings ("X's Zoom Meeting")
//! start as <date>_-_Zoom_call_HHMM and are titled from the transcript later (`title_pending`).
//!
//! Zoom access: a Server-to-Server OAuth app whose Account ID, Client ID and Client Secret are in
//! the login keychain (zoom-s2s-account-id, zoom-s2s-client-id, zoom-s2s-client-secret).

mod util;

use anyhow::{anyhow, Result};
use chrono::{DateTime, Local, NaiveDateTime, TimeZone, Utc};
use pipeline_common as common;
use serde_json::{json, Map, Value};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use util::{http_err, Exit, HttpError};

const HELP: &str = "Pull new Zoom cloud recordings (audio only) into the work folder (~/Downloads/recordings unless changed in the menu) for MacWhisper.

Run by the launch agent local.zoom-recordings every 10 minutes. Manual use:
    zoom_fetch              fetch what is new
    zoom_fetch --dry-run    list what would be fetched, download nothing
    zoom_fetch --since 2026-11-16T00:00:00Z    one-off: also fetch older recordings

options:
  -h, --help     show this help message and exit
  --dry-run      show what would be fetched
  --since SINCE  one-off run: fetch recordings that started after this UTC time (e.g.
                 2026-11-16T00:00:00Z) instead of zoom_since; files already downloaded by hand
                 are still skipped";

fn config_file() -> PathBuf {
    common::meta().join("zoom_config.json")
}
fn state_file() -> PathBuf {
    common::meta().join("zoom_state.json")
}
fn log_file() -> PathBuf {
    common::meta().join("zoom.log")
}
fn lock_file() -> PathBuf {
    common::meta().join(".zoom.lock")
}
/// Downloads land here first: outside the watch folder (MacWhisper must only ever see a complete
/// file, under its final name) and on the same disk, so the finished file is moved in whole.
fn partial() -> PathBuf {
    util::partial_dir("local.zoom-recordings")
}

fn default_config() -> Value {
    json!({
        "_help": {
            "zoom_user": "Zoom user whose cloud recordings are fetched (user ID or email)",
            "zoom_since": "only recordings that started after this UTC time are fetched",
            "generic_topics": "regexes for Zoom topics that say nothing about the meeting",
            "also_look_in": "folders, besides the work folder, where an already-downloaded copy (same GMT..._Recording prefix) means skip",
        },
        "zoom_user": "",
        // a new config starts from now: older recordings are not fetched
        "zoom_since": Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        "generic_topics": ["'s Zoom Meeting$", "Personal Meeting Room", "^Zoom Meeting$", "^Meeting$"],
        "also_look_in": ["~/Downloads"],
    })
}

fn log(msg: &str) {
    common::log_to(&log_file(), msg);
}

/// `2026-11-23T14:43:00Z` -> UTC time (Python: strptime "%Y-%m-%dT%H:%M:%SZ").
fn parse_utc(s: &str) -> Result<DateTime<Utc>> {
    NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%SZ")
        .map(|t| Utc.from_utc_datetime(&t))
        .map_err(|_| anyhow!("time data '{s}' does not match format '%Y-%m-%dT%H:%M:%SZ'"))
}

/// `d[key]` as a string, or Python's KeyError text.
fn req_str<'a>(d: &'a Value, key: &str) -> Result<&'a str> {
    d.get(key).and_then(Value::as_str).ok_or_else(|| anyhow!("'{key}'"))
}

fn str_list(v: &Value) -> Vec<String> {
    v.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default()
}

/// Python's `str(value)` for the dry-run line ("None" for a missing topic).
fn py_str(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => "None".into(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(b)) => if *b { "True".into() } else { "False".into() },
        Some(other) => other.to_string(),
    }
}

// ---------------------------------------------------------------- Zoom API

fn keychain(service: &str) -> Result<String> {
    common::keychain(service).ok_or_else(|| {
        // logged, so the menu bar shows it (launchd only keeps stderr in a file)
        let msg = format!("ERROR Zoom credentials missing: no keychain item '{service}'. See README.md (Zoom).");
        log(&msg);
        anyhow::Error::new(Exit(msg))
    })
}

struct Zoom {
    account: String,
    client: String,
    secret: String,
    token: Option<String>,
    expires: f64,
    oauth: String,
    api: String,
}

impl Zoom {
    fn new() -> Result<Zoom> {
        Ok(Zoom {
            account: keychain("zoom-s2s-account-id")?,
            client: keychain("zoom-s2s-client-id")?,
            secret: keychain("zoom-s2s-client-secret")?,
            token: None,
            expires: 0.0,
            oauth: "https://zoom.us/oauth/token".into(),
            api: "https://api.zoom.us/v2".into(),
        })
    }

    fn access_token(&mut self) -> Result<String> {
        if let Some(t) = &self.token {
            if common::now_ts() < self.expires - 60.0 {
                return Ok(t.clone());
            }
        }
        use base64::Engine;
        let basic = base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", self.client, self.secret));
        let url = format!("{}?{}", self.oauth,
                          util::urlencode(&[("grant_type", "account_credentials"), ("account_id", &self.account)]));
        let d: Value = util::agent(30).post(&url).set("Authorization", &format!("Basic {basic}"))
            .send_bytes(&[]).map_err(http_err)?.into_json()?;
        let token = req_str(&d, "access_token")?.to_string();
        let expires_in = match d.get("expires_in") {
            None => 3600.0,
            Some(Value::String(s)) => s.trim().parse::<i64>().map_err(|e| anyhow!("{e}"))? as f64,
            Some(v) => v.as_f64().ok_or_else(|| anyhow!("bad expires_in {v}"))?.trunc(),
        };
        self.token = Some(token.clone());
        self.expires = common::now_ts() + expires_in;
        Ok(token)
    }

    fn get(&mut self, path: &str, params: &[(&str, &str)]) -> Result<Value> {
        let url = format!("{}{}?{}", self.api, path, util::urlencode(params));
        let tok = self.access_token()?;
        Ok(util::agent(60).get(&url).set("Authorization", &format!("Bearer {tok}")).call().map_err(http_err)?.into_json()?)
    }

    /// Meetings with cloud recordings from `since` until tomorrow, in <=30-day API windows.
    fn recordings(&mut self, user: &str, since: DateTime<Utc>) -> Result<Vec<Value>> {
        let mut meetings = Vec::new();
        let mut start = since.date_naive();
        let end = Utc::now().date_naive() + chrono::Duration::days(1);
        while start <= end {
            let stop = std::cmp::min(start + chrono::Duration::days(29), end);
            let mut token = String::new();
            loop {
                let (from, to) = (start.format("%Y-%m-%d").to_string(), stop.format("%Y-%m-%d").to_string());
                let mut params = vec![("from", from.as_str()), ("to", to.as_str()), ("page_size", "300")];
                if !token.is_empty() {
                    params.push(("next_page_token", token.as_str()));
                }
                let d = self.get(&format!("/users/{}/recordings", quote(user)), &params)?;
                if let Some(Value::Array(m)) = d.get("meetings") {
                    meetings.extend(m.iter().cloned());
                }
                token = d.get("next_page_token").and_then(Value::as_str).unwrap_or("").to_string();
                if token.is_empty() {
                    break;
                }
            }
            start = stop + chrono::Duration::days(1);
        }
        Ok(meetings)
    }

    fn download(&mut self, url: &str, dest: &Path, expected_size: Option<u64>) -> Result<()> {
        let sep = if url.contains('?') { "&" } else { "?" };
        let full = format!("{url}{sep}{}", util::urlencode(&[("access_token", &self.access_token()?)]));
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        let name = dest.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let tmp = dest.with_file_name(format!("{name}.part"));
        let resp = util::agent(120).get(&full).call().map_err(http_err)?;
        {
            let mut f = fs::File::create(&tmp)?;
            std::io::copy(&mut resp.into_reader(), &mut f)?;
            f.flush()?;
        }
        let size = fs::metadata(&tmp)?.len();
        if let Some(exp) = expected_size.filter(|&e| e != 0) {
            if size != exp {
                fs::remove_file(&tmp)?;
                return Err(anyhow!("download of {name} is {size} bytes, Zoom says {exp}"));
            }
        }
        fs::rename(&tmp, dest)?;
        Ok(())
    }
}

/// `urllib.parse.quote(s)` (safe '/').
fn quote(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"_.-~/".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

// ---------------------------------------------------------------- naming

fn zoom_stem(recording_start: &str) -> Result<String> {
    Ok(format!("GMT{}_Recording", parse_utc(recording_start)?.format("%Y%m%d-%H%M%S")))
}

/// A copy with this GMT..._Recording prefix in the work folder (inbox, processed, originals) or one
/// of the configured folders.
fn already_have(stem: &str, folders: &[String]) -> Result<bool> {
    let work = [common::recordings(), common::processed(), common::processed_audio(), common::originals()];
    for dir in work.into_iter().chain(folders.iter().map(|f| util::expanduser(f))) {
        if util::dir_has_prefix(&dir, stem)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn clean_title(t: &str, max_chars: usize) -> String {
    let re = regex::Regex::new(r#"[\\/:*?"<>|\n\r\t]+"#).unwrap();
    let t = re.replace_all(t, " ");
    let t = t.trim_matches(util::py_space).trim_matches(|c| ".'\"`".contains(c)).trim_matches(util::py_space);
    let mut t = util::join_words(t);
    if t.chars().count() > max_chars {
        let head: String = t.chars().take(max_chars + 1).collect();
        let cut = match head.rfind(' ') {
            Some(i) => head[..i].to_string(),
            None => String::new(), // no space: cut at the limit
        };
        let chosen = if cut.chars().count() >= max_chars / 2 { cut } else { t.chars().take(max_chars).collect() };
        t = chosen.trim_end_matches(|c| " ,;-".contains(c)).to_string();
    }
    t
}

/// (title, pending). A meaningful Zoom topic is the title. Otherwise a temporary 'Zoom call HHMM':
/// match_speakers replaces it with a title written from the full transcript once there is one.
fn make_title(meeting: &Value, recording_start: &str, generic_topics: &[String]) -> Result<(String, bool)> {
    let topic = meeting.get("topic").and_then(Value::as_str).unwrap_or("").trim_matches(util::py_space);
    if !topic.is_empty() {
        let mut generic = false;
        for p in generic_topics {
            if regex::RegexBuilder::new(p).case_insensitive(true).build()?.is_match(topic) {
                generic = true;
                break;
            }
        }
        if !generic {
            return Ok((clean_title(topic, 80), false));
        }
    }
    let local = parse_utc(recording_start)?.with_timezone(&Local);
    Ok((format!("Zoom call {}", local.format("%H%M")), true))
}

fn name_taken_in(n: &str, dirs: &[PathBuf]) -> Result<bool> {
    let prefix = format!("{n}.");
    for d in dirs {
        if util::dir_has_prefix(d, &prefix)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn taken(n: &str) -> Result<bool> {
    name_taken_in(n, &[common::recordings(), common::processed(), common::processed_audio(), common::originals()])
}

/// The naming: local date, ' - ', title, with every space as '_' -> 20261118_-_Atlas_debrief.
/// A second recording with the same name on the same day gets its local start time: ..._1425, and
/// _1425_2, _1425_3 ... if that is taken too (never an existing file overwritten).
fn standard_name(recording_start: &str, title: &str) -> Result<String> {
    standard_name_in(recording_start, title, &taken)
}

fn standard_name_in(recording_start: &str, title: &str, taken: &dyn Fn(&str) -> Result<bool>) -> Result<String> {
    let local = parse_utc(recording_start)?.with_timezone(&Local);
    let base = format!("{} - {}", local.format("%Y%m%d"), title).replace(' ', "_");
    if !taken(&base)? {
        return Ok(base);
    }
    let timed = format!("{base}_{}", local.format("%H%M"));
    let (mut name, mut i) = (timed.clone(), 2);
    while taken(&name)? {
        name = format!("{timed}_{i}");
        i += 1;
    }
    Ok(name)
}

/// run() result when another run holds the lock: not a check of its own, so no heartbeat.
const BUSY: i32 = 75;

// ---------------------------------------------------------------- main

fn run(args: &util::Args, connect: &dyn Fn() -> Result<Zoom>) -> Result<i32> {
    let meta = common::meta();
    if !meta.is_dir() {
        fs::create_dir(&meta)?;
    }
    let Some(_lock) = common::try_lock(&lock_file())? else { return Ok(BUSY) };
    let dry_run = args.has("--dry-run");
    let cfg = common::load_config(&config_file(), &default_config(), !dry_run);
    let mut state: Map<String, Value> = match common::load_json(&state_file()) {
        Some(Value::Object(m)) => m,
        _ => Map::new(),
    };
    let since_s = match args.value("--since") {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => req_str(&cfg, "zoom_since")?.to_string(),
    };
    let since = parse_utc(&since_s)?;
    let generic = str_list(&cfg["generic_topics"]);
    let look_in = str_list(&cfg["also_look_in"]);
    let mut zoom = connect()?;
    let mut meetings = zoom.recordings(req_str(&cfg, "zoom_user")?, since)?;
    let now = util::utc_iso(Utc::now());
    let partial = partial();
    if !dry_run {
        fs::create_dir_all(&partial)?;
    }

    meetings.sort_by(|a, b| {
        let k = |m: &Value| m.get("start_time").and_then(Value::as_str).unwrap_or("").to_string();
        k(a).cmp(&k(b))
    });
    for m in &meetings {
        let files = m.get("recording_files").and_then(Value::as_array).cloned().unwrap_or_default();
        for a in &files {
            if a.get("recording_type").and_then(Value::as_str) != Some("audio_only")
                || a.get("file_type").and_then(Value::as_str) != Some("M4A")
            {
                continue;
            }
            let id = req_str(a, "id")?.to_string();
            let rs = req_str(a, "recording_start")?.to_string();
            if state.contains_key(&id) || parse_utc(&rs)? <= since {
                continue;
            }
            let stem = zoom_stem(&rs)?;
            if already_have(&stem, &look_in)? {
                if dry_run {
                    log(&format!("[dry run] {stem}: already downloaded by hand; would skip"));
                    continue;
                }
                state.insert(id, json!({"status": "already-had", "stem": stem, "at": now}));
                log(&format!("{stem}: already downloaded by hand; skipped"));
                common::save_json(&state_file(), &Value::Object(state.clone()))?;
                continue;
            }
            if a.get("status").and_then(Value::as_str) != Some("completed") {
                continue; // Zoom is still processing the audio; next run
            }
            if dry_run {
                let mb = a.get("file_size").and_then(Value::as_f64).unwrap_or(0.0) / 1e6;
                log(&format!("[dry run] would fetch {stem} ({}, {mb:.1} MB)", py_str(m.get("topic"))));
                continue;
            }
            let fetched = (|| -> Result<()> {
                let apath = partial.join(format!("{stem}.m4a"));
                let url = req_str(a, "download_url")?;
                zoom.download(url, &apath, a.get("file_size").and_then(Value::as_u64))?;
                let (title, pending) = make_title(m, &rs, &generic)?;
                let name = standard_name(&rs, &title)?;
                // never over a file that appeared since (the Drive job, or a name in another case)
                let name = common::place_new(&apath, &common::recordings(), &name, ".m4a")?;
                state.insert(id.clone(), json!({
                    "status": "fetched", "name": name, "title_pending": pending,
                    "topic": m.get("topic").cloned().unwrap_or(Value::Null),
                    "meeting_uuid": m.get("uuid").cloned().unwrap_or(Value::Null),
                    "recording_start": rs, "at": now,
                }));
                log(&format!("fetched '{name}.m4a'{}",
                             if pending { " (temporary name; titled from the full transcript later)" } else { "" }));
                Ok(())
            })();
            if let Err(e) = fetched {
                log(&format!("{stem}: ERROR {e:#}; will retry"));
            }
            common::save_json(&state_file(), &Value::Object(state.clone()))?;
        }
    }

    if !dry_run {
        util::clean_partial(&partial)?; // leftovers of interrupted runs
    }
    Ok(0)
}

/// One run of the job, as the program would do it; returns the exit code. `argv[0]` is the program
/// name, as in `std::env::args()`. The menu bar app calls this on its own timer.
pub fn job(argv: Vec<String>) -> i32 {
    let args = util::parse_args("zoom_fetch", "zoom_fetch [-h] [--dry-run] [--since SINCE]", HELP,
                                &[("--dry-run", false), ("--since", true)], &argv[1..]);
    let rc = match run(&args, &Zoom::new) {
        Ok(BUSY) => 0, // another run is checking; it touches the heartbeat
        Ok(rc) => {
            if rc == 0 && !argv.iter().any(|a| a == "--dry-run" || a == "--login") {
                util::note_recovery(&log_file());
                // time of the last good check, shown in the menu bar
                if let Err(e) = util::touch(&common::meta().join(".zoom-checked")) {
                    log(&format!("ERROR {e:#}"));
                    return 1;
                }
            }
            rc
        }
        Err(e) => {
            if let Some(x) = e.downcast_ref::<Exit>() {
                eprintln!("{x}");
            } else if let Some(h) = e.downcast_ref::<HttpError>() {
                let body = &h.body[..h.body.len().min(300)];
                log(&format!("Zoom API ERROR {}: {}", h.code, util::py_bytes_repr(body)));
            } else {
                log(&format!("ERROR {e:#}"));
            }
            1
        }
    };
    rc
}

/// The command line: run once and exit with its code.
pub fn main_with(argv: Vec<String>) -> ! {
    std::process::exit(job(argv))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn generic() -> Vec<String> {
        str_list(&default_config()["generic_topics"])
    }

    #[test]
    fn utc_parsing() {
        let t = parse_utc("2026-11-16T15:35:20Z").unwrap();
        assert_eq!(t.to_rfc3339(), "2026-11-16T15:35:20+00:00");
        assert!(parse_utc("2026-11-16T15:35:20").is_err());
        assert!(parse_utc("2026-11-16 15:35:20Z").is_err());
        assert_eq!(zoom_stem("2026-11-16T09:35:11Z").unwrap(), "GMT20261116-093511_Recording");
        assert_eq!(util::utc_iso(t), "2026-11-16T15:35:20+00:00");
    }

    #[test]
    fn titles() {
        let rs = "2026-11-16T15:35:20Z";
        let hhmm = parse_utc(rs).unwrap().with_timezone(&Local).format("%H%M").to_string();
        let personal = json!({"topic": "Jane Doe's Zoom Meeting"});
        assert_eq!(make_title(&personal, rs, &generic()).unwrap(), (format!("Zoom call {hhmm}"), true));
        for t in ["Zoom meeting", "meeting", "My Personal Meeting Room", "", "   "] {
            assert!(make_title(&json!({"topic": t}), rs, &generic()).unwrap().1, "{t}");
        }
        assert!(make_title(&json!({}), rs, &generic()).unwrap().1);
        let real = json!({"topic": " Acme / Globex: Weekly "});
        assert_eq!(make_title(&real, rs, &generic()).unwrap(), ("Acme Globex Weekly".to_string(), false));
        assert!(make_title(&real, rs, &["(".to_string()]).is_err());
    }

    #[test]
    fn clean_titles() {
        assert_eq!(clean_title("'Atlas debrief.'", 80), "Atlas debrief");
        assert_eq!(clean_title("a\tb\nc", 80), "a b c");
        let long = "word ".repeat(30);
        let t = clean_title(&long, 80);
        assert!(t.chars().count() <= 80 && t.ends_with("word"), "{t}");
        // a long title without spaces is cut to max_chars (the Python cut max_chars + 1 until 2026-11-24)
        assert_eq!(clean_title(&"x".repeat(100), 80).len(), 80);
        // a space at position max_chars: cut there
        let s = format!("{} {}", "a".repeat(80), "b".repeat(10));
        assert_eq!(clean_title(&s, 80), "a".repeat(80));
        // the only space is early: hard cut at max_chars
        let s = format!("ab {}", "c".repeat(100));
        assert_eq!(clean_title(&s, 80).chars().count(), 80);
        assert_eq!(clean_title("Встреча — итоги, ", 10), "Встреча —");
        assert_eq!(clean_title("`quoted` title.", 80), "quoted` title");
    }

    #[test]
    fn standard_names() {
        let root = std::env::temp_dir().join(format!("zoom-fetch-test-{}", std::process::id()));
        let dirs = [root.join("a"), root.join("b")];
        fs::create_dir_all(&dirs[1]).unwrap();
        fs::write(dirs[1].join("20261118_-_Atlas_debrief.txt"), "").unwrap();
        assert!(name_taken_in("20261118_-_Atlas_debrief", &dirs).unwrap());
        assert!(!name_taken_in("20261118_-_Atlas", &dirs).unwrap());
        assert!(already_have("20261118", &[dirs[1].to_string_lossy().into_owned(), "/nonexistent".into()]).unwrap());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn name_collisions_never_overwrite() {
        let rs = "2026-11-24T11:25:00Z";
        let local = parse_utc(rs).unwrap().with_timezone(&Local);
        let base = format!("{}_-_Call", local.format("%Y%m%d"));
        let timed = format!("{base}_{}", local.format("%H%M"));
        let have = std::cell::RefCell::new(Vec::<String>::new());
        let taken = |n: &str| Ok(have.borrow().iter().any(|h| h == n));
        for want in [base.clone(), timed.clone(), format!("{timed}_2"), format!("{timed}_3")] {
            let n = standard_name_in(rs, "Call", &taken).unwrap();
            assert_eq!(n, want);
            have.borrow_mut().push(n);
        }
    }

    #[test]
    fn default_config_file_as_python_writes_it() {
        // json.dumps(zoom_fetch.DEFAULT_CONFIG, indent=2, ensure_ascii=False) + "\n", from the Python
        // (zoom_user empty; zoom_since is the time the config is made)
        let mut cfg = default_config();
        assert!(parse_utc(cfg["zoom_since"].as_str().unwrap()).is_ok());
        cfg["zoom_since"] = json!("2026-01-01T00:00:00Z");
        assert_eq!(common::to_pretty(&cfg), include_str!("default_config.python.json"));
    }

    #[test]
    fn quoting() {
        assert_eq!(quote("AbCdEfGh12345678ijKlMn"), "AbCdEfGh12345678ijKlMn");
        assert_eq!(quote("a b@c.d/e"), "a%20b%40c.d/e");
        assert_eq!(py_str(None), "None");
    }
}

#[cfg(test)]
mod run_tests {
    use super::*;
    use std::io::Read;
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    type Seen = Arc<Mutex<Vec<String>>>;

    /// A one-thread HTTP server answering `reply(its base URL, method, path)` and recording
    /// "METHOD path Authorization-header".
    fn fake(reply: impl Fn(&str, &str, &str) -> (u16, Vec<u8>) + Send + 'static) -> (String, Seen) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen: Seen = Arc::new(Mutex::new(Vec::new()));
        let seen2 = seen.clone();
        let base2 = base.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                let mut req = Vec::new();
                let mut buf = [0u8; 8192];
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    let n = s.read(&mut buf).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    req.extend_from_slice(&buf[..n]);
                }
                let head = String::from_utf8_lossy(&req).to_string();
                let mut first = head.lines().next().unwrap_or("").split(' ');
                let (m, p) = (first.next().unwrap_or("").to_string(), first.next().unwrap_or("").to_string());
                let auth = head.lines().find(|l| l.to_ascii_lowercase().starts_with("authorization:")).unwrap_or("").to_string();
                seen2.lock().unwrap().push(format!("{m} {p} {auth}").trim_end().to_string());
                let (code, data) = reply(&base2, &m, &p);
                let _ = s.write_all(format!("HTTP/1.1 {code} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", data.len()).as_bytes());
                let _ = s.write_all(&data);
            }
        });
        (base, seen)
    }

    #[test]
    fn whole_run_against_fake_zoom() {
        let root = std::env::temp_dir().join(format!("zoom-fetch-run-{}", std::process::id()));
        let by_hand = root.join("by-hand");
        fs::create_dir_all(root.join("speaker-matching")).unwrap();
        fs::create_dir_all(&by_hand).unwrap();
        std::env::set_var("RECORDINGS_ROOT", &root);
        std::env::set_var("RECORDINGS_CACHE", root.join("cache"));

        let fmt = move |t: DateTime<Utc>| t.format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let day = Utc::now().date_naive().and_hms_opt(0, 0, 0).unwrap().and_utc() - chrono::Duration::days(1);
        let since = day - chrono::Duration::days(1);
        let at = move |h: i64, m: i64| fmt(day + chrono::Duration::hours(h) + chrono::Duration::minutes(m));
        let mut cfg = default_config();
        cfg["zoom_since"] = json!(fmt(since));
        cfg["also_look_in"] = json!([by_hand.to_string_lossy()]);
        common::save_json(&config_file(), &cfg).unwrap();
        fs::write(by_hand.join(format!("{}_audio.m4a", zoom_stem(&at(5, 0)).unwrap())), "").unwrap();

        let audio = b"m4a bytes".to_vec();
        let (base, seen) = {
            let audio = audio.clone();
            fake(move |b, m, p| {
                let rec = |id: &str, start: &str, status: &str, size: usize| json!({
                    "id": id, "recording_type": "audio_only", "file_type": "M4A", "status": status,
                    "recording_start": start, "file_size": size, "download_url": format!("{b}/rec/{id}")});
                match (m, p) {
                    ("POST", p) if p.starts_with("/oauth/token?grant_type=account_credentials&account_id=ACC") =>
                        (200, br#"{"access_token": "TOK", "expires_in": 3599}"#.to_vec()),
                    ("GET", p) if p.starts_with("/v2/users/USER/recordings?") && !p.contains("next_page_token") => (200, json!({
                        "next_page_token": "P2",
                        "meetings": [
                            {"topic": "Acme <> Globex Weekly", "uuid": "u1", "start_time": at(9, 0), "recording_files": [
                                {"id": "v1", "recording_type": "shared_screen_with_speaker_view", "file_type": "MP4",
                                 "status": "completed", "recording_start": at(9, 0)},
                                rec("a1", &at(9, 0), "completed", audio.len())]},
                            {"topic": "Jane Doe's Zoom Meeting", "uuid": "u2", "start_time": at(3, 0),
                             "recording_files": [rec("a2", &at(3, 0), "completed", audio.len())]},
                            {"topic": "Later", "start_time": at(10, 0), "recording_files": [rec("a3", &at(10, 0), "processing", 1)]},
                            {"topic": "Old", "start_time": fmt(since), "recording_files": [rec("a4", &fmt(since), "completed", 1)]},
                            {"topic": "By hand", "start_time": at(5, 0), "recording_files": [rec("a5", &at(5, 0), "completed", 1)]},
                            {"topic": "Short", "start_time": at(6, 0), "recording_files": [rec("a6", &at(6, 0), "completed", 999)]},
                        ]}).to_string().into_bytes()),
                    ("GET", p) if p.starts_with("/v2/users/USER/recordings?") => (200, json!({
                        "next_page_token": "",
                        "meetings": [{"topic": "Acme <> Globex Weekly", "start_time": at(11, 0),
                                      "recording_files": [rec("a7", &at(11, 0), "completed", audio.len())]}]}).to_string().into_bytes()),
                    ("GET", p) if p.starts_with("/rec/") && p.ends_with("?access_token=TOK") => (200, audio.clone()),
                    _ => (404, b"{}".to_vec()),
                }
            })
        };
        let connect = || -> Result<Zoom> {
            Ok(Zoom { account: "ACC".into(), client: "CID".into(), secret: "SEC".into(), token: None, expires: 0.0,
                      oauth: format!("{base}/oauth/token"), api: format!("{base}/v2") })
        };
        cfg["zoom_user"] = json!("USER");
        common::save_json(&config_file(), &cfg).unwrap();

        assert_eq!(run(&util::Args::default(), &connect).unwrap(), 0);

        let local = |s: &str| parse_utc(s).unwrap().with_timezone(&Local);
        let d1 = local(&at(9, 0)).format("%Y%m%d").to_string();
        let first = format!("{d1}_-_Acme_Globex_Weekly");
        let second = format!("{}_-_Acme_Globex_Weekly_{}", local(&at(11, 0)).format("%Y%m%d"), local(&at(11, 0)).format("%H%M"));
        let l2 = local(&at(3, 0));
        let personal = format!("{}_-_Zoom_call_{}", l2.format("%Y%m%d"), l2.format("%H%M"));
        for n in [&first, &second, &personal] {
            assert_eq!(fs::read(root.join(format!("{n}.m4a"))).unwrap(), audio, "{n}");
        }
        let state = common::load_json(&state_file()).unwrap();
        assert_eq!(state["a1"], json!({"status": "fetched", "name": first, "title_pending": false,
            "topic": "Acme <> Globex Weekly", "meeting_uuid": "u1", "recording_start": at(9, 0), "at": state["a1"]["at"]}));
        assert_eq!(state["a2"]["title_pending"], true);
        assert_eq!(state["a2"]["name"], json!(personal));
        assert_eq!(state["a7"]["meeting_uuid"], Value::Null);
        assert_eq!(state["a5"]["status"], "already-had");
        for id in ["a3", "a4", "a6", "v1"] {
            assert!(state.get(id).is_none(), "{id}");
        }
        let log_text = fs::read_to_string(log_file()).unwrap();
        let stem6 = zoom_stem(&at(6, 0)).unwrap();
        for line in [format!("fetched '{first}.m4a'\n"),
                     format!("fetched '{personal}.m4a' (temporary name; titled from the full transcript later)"),
                     format!("{}: already downloaded by hand; skipped", zoom_stem(&at(5, 0)).unwrap()),
                     format!("{stem6}: ERROR download of {stem6}.m4a is {} bytes, Zoom says 999; will retry", audio.len())] {
            assert!(log_text.contains(&line), "{line}\n{log_text}");
        }
        let seen = seen.lock().unwrap();
        assert_eq!(seen.iter().filter(|r| r.starts_with("POST /oauth/token")).count(), 1, "token reused: {seen:?}");
        assert!(seen.iter().any(|r| r.starts_with("POST /oauth/token") && r.ends_with("Basic Q0lEOlNFQw==")));
        assert!(seen.iter().any(|r| r.contains("next_page_token=P2") && r.ends_with("Bearer TOK")));
        assert!(fs::read_dir(root.join("cache/local.zoom-recordings")).unwrap().next().is_none());
        let _ = fs::remove_dir_all(&root);
    }
}
