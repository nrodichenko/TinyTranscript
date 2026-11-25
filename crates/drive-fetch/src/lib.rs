//! Pull audio dropped into a Google Drive folder into the work folder (~/Downloads/recordings unless changed in the menu) for MacWhisper.
//! Rust port of drive_fetch.py; same flags, config, state, lock, cache folder and log lines.
//!
//! Share a voice note or recording to the Drive folder "Recordings inbox". Within a minute this job
//! downloads it, checks it against Drive's checksum, converts it to .m4a if it is not already
//! m4a/mp3/wav/aac (Telegram voice notes are .ogg), moves it whole into the recordings inbox and
//! then moves the Drive copy to the Drive trash (restorable there for 30 days).
//!
//! Name in the inbox: <YYYYMMDD>_-_<title>.<ext>, the date of the recording (from the file's own
//! metadata if it has one, else when it reached Drive). A name already in the standard form is kept;
//! other names are read by Claude Haiku (`name_reader`); names that say nothing become a temporary
//! <date>_-_Drive_HHMM (`title_pending`), titled later from the transcript.
//!
//! Google access: our own OAuth client (Desktop app) with the full Drive scope; client ID and
//! secret in the keychain as `google-drive-recordings-client`, the login (`--login`) as
//! `google-drive-recordings`. What this code does with that access is limited in
//! `guard::check()`: GETs, creating the inbox folder once, and setting trashed=true on a file it
//! has just listed in that folder and downloaded. No other write; HTTP DELETE is refused.

mod util;

use anyhow::{anyhow, Result};
use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, TimeZone, Utc};
use pipeline_common as common;
use regex::Regex;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;
use util::{http_err, Exit, HttpError};

const HELP: &str = "Pull audio dropped into a Google Drive folder into the work folder (~/Downloads/recordings unless changed in the menu) for MacWhisper.

Run by the launch agent local.drive-recordings every minute. Manual use:
    drive_fetch              fetch what is new
    drive_fetch --dry-run    list what would be fetched; download and trash nothing
    drive_fetch --login      sign in to Google in the browser (once, or after revoking access)

options:
  -h, --help    show this help message and exit
  --dry-run     show what would be fetched
  --login       sign in to Google in the browser
  --no-browser  with --login: only print the sign-in link";

const KEYCHAIN_ITEM: &str = "google-drive-recordings";
const CLIENT_ITEM: &str = "google-drive-recordings-client";
const SCOPE: &str = "https://www.googleapis.com/auth/drive";
/// macOS's own converter, used when ffmpeg (Homebrew) is not installed: it reads the usual audio
/// formats (m4a, mp3, wav, aiff, caf, flac, 3gp, mp4 audio) but not ogg/opus/webm/video.
const AFCONVERT: &str = "/usr/bin/afconvert";
const KEEP_AS_IS: [&str; 4] = [".m4a", ".mp3", ".wav", ".aac"];
const MEDIA_EXT: [&str; 15] = [".ogg", ".oga", ".opus", ".flac", ".amr", ".3gp", ".caf", ".aiff", ".aif",
                               ".wma", ".webm", ".mp4", ".mov", ".m4v", ".mkv"];
const DRIVE: &str = "https://www.googleapis.com/drive/v3";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const FOLDER_MIME: &str = "application/vnd.google-apps.folder";

fn config_file() -> PathBuf {
    common::meta().join("drive_config.json")
}
fn state_file() -> PathBuf {
    common::meta().join("drive_state.json")
}
fn log_file() -> PathBuf {
    common::meta().join("drive.log")
}
fn lock_file() -> PathBuf {
    common::meta().join(".drive.lock")
}
/// Downloads land here first: outside the watch folder, same disk, so the finished file moves in whole.
fn partial() -> PathBuf {
    util::partial_dir("local.drive-recordings")
}

fn default_config() -> Value {
    json!({
        "_help": {
            "folder_name": "Drive folder (in My Drive) that is watched; created on the first run",
            "folder_id": "filled in on the first run; set it to watch another folder",
            "trash_after_fetch": "move the Drive copy to the Drive trash once it is safely in the inbox",
            "name_reader": "'claude': Claude Haiku reads free-format file names for a date and title; 'none': only names in the standard form are kept",
        },
        "folder_name": "Recordings inbox",
        "folder_id": "",
        "trash_after_fetch": true,
        "name_reader": "claude",
    })
}

fn log(msg: &str) {
    common::log_to(&log_file(), msg);
}

fn save_config(cfg: &Value) -> Result<()> {
    common::save_json(&config_file(), cfg)
}

fn req_str<'a>(d: &'a Value, key: &str) -> Result<&'a str> {
    d.get(key).and_then(Value::as_str).ok_or_else(|| anyhow!("'{key}'"))
}

fn re(cell: &'static OnceLock<Regex>, pat: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pat).unwrap())
}

/// Characters not allowed in a file name, as the Python's `[\\/:*?"<>|]+`.
fn bad_chars() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    re(&R, r#"[\\/:*?"<>|]+"#)
}

/// Whether the text has a letter (Python `[^\W\d_]`).
fn has_letter(s: &str) -> bool {
    static R: OnceLock<Regex> = OnceLock::new();
    re(&R, r"[^\W\d_]").is_match(s)
}

// ---------------------------------------------------------------- Python path pieces

/// `PurePosixPath(s).name`.
fn py_name(s: &str) -> &str {
    s.rsplit('/').find(|p| !p.is_empty() && *p != ".").unwrap_or("")
}

/// `PurePosixPath(s).suffix` (Python 3.14: everything from the last dot, unless the name starts there).
fn py_suffix(s: &str) -> &str {
    let n = py_name(s);
    match n.rfind('.') {
        Some(i) if i > 0 => &n[i..],
        _ => "",
    }
}

/// `PurePosixPath(s).stem`.
fn py_stem(s: &str) -> &str {
    let n = py_name(s);
    match n.rfind('.') {
        Some(i) if i > 0 => &n[..i],
        _ => n,
    }
}

// ---------------------------------------------------------------- the request guard

/// The only writes this program may send to Drive. Kept separate so it can be tested on its own.
mod guard {
    use super::FOLDER_MIME;
    use serde_json::Value;
    use std::collections::HashMap;
    use std::fmt;

    #[derive(Debug, PartialEq)]
    pub struct Refused(pub String);

    impl fmt::Display for Refused {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(&self.0)
        }
    }
    impl std::error::Error for Refused {}

    /// Allowed: GET; POST of a new folder; PATCH that only sets trashed=true on a file listed in the
    /// inbox folder this run (`listed`: file id -> parents). Nothing else.
    pub fn check(method: &str, path: &str, body: Option<&Value>, folder_id: Option<&str>,
                 listed: &HashMap<String, Vec<String>>) -> Result<(), Refused> {
        let refuse = |m: &str| Err(Refused(m.to_string()));
        if method == "DELETE" {
            return refuse("HTTP DELETE is permanent in Drive and is never sent");
        }
        if method == "POST" {
            let ok = path == "/files" && matches!(body, Some(Value::Object(b))
                if !b.is_empty()
                    && b.get("mimeType").and_then(Value::as_str) == Some(FOLDER_MIME)
                    && b.keys().all(|k| ["name", "mimeType", "parents"].contains(&k.as_str())));
            if !ok {
                return refuse("the only POST allowed is creating the inbox folder");
            }
        }
        if method == "PATCH" {
            let fid = path.rsplit('/').next().unwrap_or("");
            let trash_only = body == Some(&serde_json::json!({"trashed": true}));
            let in_folder = match folder_id {
                Some(f) if !f.is_empty() => listed.get(fid).is_some_and(|p| p.iter().any(|x| x == f)),
                _ => false,
            };
            if !trash_only || !in_folder {
                return refuse("PATCH may only trash a file listed in the inbox folder");
            }
        }
        if !["GET", "POST", "PATCH"].contains(&method) {
            return Err(Refused(format!("{method} is not allowed")));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------- Google Drive

fn login_json() -> Result<Value> {
    let v = common::keychain(KEYCHAIN_ITEM).ok_or_else(|| anyhow::Error::new(Exit(format!(
        "Google login missing: no keychain item '{KEYCHAIN_ITEM}'. Run drive_fetch --login (README.md, Google Drive)."))))?;
    Ok(serde_json::from_str(&v)?)
}

struct Drive {
    cred: Value,
    /// the access token and when to get a new one (a minute before Google says it expires)
    token: Option<(String, std::time::Instant)>,
    /// file id -> parents, from list_inbox(); only these may be trashed
    listed: HashMap<String, Vec<String>>,
    api: String,
    token_url: String,
}

impl Drive {
    fn new() -> Result<Drive> {
        Ok(Drive { cred: login_json()?, token: None, listed: HashMap::new(), api: DRIVE.into(), token_url: TOKEN_URL.into() })
    }

    fn token(&mut self) -> Result<String> {
        if let Some((t, until)) = &self.token {
            if std::time::Instant::now() < *until {
                return Ok(t.clone());
            }
        }
        let form = [("client_id", req_str(&self.cred, "client_id")?), ("client_secret", req_str(&self.cred, "client_secret")?),
                    ("refresh_token", req_str(&self.cred, "refresh_token")?), ("grant_type", "refresh_token")];
        let resp = match util::agent(30).post(&self.token_url).send_form(&form) {
            Ok(r) => r,
            Err(e) => {
                let e = http_err(e);
                if let Some(h) = e.downcast_ref::<HttpError>() {
                    if h.code == 400 || h.code == 401 {
                        if h.body.windows(13).any(|w| w == b"invalid_grant") {
                            return Err(anyhow::Error::new(Exit("ERROR Google login expired or was revoked: menu bar 🎙 > \
                                \"Log in to Google Drive…\" (or drive_fetch --login)".into())));
                        }
                        // As in the Python, the body was read by this check, so the log shows b''.
                        return Err(anyhow::Error::new(HttpError { code: h.code, reason: h.reason.clone(), body: vec![] }));
                    }
                }
                return Err(e);
            }
        };
        let d: Value = resp.into_json()?;
        let t = req_str(&d, "access_token")?.to_string();
        let secs = d.get("expires_in").and_then(Value::as_u64).unwrap_or(3600).saturating_sub(60).max(30);
        self.token = Some((t.clone(), std::time::Instant::now() + Duration::from_secs(secs)));
        Ok(t)
    }

    /// Every Drive call goes through here, and through the guard first.
    fn request(&mut self, method: &str, path: &str, params: &[(&str, &str)], body: Option<&Value>,
               folder_id: Option<&str>) -> Result<ureq::Response> {
        guard::check(method, path, body, folder_id, &self.listed)?;
        let mut url = format!("{}{}", self.api, path);
        if !params.is_empty() {
            url += &format!("?{}", util::urlencode(params));
        }
        // a token that stopped working before its time (401): once more with a new one
        for attempt in 0..2 {
            let req = util::agent(120).request(method, &url).set("Authorization", &format!("Bearer {}", self.token()?));
            let r = match body {
                Some(b) => req.send_json(b),
                None => req.call(),
            };
            match r {
                Err(ureq::Error::Status(401, _)) if attempt == 0 => self.token = None,
                r => return r.map_err(http_err),
            }
        }
        unreachable!()
    }

    fn json(&mut self, method: &str, path: &str, params: &[(&str, &str)], body: Option<&Value>,
            folder_id: Option<&str>) -> Result<Value> {
        Ok(self.request(method, path, params, body, folder_id)?.into_json()?)
    }

    /// The inbox folder's id; with `create` false (dry run) None if it does not exist yet, and the
    /// config is not written.
    fn ensure_folder(&mut self, cfg: &mut Value, create: bool) -> Result<Option<String>> {
        if let Some(fid) = cfg.get("folder_id").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            return Ok(Some(fid.to_string()));
        }
        let folder_name = req_str(cfg, "folder_name")?.to_string();
        let name = folder_name.replace('\\', "\\\\").replace('\'', "\\'");
        let q = format!("name = '{name}' and mimeType = '{FOLDER_MIME}' and 'root' in parents and trashed = false");
        let found = self.json("GET", "/files", &[("q", &q), ("fields", "files(id,name)"), ("spaces", "drive")], None, None)?;
        let first = found.get("files").and_then(Value::as_array).and_then(|f| f.first()).cloned();
        if !create {
            return first.map(|f| req_str(&f, "id").map(str::to_string)).transpose();
        }
        let fid = if let Some(f) = first {
            let fid = req_str(&f, "id")?.to_string();
            log(&format!("watching the existing Drive folder '{folder_name}' ({fid})"));
            fid
        } else {
            let body = json!({"name": folder_name, "mimeType": FOLDER_MIME, "parents": ["root"]});
            let r = self.json("POST", "/files", &[("fields", "id")], Some(&body), None)?;
            let fid = req_str(&r, "id")?.to_string();
            log(&format!("created the Drive folder '{folder_name}' ({fid})"));
            fid
        };
        cfg["folder_id"] = Value::String(fid.clone());
        save_config(cfg)?;
        Ok(Some(fid))
    }

    fn list_inbox(&mut self, folder_id: &str) -> Result<Vec<Value>> {
        let mut files = Vec::new();
        let mut page: Option<String> = None;
        let q = format!("'{folder_id}' in parents and trashed = false");
        loop {
            let mut params = vec![
                ("q", q.as_str()),
                ("fields", "nextPageToken,files(id,name,mimeType,size,md5Checksum,createdTime,modifiedTime,parents)"),
                ("pageSize", "100"), ("spaces", "drive")];
            if let Some(p) = &page {
                params.push(("pageToken", p.as_str()));
            }
            let r = self.json("GET", "/files", &params, None, None)?;
            if let Some(Value::Array(f)) = r.get("files") {
                files.extend(f.iter().cloned());
            }
            page = r.get("nextPageToken").and_then(Value::as_str).filter(|s| !s.is_empty()).map(String::from);
            if page.is_none() {
                break;
            }
        }
        for f in &files {
            let parents = f.get("parents").and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|p| p.as_str().map(String::from)).collect()).unwrap_or_default();
            self.listed.insert(req_str(f, "id")?.to_string(), parents);
        }
        Ok(files)
    }

    fn download(&mut self, f: &Value, dest: &Path) -> Result<()> {
        use md5::Digest;
        let mut md5 = md5::Md5::new();
        let name = dest.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let tmp = dest.with_file_name(format!("{name}.part"));
        let r = self.request("GET", &format!("/files/{}", req_str(f, "id")?), &[("alt", "media")], None, None)?;
        {
            let mut reader = r.into_reader();
            let mut out = fs::File::create(&tmp)?;
            let mut buf = vec![0u8; 1 << 20];
            loop {
                let n = reader.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                out.write_all(&buf[..n])?;
                md5.update(&buf[..n]);
            }
            out.flush()?;
        }
        let size = fs::metadata(&tmp)?.len();
        if let Some(want) = drive_size(f)? {
            if want != size as i64 {
                fs::remove_file(&tmp)?;
                return Err(anyhow!("incomplete download ({size} of {} bytes)", py_str(f.get("size"))));
            }
        }
        if let Some(sum) = f.get("md5Checksum").filter(|v| util::truthy(v)) {
            let got: String = md5.finalize().iter().map(|b| format!("{b:02x}")).collect();
            if sum.as_str() != Some(got.as_str()) {
                fs::remove_file(&tmp)?;
                return Err(anyhow!("checksum does not match Drive's"));
            }
        }
        fs::rename(&tmp, dest)?;
        Ok(())
    }

    fn trash(&mut self, fid: &str, folder_id: &str) -> Result<()> {
        self.json("PATCH", &format!("/files/{fid}"), &[("fields", "id,trashed")], Some(&json!({"trashed": true})),
                  Some(folder_id))?;
        Ok(())
    }
}

/// Python's `str()` of a JSON scalar.
fn py_str(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => "None".into(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

/// Drive's `size` (a string in the API) when it is set (Python: `if f.get("size")`).
fn drive_size(f: &Value) -> Result<Option<i64>> {
    Ok(match f.get("size") {
        Some(Value::String(s)) if !s.is_empty() => {
            Some(s.trim().parse().map_err(|_| anyhow!("invalid literal for int() with base 10: '{s}'"))?)
        }
        Some(Value::Number(n)) => n.as_i64().filter(|&n| n != 0),
        _ => None,
    })
}

// ---------------------------------------------------------------- naming

/// Drive's `2026-11-24T04:48:46.123Z` (first 19 characters) as UTC.
fn parse_drive_time(s: &str) -> Result<DateTime<Utc>> {
    let head: String = s.chars().take(19).collect();
    NaiveDateTime::parse_from_str(&head, "%Y-%m-%dT%H:%M:%S")
        .map(|t| Utc.from_utc_datetime(&t))
        .map_err(|_| anyhow!("time data '{head}' does not match format '%Y-%m-%dT%H:%M:%S'"))
}

/// When the audio was recorded according to ffprobe's `creation_time` output, if usable.
fn parse_creation_time(stdout: &str) -> Option<DateTime<Utc>> {
    let t = stdout.trim_matches(util::py_space).lines().next().unwrap_or("");
    if !t.is_empty() && !t.starts_with("1970") && !t.starts_with("1904") {
        return parse_drive_time(&t.replace(' ', "T")).ok();
    }
    None
}

/// When the audio was recorded, if the file says (creation_time tag), else when it reached Drive.
fn recorded_at(path: &Path, fallback: DateTime<Utc>) -> DateTime<Utc> {
    let Some(ffprobe) = common::tool("ffprobe") else { return fallback };
    let mut cmd = Command::new(ffprobe);
    cmd.args(["-v", "error", "-show_entries", "format_tags=creation_time", "-of", "default=nw=1:nk=1"]).arg(path);
    match util::run_capture(&mut cmd, Duration::from_secs(60)) {
        Ok((_, out, _)) => match String::from_utf8(out) {
            Ok(s) => parse_creation_time(&s).unwrap_or(fallback),
            Err(_) => fallback,
        },
        Err(_) => fallback,
    }
}

fn taken_in(n: &str, dirs: &[PathBuf]) -> Result<bool> {
    let prefix = format!("{n}.");
    for d in dirs {
        if util::dir_has_prefix(d, &prefix)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn taken(n: &str) -> Result<bool> {
    taken_in(n, &[common::recordings(), common::processed(), common::processed_audio(), common::originals()])
}

fn valid_date(y: &str, m: &str, d: &str) -> bool {
    match (y.parse::<i32>(), m.parse::<u32>(), d.parse::<u32>()) {
        (Ok(y), Ok(m), Ok(d)) => (1..=9999).contains(&y) && NaiveDate::from_ymd_opt(y, m, d).is_some(),
        _ => false,
    }
}

/// '20261123 - встреча с Ромашка' / '20261123_-_Acme_call' / '2026-11-23 Acme call' ->
/// '20261123_-_<Title_with_underscores>'; None if the name does not start with a date and a title
/// with at least one letter (so a timestamp name like '20261124_103522' is not taken as a title).
fn standard_name(stem: &str) -> Option<String> {
    static R: OnceLock<Regex> = OnceLock::new();
    let m = re(&R, r"^(\d{4})[-._]?(\d{2})[-._]?(\d{2})(?:[\s_]*[-\x{2013}\x{2014}][\s_]*|[\s_]+)(.+)$").captures(stem)?;
    if !valid_date(&m[1], &m[2], &m[3]) {
        return None;
    }
    let title = bad_chars().replace_all(&m[4], " ").replace('_', " ");
    let title = util::join_words(&title);
    let title = cap_words(title.trim_matches(|c| " -.".contains(c)), TITLE_MAX);
    if !has_letter(&title) {
        return None;
    }
    Some(format!("{}{}{}_-_{}", &m[1], &m[2], &m[3], title.replace(' ', "_")))
}

/// The longest title kept from a Drive file name (as for a name read by Claude): file names stay
/// well under macOS's 255 bytes with the date, '_with_<people>' and a number added.
const TITLE_MAX: usize = 60;

/// `t` cut to at most `max` characters, at a space if one is in the second half.
fn cap_words(t: &str, max: usize) -> String {
    if t.chars().count() <= max {
        return t.to_string();
    }
    let head: String = t.chars().take(max + 1).collect();
    let cut = match head.rfind(' ') {
        Some(i) if head[..i].chars().count() >= max / 2 => head[..i].to_string(),
        _ => t.chars().take(max).collect(),
    };
    cut.trim_end_matches(|c| " ,;-.".contains(c)).to_string()
}

/// Why a name could not be read. `Soft` is the Python's RuntimeError/ValueError (fall back to the
/// transcript title); `Hard` is any other exception (the file is retried on the next run).
#[derive(Debug)]
enum NameErr {
    Soft(String),
    Hard(String),
}

fn read_name_prompt(local: DateTime<Local>) -> String {
    format!(
        "A recording was uploaded with the file name below (without extension). It was recorded on \
         {}. Reply with JSON only: {{\"date\": \"YYYYMMDD\" or null, \"title\": \
         string or null}}.\n\
         - title: what the recording is about, as the name says it (meeting subject, company, people), \
         keeping the name's own words and language, without the date, at most 60 characters. null if \
         the name says nothing about the content: recorder defaults ('Standard recording 13', 'New \
         Recording 3', 'Interview 001', 'audio_...', 'voice ...'), timestamps, ids, a street or place \
         name only.\n\
         - date: only if the name states or implies one ('23 ноября', '2026-11-23', 'yesterday' = the \
         day before the recording date); otherwise null.",
        local.format("%Y-%m-%d (%A)"))
}

/// Claude's reply -> ('YYYYMMDD' or None, title or None).
fn parse_name_reply(reply: &str) -> Result<(Option<String>, Option<String>), NameErr> {
    static R: OnceLock<Regex> = OnceLock::new();
    let data: Value = match re(&R, r"(?s)\{.*\}").find(reply) {
        Some(m) => serde_json::from_str(m.as_str()).map_err(|e| NameErr::Soft(e.to_string()))?,
        None => json!({}),
    };
    let date = data.get("date").and_then(Value::as_str).filter(|d| {
        d.len() == 8 && d.chars().all(|c| c.is_ascii_digit()) && valid_date(&d[..4], &d[4..6], &d[6..])
    }).map(String::from);
    let title = match data.get("title") {
        Some(Value::String(t)) => {
            let t = bad_chars().replace_all(t, " ").replace('_', " ");
            let t = util::join_words(&t);
            let t: String = t.trim_matches(|c| " -.".contains(c)).chars().take(60).collect();
            (!t.is_empty()).then_some(t)
        }
        // Python: `title or None` keeps a truthy non-string (a number, true, a list), which then
        // fails in make_name; the file is retried on the next run.
        Some(v) if util::truthy(v) => return Err(NameErr::Hard(format!("title is not text: {v}"))),
        _ => None,
    };
    Ok((date, title))
}

/// Ask Claude what a free-format file name says.
fn read_name(stem: &str, local: DateTime<Local>) -> Result<(Option<String>, Option<String>), NameErr> {
    let reply = common::claude::ask(&read_name_prompt(local), stem, &format!("read name '{stem}'"), 120)
        .map_err(|e| NameErr::Soft(format!("{e:#}")))?;
    parse_name_reply(&reply)
}

/// (name without extension, title_pending).
///
/// A name in the standard form '<YYYYMMDD>_-_<title>' (or close to it) is kept, normalised, with
/// its own date. Any other name with a letter in it is read by Claude (name_reader 'claude').
/// Without a title the file is saved as '<date>_-_Drive_HHMM' and titled from the full transcript
/// by match_speakers; the same happens if Claude cannot be reached.
fn make_name_with<F>(file_name: &str, when: DateTime<Utc>, name_reader: Option<&str>, reader: F,
                     taken: &dyn Fn(&str) -> Result<bool>) -> Result<(String, bool)>
where
    F: Fn(&str, DateTime<Local>) -> Result<(Option<String>, Option<String>), NameErr>,
{
    let plain = file_name.replace('/', " "); // Drive allows '/' in a name: 'Call 1/2'
    let stem = py_stem(&plain).trim_matches(util::py_space);
    let local = when.with_timezone(&Local);
    let mut base = standard_name(stem);
    if base.is_none() && name_reader == Some("claude") && has_letter(stem) {
        match reader(stem, local) {
            Ok((date, Some(title))) => {
                let b = format!("{}_-_{}", date.unwrap_or_else(|| local.format("%Y%m%d").to_string()), title.replace(' ', "_"));
                log(&format!("'{file_name}': Claude read the name as '{b}'"));
                base = Some(b);
            }
            Ok((_, None)) => {}
            Err(NameErr::Soft(e)) => {
                log(&format!("'{file_name}': could not read the name with Claude ({e}); titling from the transcript"))
            }
            Err(NameErr::Hard(e)) => return Err(anyhow!(e)),
        }
    }
    let pending = base.is_none();
    let base = base.unwrap_or_else(|| format!("{}_-_Drive_{}", local.format("%Y%m%d"), local.format("%H%M")));
    if !taken(&base)? {
        return Ok((base, pending));
    }
    // a second file with the same name: its time, then _2, _3 ... never an overwrite
    let timed = format!("{base}_{}", local.format("%H%M%S"));
    let (mut name, mut i) = (timed.clone(), 2);
    while taken(&name)? {
        name = format!("{timed}_{i}");
        i += 1;
    }
    Ok((name, pending))
}

fn make_name(file_name: &str, when: DateTime<Utc>, cfg: &Value) -> Result<(String, bool)> {
    // without Claude Code installed, a free-form name gets the temporary name (no error)
    let reader = cfg.get("name_reader").and_then(Value::as_str).filter(|r| *r != "claude" || common::claude::available());
    make_name_with(file_name, when, reader, read_name, &taken)
}

fn is_media(f: &Value) -> Result<bool> {
    let mt = f.get("mimeType").and_then(Value::as_str).unwrap_or("");
    if mt.starts_with("application/vnd.google-apps") {
        return Ok(false);
    }
    let ext = py_suffix(req_str(f, "name")?).to_lowercase();
    Ok(mt.starts_with("audio/") || mt.starts_with("video/")
        || KEEP_AS_IS.contains(&ext.as_str()) || MEDIA_EXT.contains(&ext.as_str()))
}

// ---------------------------------------------------------------- login

fn token_urlsafe(n: usize) -> String {
    use base64::Engine;
    use rand::RngCore;
    let mut b = vec![0u8; n];
    rand::rngs::OsRng.fill_bytes(&mut b);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

/// `json.dumps` of a flat object of strings (", " and ": " separators, ASCII only).
fn py_dumps_flat(pairs: &[(&str, &str)]) -> String {
    let esc = |s: &str| {
        let mut o = String::from("\"");
        for c in s.chars() {
            match c {
                '"' => o.push_str("\\\""),
                '\\' => o.push_str("\\\\"),
                '\n' => o.push_str("\\n"),
                '\r' => o.push_str("\\r"),
                '\t' => o.push_str("\\t"),
                c if (c as u32) < 0x20 || (c as u32) > 0x7e => {
                    let mut buf = [0u16; 2];
                    for u in c.encode_utf16(&mut buf) {
                        o.push_str(&format!("\\u{u:04x}"));
                    }
                }
                c => o.push(c),
            }
        }
        o.push('"');
        o
    };
    let items: Vec<String> = pairs.iter().map(|(k, v)| format!("{}: {}", esc(k), esc(v))).collect();
    format!("{{{}}}", items.join(", "))
}

/// The query parameters of a request path, first value of each, blank values dropped (parse_qs).
fn query_params(path: &str) -> HashMap<String, String> {
    let mut q = HashMap::new();
    let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
    let query = query.split('#').next().unwrap_or("");
    for (k, v) in url::form_urlencoded::parse(query.as_bytes()) {
        if !v.is_empty() {
            q.entry(k.into_owned()).or_insert_with(|| v.into_owned());
        }
    }
    q
}

/// Browser sign-in (OAuth with PKCE and a one-off loopback address), then the login goes to the keychain.
fn oauth_login(open_browser: bool) -> Result<()> {
    use sha2::{Digest, Sha256};
    use std::net::TcpListener;
    let raw = common::keychain(CLIENT_ITEM).ok_or_else(|| anyhow::Error::new(Exit(format!(
        "no keychain item '{CLIENT_ITEM}' with the OAuth client ID and secret; see README.md"))))?;
    let client: Value = serde_json::from_str(&raw)?;
    let (client_id, client_secret) = (req_str(&client, "client_id")?.to_string(), req_str(&client, "client_secret")?.to_string());
    let verifier = token_urlsafe(64);
    let challenge = {
        use base64::Engine;
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
    };
    let expect = token_urlsafe(16);
    let mut got: HashMap<String, String> = HashMap::new();

    let listener = TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let redirect = format!("http://127.0.0.1:{}/", listener.local_addr()?.port());
    let url = format!("https://accounts.google.com/o/oauth2/v2/auth?{}", util::urlencode(&[
        ("client_id", &client_id), ("redirect_uri", &redirect), ("response_type", "code"), ("scope", SCOPE),
        ("access_type", "offline"), ("prompt", "consent"), ("state", &expect),
        ("code_challenge", &challenge), ("code_challenge_method", "S256")]));
    println!("Opening the Google sign-in in your browser. If it does not open, visit:\n{url}");
    if open_browser {
        let _ = Command::new("/usr/bin/open").arg(&url).status();
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(600);
    while !got.contains_key("code") && !got.contains_key("error") && std::time::Instant::now() < deadline {
        let mut stream = match listener.accept() {
            Ok((s, _)) => s,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        stream.set_nonblocking(false)?;
        stream.set_read_timeout(Some(Duration::from_secs(30)))?;
        let mut req = Vec::new();
        let mut buf = [0u8; 4096];
        while !req.windows(4).any(|w| w == b"\r\n\r\n") && req.len() < 65536 {
            match stream.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => req.extend_from_slice(&buf[..n]),
            }
        }
        let text = String::from_utf8_lossy(&req);
        let mut parts = text.lines().next().unwrap_or("").split(' ');
        let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or("/"));
        if method != "GET" {
            let _ = stream.write_all(b"HTTP/1.0 501 Unsupported method\r\nConnection: close\r\n\r\n");
            continue;
        }
        let q = query_params(path);
        if q.get("state").map(String::as_str).unwrap_or("") == expect {
            got.extend(q);
        }
        let msg = if got.contains_key("code") {
            "signed in. You can close this tab.".to_string()
        } else {
            format!("sign-in did not complete: {}", got.get("error").map(String::as_str).unwrap_or("no code"))
        };
        let _ = stream.write_all(format!("HTTP/1.0 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\r\n\
                                          <p>TinyTranscript: {msg}</p>").as_bytes());
    }
    drop(listener);
    let Some(code) = got.get("code").cloned() else {
        return Err(anyhow::Error::new(Exit(format!("sign-in did not complete: {}",
                                                    got.get("error").map(String::as_str).unwrap_or("timed out")))));
    };
    let tok: Value = util::agent(30).post(TOKEN_URL).send_form(&[
        ("code", &code), ("client_id", &client_id), ("client_secret", &client_secret), ("redirect_uri", &redirect),
        ("grant_type", "authorization_code"), ("code_verifier", &verifier)]).map_err(http_err)?.into_json()?;
    let scopes = tok.get("scope").and_then(Value::as_str).unwrap_or("");
    let refresh = tok.get("refresh_token").and_then(Value::as_str);
    let Some(refresh) = refresh.filter(|_| scopes.split(util::py_space).any(|s| s == SCOPE)) else {
        return Err(anyhow::Error::new(Exit("Google did not grant Drive access (tick the Drive box on the consent screen)".into())));
    };
    let cred = py_dumps_flat(&[("type", "authorized_user"), ("client_id", &client_id),
                                ("client_secret", &client_secret), ("refresh_token", refresh)]);
    common::keychain_store(KEYCHAIN_ITEM, &cred)?;
    log(&format!("signed in to Google Drive; login stored in the keychain as '{KEYCHAIN_ITEM}'"));
    Ok(())
}

// ---------------------------------------------------------------- main

fn trash_note(e: &anyhow::Error) -> String {
    format!("{e:#}")
}

fn run(args: &util::Args, connect: &dyn Fn() -> Result<Drive>) -> Result<i32> {
    if args.has("--login") {
        oauth_login(!args.has("--no-browser"))?;
        return Ok(-1); // Python returns None here: exit 0, but no heartbeat
    }
    let meta = common::meta();
    if !meta.is_dir() {
        fs::create_dir(&meta)?;
    }
    let Some(_lock) = common::try_lock(&lock_file())? else { return Ok(BUSY) };
    let dry_run = args.has("--dry-run");
    let mut cfg = common::load_config(&config_file(), &default_config(), !dry_run);
    let mut state: Map<String, Value> = match common::load_json(&state_file()) {
        Some(Value::Object(m)) => m,
        _ => Map::new(),
    };
    let save_state = |s: &Map<String, Value>| common::save_json(&state_file(), &Value::Object(s.clone()));
    let mut drive = connect()?;
    let Some(folder) = drive.ensure_folder(&mut cfg, !dry_run)? else {
        log(&format!("[dry run] no Drive folder '{}' yet; a real run creates it", req_str(&cfg, "folder_name")?));
        return Ok(0);
    };
    let mut files = drive.list_inbox(&folder)?;
    let now_t = Utc::now();
    let now = util::utc_iso(now_t);
    let partial = partial();
    if !dry_run {
        fs::create_dir_all(&partial)?;
    }
    let trash_after = cfg.get("trash_after_fetch").map(util::truthy).ok_or_else(|| anyhow!("'trash_after_fetch'"))?;

    files.sort_by(|a, b| {
        let k = |f: &Value| f.get("createdTime").and_then(Value::as_str).unwrap_or("").to_string();
        k(a).cmp(&k(b))
    });
    for f in &files {
        let fid = req_str(f, "id")?.to_string();
        let fname = req_str(f, "name")?.to_string();
        let entry = state.get(&fid).cloned().unwrap_or_else(|| json!({}));
        let status = entry.get("status").and_then(Value::as_str).unwrap_or("");
        if status == "fetched" {
            // Fetched on an earlier run but the trash call failed: only the trash is retried.
            if trash_after && !dry_run {
                match drive.trash(&fid, &folder) {
                    Ok(()) => {
                        state[&fid]["trashed"] = Value::Bool(true);
                        log(&format!("'{fname}': moved to the Drive trash"));
                    }
                    Err(e) => log(&format!("'{fname}': ERROR moving to the Drive trash: {}", trash_note(&e))),
                }
                save_state(&state)?;
            }
            continue;
        }
        let changed_at = f.get("modifiedTime").and_then(Value::as_str).unwrap_or("").to_string();
        if status == "unconvertible"
            && entry.get("modifiedTime").and_then(Value::as_str) == Some(changed_at.as_str())
            && entry.get("ffmpeg").and_then(Value::as_bool) == Some(common::tool("ffmpeg").is_some()) {
            continue; // failed to convert; tried again once the file changes in Drive or ffmpeg is installed
        }
        let mime = f.get("mimeType").cloned().unwrap_or(Value::Null);
        if mime.as_str() == Some(FOLDER_MIME) {
            continue;
        }
        if !is_media(f)? {
            if dry_run {
                log(&format!("[dry run] '{fname}': not audio or video ({}); would leave it", py_str(Some(&mime))));
                continue;
            }
            if status != "skipped" {
                state.insert(fid.clone(), json!({"status": "skipped", "drive_name": fname, "mime": mime, "at": now}));
                log(&format!("'{fname}': not audio or video ({}); left in Drive", py_str(Some(&mime))));
                save_state(&state)?;
            }
            continue;
        }
        let changed = match f.get("modifiedTime").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            Some(m) => m,
            None => req_str(f, "createdTime")?,
        };
        if now_t - parse_drive_time(changed)? < chrono::Duration::seconds(20) {
            continue; // just arrived; next run
        }
        if dry_run {
            let size = match f.get("size") {
                None => 0,
                Some(Value::String(s)) => s.trim().parse::<i64>().map_err(|e| anyhow!("{e}"))?,
                Some(v) => v.as_i64().ok_or_else(|| anyhow!("bad size {v}"))?,
            };
            log(&format!("[dry run] would fetch '{fname}' ({:.1} MB)", size as f64 / 1e6));
            continue;
        }
        let suffix = match py_suffix(&fname.replace('/', " ")).to_lowercase() {
            s if s.is_empty() => ".audio".to_string(),
            s => s,
        };
        let fetched = (|| -> Result<(), FetchErr> {
            let raw = partial.join(format!("{fid}{suffix}"));
            drive.download(f, &raw)?;
            let when = recorded_at(&raw, parse_drive_time(req_str(f, "createdTime")?)?);
            // converted before it is named: a file that cannot be converted costs no Claude call
            let ready = if KEEP_AS_IS.contains(&suffix.as_str()) {
                raw.clone()
            } else {
                let ready = partial.join(format!("{fid}.m4a"));
                let r = convert(&raw, &ready);
                let _ = fs::remove_file(&raw);
                r?;
                ready
            };
            let (name, pending) = match make_name(&fname, when, &cfg) {
                Ok(n) => n,
                Err(e) => {
                    let _ = fs::remove_file(&ready);
                    return Err(e.into());
                }
            };
            let ext = if ready == raw { suffix.clone() } else { ".m4a".to_string() };
            // whole file into the watch folder at once, never over a file that appeared since (the
            // Zoom job, or a name in another case)
            let name = common::place_new(&ready, &common::recordings(), &name, &ext)?;
            let final_path = common::recordings().join(format!("{name}{ext}"));
            state.insert(fid.clone(), json!({
                "status": "fetched", "name": name, "title_pending": pending, "drive_name": fname,
                "recorded": util::utc_iso(when), "at": now, "trashed": false,
            }));
            save_state(&state)?;
            log(&format!("fetched '{fname}' -> '{}'{}", final_path.file_name().unwrap_or_default().to_string_lossy(),
                         if pending { " (temporary name; titled from the full transcript later)" } else { "" }));
            Ok(())
        })();
        match fetched {
            Ok(()) => {}
            Err(FetchErr::Convert(stderr)) => {
                // the same file converts no better next time: wait until it changes or ffmpeg is installed
                let tail = &stderr[stderr.len().saturating_sub(300)..];
                let ffmpeg = common::tool("ffmpeg").is_some();
                state.insert(fid.clone(), json!({
                    "status": "unconvertible", "drive_name": fname, "modifiedTime": changed_at,
                    "ffmpeg": ffmpeg, "at": now, "error": util::py_bytes_repr(tail),
                }));
                save_state(&state)?;
                log(&format!("'{fname}': ERROR converting audio: {}; left in Drive, not fetched again until it changes{}",
                             util::py_bytes_repr(tail), if ffmpeg { "" } else { " or ffmpeg is installed (brew install ffmpeg)" }));
                continue;
            }
            Err(FetchErr::Other(e)) => {
                log(&format!("'{fname}': ERROR {e:#}; will retry"));
                continue;
            }
        }
        if trash_after {
            match drive.trash(&fid, &folder) {
                Ok(()) => {
                    state[&fid]["trashed"] = Value::Bool(true);
                    log(&format!("'{fname}': moved to the Drive trash"));
                }
                Err(e) => log(&format!("'{fname}': ERROR moving to the Drive trash: {}; will retry", trash_note(&e))),
            }
            save_state(&state)?;
        }
    }

    if !dry_run {
        util::clean_partial(&partial)?; // leftovers of interrupted runs
    }
    Ok(0)
}

/// run() result when another run holds the lock: not a check of its own, so no heartbeat.
const BUSY: i32 = 75;

enum FetchErr {
    Convert(Vec<u8>),
    Other(anyhow::Error),
}

impl<E: Into<anyhow::Error>> From<E> for FetchErr {
    fn from(e: E) -> Self {
        FetchErr::Other(e.into())
    }
}

/// To AAC .m4a with ffmpeg if installed, else with macOS's afconvert; a failed conversion gives
/// its stderr.
fn convert(raw: &Path, ready: &Path) -> Result<(), FetchErr> {
    let (prog, mut cmd) = match common::tool("ffmpeg") {
        Some(ffmpeg) => {
            let mut cmd = Command::new(&ffmpeg);
            cmd.args(["-v", "error", "-y", "-i"]).arg(raw).args(["-vn", "-c:a", "aac", "-b:a", "96k"]).arg(ready);
            (ffmpeg.display().to_string(), cmd)
        }
        None => {
            let mut cmd = Command::new(AFCONVERT);
            cmd.args(["-f", "m4af", "-d", "aac", "-b", "96000"]).arg(raw).arg(ready);
            (AFCONVERT.to_string(), cmd)
        }
    };
    match util::run_capture(&mut cmd, Duration::from_secs(3600)) {
        Ok((st, _, mut err)) if !st.success() => {
            if prog == AFCONVERT {
                err.extend_from_slice(b" (macOS cannot convert this format; `brew install ffmpeg` can)");
            }
            Err(FetchErr::Convert(err))
        }
        Ok(_) => Ok(()),
        Err(e) if e.to_string() == "timeout" => Err(FetchErr::Other(anyhow!(
            "Command '{prog} ... {}' timed out after 3600 seconds", ready.display()))),
        Err(e) => Err(FetchErr::Other(anyhow!("{e}: '{prog}'"))),
    }
}

/// One run of the job, as the program would do it; returns the exit code. `argv[0]` is the program
/// name, as in `std::env::args()`. The menu bar app calls this on its own timer.
pub fn job(argv: Vec<String>) -> i32 {
    let args = util::parse_args("drive_fetch", "drive_fetch [-h] [--dry-run] [--login] [--no-browser]", HELP,
                                &[("--dry-run", false), ("--login", false), ("--no-browser", false)], &argv[1..]);
    let rc = match run(&args, &Drive::new) {
        Ok(-1) => 0,
        Ok(BUSY) => 0, // another run is checking; it touches the heartbeat
        Ok(rc) => {
            if rc == 0 && !argv.iter().any(|a| a == "--dry-run" || a == "--login") {
                util::note_recovery(&log_file());
                // time of the last good check, shown in the menu bar
                if let Err(e) = util::touch(&common::meta().join(".drive-checked")) {
                    log(&format!("ERROR {e:#}"));
                    return 1;
                }
            }
            rc
        }
        Err(e) => {
            if let Some(x) = e.downcast_ref::<Exit>() {
                log(&x.0);
                eprintln!("{x}");
            } else if let Some(h) = e.downcast_ref::<HttpError>() {
                let body = &h.body[..h.body.len().min(300)];
                log(&format!("Drive API ERROR {}: {}", h.code, util::py_bytes_repr(body)));
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
mod tests;
