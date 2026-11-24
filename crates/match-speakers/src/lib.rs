//! Name the speakers in MacWhisper watch-folder transcripts by voice (Rust port of match_speakers.py).
//!
//! MacWhisper writes <name>.txt next to each recording in ~/Downloads/recordings with speakers
//! labelled "Speaker 1", "Speaker 2", ... This program finds the transcript's session in MacWhisper's
//! database (read only), computes a voiceprint for each speaker with MacWhisper's own embedder model
//! (bin/vpembed), compares them with the voiceprints MacWhisper keeps for people you have named, and
//! writes processed/<name>.txt with the matched names. MacWhisper's txt is then moved to
//! speaker-matching/originals/, where later re-checks read it.
//!
//! Each run also copies MacWhisper's current voiceprints to voiceprints/library.json, logs what
//! changed in voiceprints/changes.log, and re-checks earlier transcripts when that could change their
//! names: a speaker was renamed in MacWhisper, or the voiceprints changed and the transcript still
//! has unknown ("Speaker N") or uncertain ("Name?") speakers.
//!
//! Every file it writes (state.json, reports, library.json, ready.jsonl) is byte-for-byte what the
//! Python version wrote, so either can take over from the other.

mod names;
pub mod mw_write;
mod py;
mod text;

use anyhow::{anyhow, bail, Context, Result};
use pipeline_common as common;
use regex::Regex;
use rusqlite::{types::Value as Sql, Connection, OpenFlags};
use serde_json::{json, Map, Value};
use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};
use text::Line;

const USAGE: &str = "usage: match_speakers [-h] [--force] [--all] [files ...]";
const ABOUT: &str = "\
Name the speakers in MacWhisper watch-folder transcripts by voice.

MacWhisper writes <name>.txt next to each recording in ~/Downloads/recordings with speakers
labelled \"Speaker 1\", \"Speaker 2\", ... This program finds the transcript's session in MacWhisper's
database (read only), computes a voiceprint for each speaker with MacWhisper's own embedder model
(bin/vpembed), compares them with the voiceprints MacWhisper keeps for people you have named, and
writes processed/<name>.txt with the matched names. MacWhisper's txt is then moved to
speaker-matching/originals/, where later re-checks read it.

Each run also copies MacWhisper's current voiceprints to voiceprints/library.json, logs what changed
in voiceprints/changes.log, and re-checks earlier transcripts when that could change their names:
a speaker was renamed in MacWhisper, or the voiceprints changed and the transcript still has
unknown (\"Speaker N\") or uncertain (\"Name?\") speakers.

Run by the launch agent local.speaker-matching on every change to the recordings folder and every
5 minutes. Manual use:
    match_speakers                 process new or changed transcripts
    match_speakers --force FILE    reprocess one transcript (file name or path)
    match_speakers --all --force   reprocess everything

positional arguments:
  files       transcripts to process (default: every .txt in the recordings folder)

options:
  -h, --help  show this help message and exit
  --force     reprocess even if nothing changed
  --all       with --force: every transcript";

/// What a re-check of a transcript (new voiceprints, a speaker renamed) keeps from its earlier
/// record; base_stem is the name without the '_with_<names>' part, which finish() rebuilds from it.
const KEEP_ON_REPROCESS: [&str; 5] = ["macwhisper_name", "renamed_from", "title", "title_source", "base_stem"];

// ------------------------------------------------------------------ paths

fn recordings() -> PathBuf {
    common::recordings()
}
fn meta() -> PathBuf {
    common::meta()
}
fn processed() -> PathBuf {
    common::processed()
}
fn processed_audio() -> PathBuf {
    common::processed_audio()
}
fn reports() -> PathBuf {
    common::reports()
}
fn originals() -> PathBuf {
    common::originals()
}
fn voiceprints_dir() -> PathBuf {
    meta().join("voiceprints")
}
fn state_file() -> PathBuf {
    meta().join("state.json")
}
fn config_file() -> PathBuf {
    meta().join("config.json")
}
fn log_file() -> PathBuf {
    meta().join("match.log")
}
fn lock_file() -> PathBuf {
    meta().join(".lock")
}
fn vpembed() -> PathBuf {
    common::helper("vpembed") // inside TinyTranscript.app, or bin/ of a development install
}
fn ready_file() -> PathBuf {
    meta().join("ready.jsonl") // one line per finished recording, read by the menu bar app
}
fn macwhisper() -> PathBuf {
    common::home().join("Library").join("Application Support").join("MacWhisper")
}
fn db_path() -> PathBuf {
    // RECORDINGS_MW_DB: a copy of MacWhisper's database, for testing writes (mw_write.rs)
    std::env::var_os("RECORDINGS_MW_DB").map(PathBuf::from)
        .unwrap_or_else(|| macwhisper().join("Database").join("main.sqlite"))
}
fn media_dir() -> PathBuf {
    macwhisper().join("Database").join("ExternalMedia")
}
fn embedder() -> PathBuf {
    macwhisper().join("models").join("speakerkit").join("speaker_embedder").join("pyannote-v3").join("W16A16")
}

fn log(msg: &str) {
    common::log_to(&log_file(), msg);
}

/// `Path.resolve()`: the real path, or the path as is if it does not exist.
fn resolve(p: &Path) -> PathBuf {
    let p = if p.as_os_str().is_empty() { Path::new(".") } else { p };
    fs::canonicalize(p).unwrap_or_else(|_| std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf()))
}

fn parent(p: &Path) -> PathBuf {
    match p.parent() {
        Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Entries of a folder (sorted, so runs are repeatable), or none if it is not a folder.
fn entries(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = match fs::read_dir(dir) {
        Ok(rd) => rd.filter_map(|e| e.ok().map(|e| e.path())).collect(),
        Err(_) => vec![],
    };
    v.sort();
    v
}

fn mtime_and_size(p: &Path) -> Result<(f64, u64)> {
    let m = fs::metadata(p).with_context(|| format!("stat {}", p.display()))?;
    // like CPython's st_mtime: sec + nsec * 1e-9
    Ok((m.mtime() as f64 + m.mtime_nsec() as f64 * 1e-9, m.size()))
}

/// Text as Python's `read_text()` gives it (UTF-8, universal newlines).
fn read_text(p: &Path) -> Result<String> {
    let s = fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?;
    Ok(if s.contains('\r') { s.replace("\r\n", "\n").replace('\r', "\n") } else { s })
}

fn write_json(path: &Path, v: &Value, indent: usize) -> Result<()> {
    common::write_atomic(path, &(py::dumps(v, Some(indent), false) + "\n"))
}

fn load_json_obj(path: &Path) -> Map<String, Value> {
    match common::load_json(path) {
        Some(Value::Object(m)) => m,
        _ => Map::new(),
    }
}

fn mkdir(p: &Path) -> Result<()> {
    match fs::create_dir(p) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && p.is_dir() => Ok(()),
        Err(e) => Err(e).with_context(|| format!("creating {}", p.display())),
    }
}

// ------------------------------------------------------------------ config

pub fn default_config() -> Value {
    json!({
        "_help": {
            "name_threshold": "best score at or above this names the speaker",
            "maybe_threshold": "best score at or above this (and below name_threshold) gives 'Name?'",
            "min_margin": "best must beat the next different person by this much to be named without '?'",
            "min_speech_seconds": "speakers with less speech than this get at most 'Name?'",
            "settle_seconds": "wait this long after a transcript was last written before processing it",
            "aliases": "map extra spellings of a person's name to one name, e.g. {\"Jon\": \"John\"}",
            "ignore_names": "names in MacWhisper that are not people; regexes, matched case-insensitively",
            "self_names": "your own name(s) as labelled in transcripts; left out of the '_with_' part of file names",
            "with_max_names": "most people named in '_with_...' (by speaking time); the rest become '+N'",
            "short_names": "how a person appears in file names, e.g. {\"Мария Ромашка Групп\": \"Mariya\"}; otherwise a name of 3+ words (a name plus a company or role note) is cut to its first word"
        },
        "name_threshold": 0.65,
        "maybe_threshold": 0.50,
        "min_margin": 0.08,
        "min_speech_seconds": 8,
        "settle_seconds": 15,
        "aliases": {},
        "ignore_names": ["^speaker \\d+$", "^unknown", "^silence"],
        "title_max_chars": 30,
        "title_generator": "claude",
        "self_names": [],
        "with_max_names": 3,
        "short_names": {}
    })
}

pub struct Cfg {
    pub name_threshold: f64,
    pub maybe_threshold: f64,
    pub min_margin: f64,
    pub min_speech_seconds: f64,
    pub settle_seconds: f64,
    pub aliases: HashMap<String, String>,
    pub ignore: Vec<Regex>,
    pub title_max_chars: i64,
    pub title_generator: String,
    pub self_names: Vec<String>,
    pub with_max_names: i64,
    pub short_names: Vec<(String, String)>,
}

/// A Python regex as the `regex` crate reads it: Python's `$` also matches before a final newline.
fn py_regex(p: &str) -> Result<Regex> {
    let p = p.replace("\\Z", "\\z");
    let fixed = if p.ends_with('$') && !p.ends_with("\\$") { format!("{}(?:\\n?\\z)", &p[..p.len() - 1]) } else { p };
    Regex::new(&format!("(?i){fixed}")).map_err(|e| anyhow!("bad regex in config.json ignore_names: {e}"))
}

impl Cfg {
    pub fn from_value(v: &Value) -> Result<Cfg> {
        let d = default_config();
        let get = |k: &str| v.get(k).unwrap_or(&d[k]);
        let num = |k: &str| get(k).as_f64().or_else(|| d[k].as_f64()).unwrap_or(0.0);
        let int = |k: &str| get(k).as_i64().or_else(|| d[k].as_i64()).unwrap_or(0);
        let strs = |k: &str| -> Vec<String> {
            get(k).as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default()
        };
        let pairs = |k: &str| -> Vec<(String, String)> {
            get(k).as_object()
                .map(|m| m.iter().filter_map(|(a, b)| b.as_str().map(|b| (a.clone(), b.to_string()))).collect())
                .unwrap_or_default()
        };
        Ok(Cfg {
            name_threshold: num("name_threshold"),
            maybe_threshold: num("maybe_threshold"),
            min_margin: num("min_margin"),
            min_speech_seconds: num("min_speech_seconds"),
            settle_seconds: num("settle_seconds"),
            aliases: pairs("aliases").into_iter().collect(),
            ignore: strs("ignore_names").iter().map(|p| py_regex(p)).collect::<Result<_>>()?,
            title_max_chars: int("title_max_chars"),
            title_generator: get("title_generator").as_str().unwrap_or("").to_string(),
            self_names: strs("self_names"),
            with_max_names: int("with_max_names"),
            short_names: pairs("short_names"),
        })
    }

    pub fn canonical(&self, name: &str) -> String {
        let name = py::squash(name);
        self.aliases.get(&name).cloned().unwrap_or(name)
    }

    pub fn is_person(&self, name: &str) -> bool {
        !self.ignore.iter().any(|r| r.is_match(name))
    }
}

/// `^Speaker \d+$` (MacWhisper's default label).
pub fn is_speaker_label(s: &str) -> bool {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^Speaker \d+\n?\z").unwrap()).is_match(s)
}

// ------------------------------------------------------------------ vectors

fn norm(v: &[f64]) -> f64 {
    v.iter().map(|x| x * x).sum::<f64>().sqrt()
}

fn unit(v: Vec<f64>) -> Vec<f64> {
    let n = norm(&v);
    if n > 0.0 { v.into_iter().map(|x| x / n).collect() } else { v }
}

fn mean(vs: &[Vec<f64>]) -> Vec<f64> {
    let mut s = vec![0.0; vs.first().map(|v| v.len()).unwrap_or(0)];
    for v in vs {
        for (a, b) in s.iter_mut().zip(v) {
            *a += b;
        }
    }
    let n = vs.len() as f64;
    s.into_iter().map(|x| x / n).collect()
}

fn dot(a: &[f64], b: &[f64]) -> f64 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn rounded_list(v: &[f64], nd: usize) -> Value {
    Value::Array(v.iter().map(|x| json!(py::round(*x, nd))).collect())
}

// ------------------------------------------------------------------ MacWhisper database

fn db() -> Result<Connection> {
    // Read only; MacWhisper keeps writing to this database while we read it.
    let con = Connection::open_with_flags(db_path(), OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .context("unable to open MacWhisper's database")?;
    con.busy_timeout(Duration::from_secs(30))?;
    Ok(con)
}

fn hex_blob(h: &str) -> Result<Vec<u8>> {
    if !h.len().is_multiple_of(2) || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("unrecognized token: \"x'{h}'\"");
    }
    Ok((0..h.len()).step_by(2).map(|i| u8::from_str_radix(&h[i..i + 2], 16).unwrap_or(0)).collect())
}

fn sql_json(v: Sql) -> Value {
    match v {
        Sql::Null => Value::Null,
        Sql::Integer(i) => json!(i),
        Sql::Real(f) => json!(f),
        Sql::Text(s) => Value::String(s),
        Sql::Blob(b) => Value::String(String::from_utf8_lossy(&b).into_owned()),
    }
}

pub struct Voiceprint {
    id: String,
    session: String,
    speaker: String,
    name: String,
    model: Value,
    speech_seconds: f64,
    created: Value,
    recording: Option<String>,
    vec: Vec<f64>,
}

fn read_voiceprints(con: &Connection) -> Result<Vec<Voiceprint>> {
    let mut st = con.prepare(
        "select hex(v.id), hex(v.sessionID), hex(v.speakerID), s.name, v.modelID, v.speechDuration,
                v.dateCreated, se.originalFilename, v.embedding
         from speakervoiceprint v join speaker s on s.id = v.speakerID
         left join session se on se.id = v.sessionID
         order by v.dateCreated")?;
    let rows = st.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, Option<String>>(3)?,
            r.get::<_, Sql>(4)?, r.get::<_, f64>(5)?, r.get::<_, Sql>(6)?, r.get::<_, Option<String>>(7)?,
            r.get::<_, Option<Vec<u8>>>(8)?))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, session, speaker, name, model, dur, created, recording, blob) = row?;
        let blob = blob.unwrap_or_default();
        if blob.len() != 256 * 4 {
            continue;
        }
        let vec: Vec<f64> = blob.chunks(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]) as f64).collect();
        if !vec.iter().all(|x| x.is_finite()) {
            continue;
        }
        out.push(Voiceprint { id, session, speaker, name: name.unwrap_or_else(|| "None".into()), model: sql_json(model),
                              speech_seconds: py::round(dur, 1), created: sql_json(created), recording, vec });
    }
    Ok(out)
}

/// Mirror MacWhisper's voiceprints into voiceprints/library.json and log changes.
///
/// Returns (voiceprints, signature). The signature changes whenever a voiceprint is added, removed
/// or its person renamed, which tells main() to re-check unresolved transcripts.
fn sync_library(con: &Connection, cfg: &Cfg) -> Result<(Vec<Voiceprint>, String)> {
    let vps = read_voiceprints(con)?;
    let joined: Vec<String> = vps.iter().map(|v| format!("{}|{}", v.id, v.name)).collect();
    let sig = py::sha1_hex(joined.join("\n").as_bytes())[..16].to_string();
    mkdir(&voiceprints_dir())?;
    let lib_file = voiceprints_dir().join("library.json");
    let old = load_json_obj(&lib_file);
    if old.get("signature").and_then(Value::as_str) == Some(sig.as_str()) {
        return Ok((vps, sig));
    }

    let mut old_vps: Vec<(String, &Map<String, Value>)> = Vec::new();
    if let Some(Value::Array(a)) = old.get("voiceprints") {
        for o in a.iter().filter_map(Value::as_object) {
            let id = o.get("id").map(py::str_of).unwrap_or_default();
            match old_vps.iter_mut().find(|(i, _)| *i == id) {
                Some(e) => e.1 = o,
                None => old_vps.push((id, o)),
            }
        }
    }
    let rec = |r: &Option<String>| r.clone().unwrap_or_else(|| "None".into());
    let field = |o: &Map<String, Value>, k: &str| o.get(k).map(py::str_of).unwrap_or_else(|| "None".into());
    let mut changes: Vec<String> = Vec::new();
    for v in &vps {
        match old_vps.iter().find(|(i, _)| *i == v.id) {
            None => changes.push(format!("+ {}: new voiceprint from '{}' ({}s speech)", v.name, rec(&v.recording),
                                         py::float_repr(v.speech_seconds))),
            Some((_, o)) if !py::eq(o.get("name").unwrap_or(&Value::Null), &json!(v.name)) => changes.push(format!(
                "~ '{}' is now '{}' (voiceprint from '{}')", field(o, "name"), v.name, rec(&v.recording))),
            _ => {}
        }
    }
    for (id, o) in &old_vps {
        if !vps.iter().any(|v| v.id == *id) {
            changes.push(format!("- {}: voiceprint from '{}' removed", field(o, "name"), field(o, "recording")));
        }
    }
    if old.is_empty() {
        changes = vec![format!("first copy: {} voiceprints", vps.len())];
    }

    let mut people: Vec<(String, Vec<&Voiceprint>)> = Vec::new();
    for v in &vps {
        let n = cfg.canonical(&v.name);
        match people.iter_mut().find(|(p, _)| *p == n) {
            Some(e) => e.1.push(v),
            None => people.push((n, vec![v])),
        }
    }
    people.sort_by(|a, b| a.0.cmp(&b.0));
    let mut people_doc = Map::new();
    for (n, vs) in &people {
        let recs: BTreeSet<String> = vs.iter()
            .map(|v| v.recording.clone().filter(|r| !r.is_empty()).unwrap_or_else(|| "?".into())).collect();
        let units: Vec<Vec<f64>> = vs.iter().map(|v| unit(v.vec.clone())).collect();
        people_doc.insert(n.clone(), json!({
            "voiceprints": vs.len(),
            "used_for_naming": cfg.is_person(n),
            "recordings": recs.into_iter().collect::<Vec<_>>(),
            "speech_seconds": py::round(py::sum(vs.iter().map(|v| v.speech_seconds)), 1),
            "centroid": rounded_list(&unit(mean(&units)), 6),
        }));
    }
    let vp_doc: Vec<Value> = vps.iter().map(|v| json!({
        "id": v.id, "session": v.session, "speaker": v.speaker, "name": v.name, "model": v.model,
        "speech_seconds": v.speech_seconds, "created": v.created, "recording": v.recording,
        "embedding": rounded_list(&v.vec, 6),
    })).collect();
    let doc = json!({
        "_about": "Copy of MacWhisper's speaker voiceprints (pyannote/WeSpeaker, 256-d), refreshed by \
                   match_speakers. 'people' has one averaged, unit-length voiceprint per name; names \
                   matching config ignore_names are kept but not used for naming.",
        "updated": common::now_iso(),
        "signature": sig,
        "people": people_doc,
        "voiceprints": vp_doc,
    });
    write_json(&lib_file, &doc, 1)?;
    let mut f = fs::OpenOptions::new().create(true).append(true).open(voiceprints_dir().join("changes.log"))?;
    for c in &changes {
        writeln!(f, "{} {c}", common::now_iso())?;
    }
    let n_people = people.iter().filter(|(n, _)| cfg.is_person(n)).count();
    let more = if changes.len() > 5 { format!("; and {} more", changes.len() - 5) } else { String::new() };
    log(&format!("voiceprints updated ({} voiceprints, {n_people} people): {}{more}", vps.len(),
                 changes.iter().take(5).cloned().collect::<Vec<_>>().join("; ")));
    Ok((vps, sig))
}

/// Average voiceprint per named person (and how many voiceprints), leaving out the transcript's
/// own recording. In first-seen order.
fn library_for(vps: &[Voiceprint], cfg: &Cfg, exclude_session: &str) -> Vec<(String, Vec<f64>, usize)> {
    let mut by_name: Vec<(String, Vec<Vec<f64>>)> = Vec::new();
    for v in vps {
        if v.session == exclude_session || !cfg.is_person(&v.name) {
            continue;
        }
        let n = cfg.canonical(&v.name);
        let u = unit(v.vec.clone());
        match by_name.iter_mut().find(|(p, _)| *p == n) {
            Some(e) => e.1.push(u),
            None => by_name.push((n, vec![u])),
        }
    }
    by_name.into_iter().map(|(n, vs)| { let c = unit(mean(&vs)); (n, c, vs.len()) }).collect()
}

fn session_by_id(con: &Connection, session_hex: Option<&str>) -> Result<Option<(String, bool)>> {
    let Some(h) = session_hex.filter(|h| !h.is_empty()) else { return Ok(None) };
    let mut st = con.prepare("select hex(id), hasBeenDiarized from session where id = ?1 and dateDeleted is null")?;
    let mut rows = st.query([hex_blob(h)?])?;
    Ok(match rows.next()? {
        Some(r) => Some((r.get(0)?, py::truthy(Some(&sql_json(r.get(1)?))))),
        None => None,
    })
}

fn find_session(con: &Connection, stem: &str) -> Result<Option<(String, bool)>> {
    let mut st = con.prepare(
        "select hex(id), hasBeenDiarized from session
         where originalFilename = ?1 and dateDeleted is null and coalesce(isTransient, 0) = 0
         order by isFromWatchFolder desc, dateCreated desc")?;
    let mut rows = st.query([stem])?;
    Ok(match rows.next()? {
        Some(r) => Some((r.get(0)?, py::truthy(Some(&sql_json(r.get(1)?))))),
        None => None,
    })
}

/// Speaker id -> name for a session, in database order.
fn session_speaker_names(con: &Connection, session_hex: &str) -> Result<Vec<(String, Value)>> {
    let mut st = con.prepare(
        "select hex(s.id), s.name from session_speaker ss join speaker s on s.id = ss.speakerID
         where ss.sessionID = ?1")?;
    let rows = st.query_map([hex_blob(session_hex)?], |r| Ok((r.get::<_, String>(0)?, sql_json(r.get(1)?))))?;
    let mut out: Vec<(String, Value)> = Vec::new();
    for row in rows {
        let (k, v) = row?;
        match out.iter_mut().find(|(a, _)| *a == k) {
            Some(e) => e.1 = v,
            None => out.push((k, v)),
        }
    }
    Ok(out)
}

fn session_lines(con: &Connection, session_hex: &str) -> Result<Vec<Line>> {
    let mut st = con.prepare(
        "select hex(speakerID), text, wordsJson, start, end from transcriptline
         where sessionID = ?1 order by orderIndex, start")?;
    let rows = st.query_map([hex_blob(session_hex)?], |r| {
        let num = |v: Sql| match v {
            Sql::Integer(i) => i as f64,
            Sql::Real(f) => f,
            _ => 0.0,
        };
        Ok(Line { spk: r.get::<_, Option<String>>(0)?.unwrap_or_default(), text: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                  words: r.get(2)?, start: num(r.get(3)?), end: num(r.get(4)?) })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn session_media(con: &Connection, session_hex: &str, stem: &str) -> Result<Option<PathBuf>> {
    let mut st = con.prepare(
        "select filename from mediafile where sessionID = ?1 and type = 'original' order by dateCreated desc")?;
    let mut rows = st.query([hex_blob(session_hex)?])?;
    if let Some(r) = rows.next()? {
        let f: String = r.get(0)?;
        let p = media_dir().join(f);
        if p.exists() {
            return Ok(Some(p));
        }
    }
    for d in [recordings(), processed_audio(), processed()] {
        for p in entries(&d) {
            let n = file_name(&p);
            let suf = py::lower(py::suffix(&n));
            if p.is_file() && py::stem(&n) == stem && suf != ".txt" && suf != ".json" {
                return Ok(Some(p));
            }
        }
    }
    Ok(None)
}

// ------------------------------------------------------------------ matching

/// Speaker key -> (unit voiceprint, seconds of speech), in vpembed's output order.
fn run_vpembed(audio: &Path, spans: &[(String, Vec<[f64; 2]>)]) -> Result<Vec<(String, Vec<f64>, f64)>> {
    Ok(vpembed_means(audio, spans)?.into_iter().map(|(k, v, s)| (k, unit(v), s)).collect())
}

/// Speaker key -> (speech-weighted mean of the window embeddings, as MacWhisper stores a voiceprint,
/// seconds of speech).
fn vpembed_means(audio: &Path, spans: &[(String, Vec<[f64; 2]>)]) -> Result<Vec<(String, Vec<f64>, f64)>> {
    let mut seg = Map::new();
    for (k, v) in spans {
        seg.insert(k.clone(), json!(v));
    }
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
    let seg_path = meta().join(format!("tmp{}{:08x}.json", std::process::id(), nanos));
    fs::OpenOptions::new().write(true).create_new(true).open(&seg_path)
        .and_then(|mut f| f.write_all(Value::Object(seg).to_string().as_bytes()))
        .with_context(|| format!("writing {}", seg_path.display()))?;
    let args = [vpembed(), audio.to_path_buf(), seg_path.clone(), embedder()];
    let result = common::run_with_timeout(Command::new(&args[0]).args(&args[1..]), "", Duration::from_secs(1800));
    let _ = fs::remove_file(&seg_path);
    let (ok, out, err) = match result {
        Ok(r) => r,
        Err(e) if e.to_string() == "timeout" => {
            let shown: Vec<String> = args.iter().map(|a| format!("'{}'", a.display())).collect();
            bail!("Command '[{}]' timed out after 1800 seconds", shown.join(", "));
        }
        Err(e) => return Err(e.context(format!("running {}", args[0].display()))),
    };
    if !ok {
        let e = py::strip(&err);
        bail!("{}", if e.is_empty() { "vpembed exited with an error" } else { e });
    }
    let parsed: Value = serde_json::from_str(&out).context("vpembed output is not JSON")?;
    let mut emb = Vec::new();
    for (spk, r) in parsed.as_object().ok_or_else(|| anyhow!("vpembed output is not an object"))? {
        let windows = r.get("windows").and_then(Value::as_array).ok_or_else(|| anyhow!("'windows'"))?;
        if windows.is_empty() {
            continue;
        }
        let w: Vec<f64> = windows.iter().map(|x| x["w"].as_f64().unwrap_or(0.0)).collect();
        let total: f64 = w.iter().sum();
        let dim = windows[0]["e"].as_array().map(|a| a.len()).unwrap_or(0);
        let mut v = vec![0.0; dim];
        for (x, wk) in windows.iter().zip(&w) {
            let e = x["e"].as_array().ok_or_else(|| anyhow!("'e'"))?;
            for (d, ed) in v.iter_mut().zip(e) {
                *d += ed.as_f64().unwrap_or(0.0) * wk;
            }
        }
        let v: Vec<f64> = v.into_iter().map(|x| x / total).collect();
        emb.push((spk.clone(), v, r.get("seconds").and_then(Value::as_f64).unwrap_or(0.0)));
    }
    Ok(emb)
}

struct Assigned {
    speaker_id: String,
    current: Value,
    seconds: f64,
    top: Vec<(String, f64)>,
    label: Value,
    how: String,
    resolved: bool,
}

impl Assigned {
    fn label_str(&self) -> &str {
        self.label.as_str().unwrap_or("")
    }
    fn to_json(&self) -> Value {
        json!({
            "speaker_id": self.speaker_id, "current": self.current, "seconds": self.seconds,
            "top": self.top.iter().map(|(n, s)| json!({"name": n, "score": py::round(*s, 3)})).collect::<Vec<_>>(),
            "label": self.label, "how": self.how, "resolved": self.resolved,
        })
    }
}

/// Pick a label for each speaker. Speakers already named in MacWhisper keep their name.
fn assign(emb: &[(String, Vec<f64>, f64)], names_now: &[(String, Value)], lib: &[(String, Vec<f64>, usize)],
          cfg: &Cfg) -> Vec<Assigned> {
    let mut keys: Vec<String> = names_now.iter().map(|(k, _)| k.clone()).collect();
    for (k, _, _) in emb {
        if !keys.contains(k) {
            keys.push(k.clone());
        }
    }
    let mut result: Vec<Assigned> = Vec::new();
    let mut taken: Vec<String> = Vec::new();
    type Candidate = (usize, Vec<(f64, String)>, f64); // (index in result, scores best first, seconds)
    let mut candidates: Vec<Candidate> = Vec::new();
    for spk in keys {
        let current = names_now.iter().find(|(k, _)| *k == spk).map(|(_, v)| v.clone()).unwrap_or_else(|| json!(""));
        let mut r = Assigned { speaker_id: spk.clone(), current: current.clone(), seconds: 0.0, top: vec![],
                               label: Value::Null, how: String::new(), resolved: false };
        let (scores, secs) = match emb.iter().find(|(k, _, _)| *k == spk) {
            Some((_, vec, secs)) => {
                let mut scores: Vec<(f64, String)> = lib.iter().map(|(n, c, _)| (dot(vec, c), n.clone())).collect();
                scores.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal).then_with(|| b.1.cmp(&a.1)));
                r.seconds = py::round(*secs, 1);
                r.top = scores.iter().take(3).map(|(s, n)| (n.clone(), *s)).collect();
                (scores, *secs)
            }
            None => (vec![], 0.0),
        };
        let cur = current.as_str().unwrap_or("");
        if !cur.is_empty() && !is_speaker_label(cur) && cfg.is_person(cur) {
            r.label = current.clone();
            r.how = "named in MacWhisper".into();
            r.resolved = true;
            taken.push(cfg.canonical(cur));
        } else if scores.is_empty() {
            r.label = current.clone();
            r.how = "no speech or no voiceprints to compare".into();
            r.resolved = false;
        } else {
            candidates.push((result.len(), scores, secs));
        }
        result.push(r);
    }

    // Greedy, strongest match first; a person already taken can only be offered as 'Name?'.
    candidates.sort_by(|a, b| b.1[0].0.partial_cmp(&a.1[0].0).unwrap_or(std::cmp::Ordering::Equal));
    for (i, scores, secs) in candidates {
        let r = &mut result[i];
        let (best_s, best_n) = (scores[0].0, scores[0].1.clone());
        let second = scores[1..].iter().find(|(_, n)| *n != best_n).map(|(s, _)| *s).unwrap_or(0.0);
        let sure = best_s >= cfg.name_threshold && best_s - second >= cfg.min_margin
            && secs >= cfg.min_speech_seconds && !taken.contains(&best_n);
        if sure {
            r.label = json!(best_n);
            r.how = format!("voice match {best_s:.2} (next {second:.2})");
            r.resolved = true;
            taken.push(best_n);
        } else if best_s >= cfg.maybe_threshold {
            r.label = json!(format!("{best_n}?"));
            r.how = format!("possible voice match {best_s:.2} (next {second:.2})");
            r.resolved = false;
        } else {
            r.label = r.current.clone();
            r.how = format!("no match (best {best_n} {best_s:.2})");
            r.resolved = false;
        }
    }
    result
}

// ------------------------------------------------------------------ one transcript

fn obj(v: Option<&Value>) -> Map<String, Value> {
    v.and_then(Value::as_object).cloned().unwrap_or_default()
}

/// A reason to (re)process, or None.
fn needs_run(name: &str, prev: &Map<String, Value>, src_sig: &Value, names_sig: &str, lib_sig: &str,
             force: bool) -> Option<&'static str> {
    if force {
        return Some("forced");
    }
    if !py::eq(prev.get("source").unwrap_or(&Value::Null), src_sig) {
        return Some(if prev.is_empty() { "new" } else { "transcript changed" });
    }
    let status = prev.get("status").and_then(Value::as_str);
    match status {
        Some("error") => {
            let at = prev.get("at_ts").and_then(Value::as_f64).unwrap_or(0.0);
            return (common::now_ts() - at >= 3600.0).then_some("retry");
        }
        Some("no-session") => return None,
        Some("waiting-for-session") => return Some("retry"),
        _ => {}
    }
    if !processed().join(name).exists() {
        return Some("output missing");
    }
    if !names_sig.is_empty() && prev.get("names_sig").and_then(Value::as_str) != Some(names_sig) {
        return Some("speaker renamed in MacWhisper");
    }
    if status == Some("done") && py::truthy(prev.get("unresolved"))
        && prev.get("lib_sig").and_then(Value::as_str) != Some(lib_sig) {
        return Some("voiceprints changed");
    }
    None
}

fn kept(prev: &Map<String, Value>) -> Map<String, Value> {
    let mut m = Map::new();
    for k in KEEP_ON_REPROCESS {
        if let Some(v) = prev.get(k) {
            m.insert(k.to_string(), v.clone());
        }
    }
    m
}

fn process(txt: &Path, cfg: &Cfg, state: &mut Map<String, Value>, vps: &[Voiceprint], lib_sig: &str,
           force: bool) -> Result<bool> {
    let key = file_name(txt);
    let txt_stem = py::stem(&key).to_string();
    let (mut mtime, mut size) = mtime_and_size(txt)?;
    let prev = obj(state.get(&key));
    let wait = cfg.settle_seconds - (common::now_ts() - mtime);
    if wait > 0.0 {
        // MacWhisper has just written it (this run was likely triggered by that write). Wait for it
        // to settle here rather than leaving it for the next 5-minute run.
        std::thread::sleep(Duration::from_secs_f64(wait.min(120.0)));
        (mtime, size) = mtime_and_size(txt)?;
        if common::now_ts() - mtime < cfg.settle_seconds {
            return Ok(false); // still being written; the write itself triggers another run
        }
    }
    let src_sig = json!({"mtime": mtime, "size": size});

    let (session_hex, diarized, names_sig, names_now, lines, spans, audio, reason);
    {
        let con = db()?;
        // After a rename the file no longer carries MacWhisper's name for it; use the session found before.
        let mut sess = session_by_id(&con, prev.get("session").and_then(Value::as_str))?;
        if sess.is_none() {
            let mw_name = prev.get("macwhisper_name").and_then(Value::as_str).filter(|s| !s.is_empty());
            sess = find_session(&con, mw_name.unwrap_or(&txt_stem))?;
        }
        names_now = match &sess { Some((h, _)) => session_speaker_names(&con, h)?, None => vec![] };
        names_sig = if sess.is_some() {
            let mut items = names_now.clone();
            items.sort_by(|a, b| a.0.cmp(&b.0));
            let v = Value::Array(items.into_iter().map(|(k, n)| json!([k, n])).collect());
            py::sha1_hex(py::dumps(&v, None, true).as_bytes())[..16].to_string()
        } else {
            String::new()
        };
        let Some(r) = needs_run(&key, &prev, &src_sig, &names_sig, lib_sig, force) else { return Ok(false) };
        reason = r;
        let Some((h, d)) = sess else {
            let first = prev.get("first_seen").cloned().unwrap_or_else(|| json!(common::now_ts()));
            let mut e = json!({"source": src_sig, "status": "waiting-for-session", "first_seen": first});
            if common::now_ts() - first.as_f64().unwrap_or(0.0) > 86400.0 {
                e["status"] = json!("no-session");
                log(&format!("{key}: no MacWhisper session named '{txt_stem}' after a day; giving up"));
            }
            state.insert(key, e);
            return Ok(true);
        };
        (session_hex, diarized) = (h, d);
        lines = session_lines(&con, &session_hex)?;
        spans = text::speaker_spans(&lines, 0.3);
        audio = if diarized && !spans.is_empty() { session_media(&con, &session_hex, &txt_stem)? } else { None };
    }

    mkdir(&processed())?;
    let out_path = processed().join(&key);
    let text = read_text(txt)?;
    if !diarized || spans.is_empty() {
        common::write_atomic(&out_path, &text)?;
        let mut e = kept(&prev);
        for (k, v) in [("source", src_sig), ("status", json!("no-speakers")), ("session", json!(session_hex)),
                       ("names_sig", json!(names_sig)), ("processed_at", json!(common::now_iso()))] {
            e.insert(k.into(), v);
        }
        state.insert(key.clone(), Value::Object(e));
        log(&format!("{key}: transcript has no speakers; copied unchanged"));
        return Ok(true);
    }
    let Some(audio) = audio else { bail!("audio not found in MacWhisper's media folder or the recordings folder") };

    let t0 = Instant::now();
    let lib = library_for(vps, cfg, &session_hex);
    let result = assign(&run_vpembed(&audio, &spans)?, &names_now, &lib, cfg);
    let by_id = |spk: &str| result.iter().find(|r| r.speaker_id == spk);
    let label_spk = text::label_to_speaker(&text, &lines);
    let mut new_label_for: HashMap<String, String> = HashMap::new();
    for (lab, spk) in &label_spk {
        if let Some(r) = by_id(spk) {
            if !r.label_str().is_empty() && r.label_str() != lab {
                new_label_for.insert(lab.clone(), r.label_str().to_string());
            }
        }
    }
    let all_labels: BTreeSet<String> = text::txt_blocks(&text).into_iter().map(|b| b.1).collect();
    for lab in all_labels.iter().filter(|l| !label_spk.iter().any(|(k, _)| k == *l)) {
        log(&format!("{key}: could not tell which speaker '{lab}' is; left as is"));
    }
    common::write_atomic(&out_path, &text::relabel(&text, &new_label_for))?;

    let mut unresolved: Vec<String> = result.iter()
        .filter(|r| !r.resolved && r.seconds > 0.0)
        .map(|r| if py::truthy(Some(&r.label)) { py::str_of(&r.label) } else { py::str_of(&r.current) })
        .collect();
    unresolved.sort();
    let mut labels_in_txt = Map::new();
    for (lab, spk) in &label_spk {
        if let Some(r) = by_id(spk) {
            labels_in_txt.insert(lab.clone(), r.label.clone());
        }
    }
    let mut speakers: Vec<&Assigned> = result.iter().collect();
    speakers.sort_by_key(|r| py::str_of(&r.current));
    let mut compared: Vec<(&String, usize)> = lib.iter().map(|(n, _, c)| (n, *c)).collect();
    compared.sort();
    let processed_at = common::now_iso();
    let seconds_taken = py::round(t0.elapsed().as_secs_f64(), 1);
    let report = json!({
        "transcript": txt.display().to_string(), "output": out_path.display().to_string(), "session": session_hex,
        "audio": audio.display().to_string(), "processed_at": processed_at, "reason": reason,
        "seconds_taken": seconds_taken, "labels_in_txt": labels_in_txt, "unresolved": unresolved,
        "speakers": speakers.iter().map(|r| r.to_json()).collect::<Vec<_>>(),
        "people_compared": compared.iter().map(|(n, c)| ((*n).clone(), json!(c))).collect::<Map<_, _>>(),
    });
    mkdir(&reports())?;
    write_json(&reports().join(format!("{txt_stem}.json")), &report, 2)?;
    let mut e = kept(&prev);
    for (k, v) in [("source", src_sig), ("status", json!("done")), ("session", json!(session_hex)),
                   ("names_sig", json!(names_sig)), ("lib_sig", json!(lib_sig)), ("unresolved", json!(unresolved)),
                   ("processed_at", json!(processed_at)), ("labels", Value::Object(labels_in_txt.clone()))] {
        e.insert(k.into(), v);
    }
    state.insert(key.clone(), Value::Object(e));
    let summary: Vec<String> = labels_in_txt.iter().map(|(l, n)| format!("{l} -> {}", py::str_of(n))).collect();
    log(&format!("{key} ({reason}): {} [{}s]{}", summary.join(", "), py::float_repr(seconds_taken),
                 if unresolved.is_empty() { String::new() } else { format!("; unresolved: {}", unresolved.join(", ")) }));
    Ok(true)
}

fn status_done(entry: &Map<String, Value>) -> bool {
    matches!(entry.get("status").and_then(Value::as_str), Some("done") | Some("no-speakers"))
}

/// Once the named copy is in processed/, move MacWhisper's own txt out of the recordings folder.
///
/// It goes to originals/ rather than the bin: later re-checks (a rename in MacWhisper, new
/// voiceprints) relabel from it. Moving keeps its modification time, so it is not seen as changed.
fn move_to_originals(txt: &Path, state: &Map<String, Value>) -> Result<bool> {
    if resolve(&parent(txt)) != resolve(&recordings()) || !txt.exists() {
        return Ok(false);
    }
    let name = file_name(txt);
    if !status_done(&obj(state.get(&name))) || !processed().join(&name).exists() {
        return Ok(false);
    }
    mkdir(&originals())?;
    fs::rename(txt, originals().join(&name)).with_context(|| format!("moving {}", txt.display()))?;
    log(&format!("{name}: moved MacWhisper's original to speaker-matching/originals/"));
    Ok(true)
}

/// Recordings saved under a temporary name by zoom_fetch ('<date>_-_Zoom_call_HHMM'), drive_fetch
/// ('<date>_-_Drive_HHMM') or the Telegram listener in the menu bar app ('<date>_-_Telegram_HHMM'),
/// waiting for a title written from the full transcript.
fn titles_pending() -> Map<String, Value> {
    let mut pending = Map::new();
    for f in ["zoom_state.json", "drive_state.json", "telegram_state.json"] {
        for e in load_json_obj(&meta().join(f)).values() {
            if e.get("status").and_then(Value::as_str) == Some("fetched") && py::truthy(e.get("title_pending")) {
                if let Some(n) = e.get("name") {
                    pending.insert(py::str_of(n), e.clone());
                }
            }
        }
    }
    pending
}

/// The other people named for sure in a transcript (see names::people_from), with speech times
/// from its report.
fn people_in(stem: &str, entry: &Map<String, Value>, cfg: &Cfg) -> Vec<String> {
    let labels = obj(entry.get("labels"));
    let mut seconds: HashMap<String, f64> = HashMap::new();
    if let Some(Value::Object(rep)) = common::load_json(&reports().join(format!("{stem}.json"))) {
        if let Some(Value::Array(sps)) = rep.get("speakers") {
            for sp in sps {
                if let Some(lab) = sp.get("label").filter(|l| py::truthy(Some(l))) {
                    let s = sp.get("seconds").and_then(Value::as_f64).unwrap_or(0.0);
                    *seconds.entry(py::str_of(lab)).or_insert(0.0) += s;
                }
            }
        }
    }
    names::people_from(&labels, &seconds, cfg)
}

fn audio_for(stem: &str, folder: &Path) -> Vec<PathBuf> {
    entries(folder).into_iter()
        .filter(|p| {
            let n = file_name(p);
            let suf = py::lower(py::suffix(&n));
            p.is_file() && py::stem(&n) == stem && ![".txt", ".json", ".vtt", ".part"].contains(&suf.as_str())
        })
        .collect()
}

/// After a transcript is done: give recordings saved under a temporary name (Zoom, Drive) their
/// real title (written from the full named transcript), add the other people's names
/// ('<date>_-_<title>_with_Boris_Sofia'), then move the audio into processed/audio/, so the
/// recordings folder only holds what is still to do. The '_with_' part follows later changes to the
/// speaker names. Returns true if anything moved.
///
/// MacWhisper watches only the top level of the recordings folder (subfolder depth 0, set
/// 2026-11-23); audio renamed or moved inside a watched folder would be transcribed again.
fn finish(name: &str, state: &mut Map<String, Value>, cfg: &Cfg, pending: &Map<String, Value>) -> Result<bool> {
    let mut entry = obj(state.get(name));
    if !status_done(&entry) || !processed().join(name).exists() {
        return Ok(false);
    }
    let stem = py::stem(name).to_string();
    let mut base = entry.get("base_stem").filter(|v| py::truthy(Some(v))).map(py::str_of).unwrap_or_else(|| stem.clone());
    let (mut titled_now, mut titled) = (false, false);
    if let Some(p) = pending.get(&stem).filter(|_| !py::truthy(entry.get("title"))) {
        titled_now = true;
        let preset = p.get("preset_title").filter(|v| py::truthy(Some(v))).map(py::str_of); // a title the fetcher already knew
        let attempt: Result<String> = match &preset {
            Some(t) => Ok(t.clone()),
            None if cfg.title_generator == "claude" && common::claude::available() => read_text(&processed().join(name))
                .and_then(|t| names::claude_title(&t, cfg.title_max_chars, &stem)),
            None => Ok(String::new()),
        };
        let title = match attempt {
            Ok(t) => t,
            Err(e) => {
                let n = entry.get("title_attempts").and_then(Value::as_i64).unwrap_or(0) + 1;
                entry.insert("title_attempts".into(), json!(n));
                log(&format!("{name}: title from Claude failed ({e:#}); attempt {n}"));
                state.insert(name.to_string(), Value::Object(entry.clone()));
                if n < 3 {
                    return Ok(true); // keep the audio where it is and try again next run
                }
                String::new()
            }
        };
        entry.insert("title".into(), json!(if title.is_empty() { "(kept temporary name)" } else { &title }));
        if preset.is_some() {
            entry.insert("title_source".into(), json!("name"));
        }
        if !title.is_empty() {
            titled = true;
            let date = p.get("preset_date").filter(|v| py::truthy(Some(v))).map(py::str_of)
                .unwrap_or_else(|| py::prefix(&stem, 8).to_string());
            base = format!("{date} - {title}").replace(' ', "_");
        }
    }
    let people = if entry.get("status").and_then(Value::as_str) == Some("done") { people_in(&stem, &entry, cfg) } else { vec![] };
    let mut new_stem = base.clone();
    if !people.is_empty() {
        new_stem += &format!("_with_{}", people.iter().map(|p| p.replace(' ', "_")).collect::<Vec<_>>().join("_"));
    }
    if new_stem != stem {
        let clash = |n: &str| [processed(), processed_audio(), originals(), recordings()].iter()
            .any(|d| entries(d).iter().any(|p| py::stem(&file_name(p)) == n));
        let (mut candidate, mut i) = (new_stem.clone(), 2);
        while candidate != stem && clash(&candidate) {
            candidate = format!("{new_stem}_{i}");
            i += 1;
        }
        new_stem = candidate;
    }
    entry.insert("base_stem".into(), json!(base));
    state.insert(name.to_string(), Value::Object(entry.clone()));
    let mut moves: Vec<(PathBuf, PathBuf)> = Vec::new();
    let suffix_of = |p: &Path| py::suffix(&file_name(p)).to_string();
    for p in audio_for(&stem, &recordings()) {
        let dst = processed_audio().join(format!("{new_stem}{}", suffix_of(&p)));
        moves.push((p, dst));
    }
    if new_stem != stem {
        for p in audio_for(&stem, &processed_audio()) {
            // moved there by an earlier run
            let dst = processed_audio().join(format!("{new_stem}{}", suffix_of(&p)));
            moves.push((p, dst));
        }
        for (d, suffix) in [(processed(), ".txt"), (originals(), ".txt"), (reports(), ".json")] {
            let src = d.join(format!("{stem}{suffix}"));
            if src.exists() {
                moves.push((src, d.join(format!("{new_stem}{suffix}"))));
            }
        }
    }
    if moves.is_empty() && new_stem == stem {
        return Ok(titled_now);
    }
    for (src, dst) in &moves {
        mkdir(&parent(dst))?;
        fs::rename(src, dst).with_context(|| format!("moving {} to {}", src.display(), dst.display()))?;
    }
    if new_stem != stem {
        let new_name = format!("{new_stem}.txt");
        if !entry.contains_key("macwhisper_name") {
            entry.insert("macwhisper_name".into(), json!(stem));
        }
        entry.insert("renamed_from".into(), json!(stem));
        state.insert(name.to_string(), Value::Object(entry.clone()));
        state.insert(new_name.clone(), Value::Object(entry.clone()));
        state.shift_remove(name);
        let how = if !titled_now {
            "renamed"
        } else if !titled && cfg.title_generator == "claude" && !common::claude::available() {
            "no title (Claude Code is not installed); kept the temporary name, renamed"
        } else if !titled && cfg.title_generator == "claude" {
            "no title (Claude failed 3 times); kept the temporary name, renamed"
        } else if !titled {
            "kept the temporary name, renamed"
        } else if entry.get("title_source").and_then(Value::as_str) == Some("name") {
            "titled from the file name"
        } else {
            "titled from the full transcript"
        };
        log(&format!("{name}: {how} -> {new_name}"));
    }
    let moved_audio: Vec<String> = moves.iter().filter(|(s, _)| parent(s) == recordings()).map(|(_, d)| file_name(d)).collect();
    if !moved_audio.is_empty() {
        log(&format!("{new_stem}: audio moved to processed/audio/ ({})", moved_audio.join(", ")));
        announce_ready(&new_stem, &people)?;
    }
    Ok(true)
}

/// Tell the menu bar app a recording is finished; it shows a notification that opens it.
fn announce_ready(stem: &str, people: &[String]) -> Result<()> {
    static DATE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = DATE.get_or_init(|| Regex::new(r"^\d{8}_-_").unwrap());
    let head = stem.split("_with_").next().unwrap_or(stem);
    let title = re.replace(head, "").replace('_', " ");
    let event = json!({"at": common::now_iso(), "name": format!("{stem}.txt"), "title": title,
                       "people": people.iter().filter(|p| !p.starts_with('+')).collect::<Vec<_>>()});
    let mut f = fs::OpenOptions::new().create(true).append(true).open(ready_file())?;
    writeln!(f, "{}", py::dumps(&event, None, false))?;
    Ok(())
}

// ------------------------------------------------------------------ main

struct Args {
    files: Vec<String>,
    force: bool,
    all: bool,
}

fn parse_args(argv: &[String]) -> Args {
    let mut a = Args { files: vec![], force: false, all: false };
    let mut only_files = false;
    let mut unknown = Vec::new();
    for arg in argv.iter().skip(1).cloned() {
        if only_files || arg == "-" || !arg.starts_with('-') {
            a.files.push(arg);
        } else if arg == "--" {
            only_files = true;
        } else if arg == "-h" || (arg.len() > 2 && "--help".starts_with(&arg)) {
            println!("{USAGE}\n\n{ABOUT}");
            std::process::exit(0);
        } else if arg.len() > 2 && "--force".starts_with(&arg) {
            a.force = true;
        } else if arg.len() > 2 && "--all".starts_with(&arg) {
            a.all = true;
        } else {
            unknown.push(arg);
        }
    }
    if !unknown.is_empty() {
        eprintln!("{USAGE}\nmatch_speakers: error: unrecognized arguments: {}", unknown.join(" "));
        std::process::exit(2);
    }
    a
}

fn txt_files(folder: &Path) -> Vec<(String, PathBuf)> {
    entries(folder).into_iter()
        .filter(|p| {
            let n = file_name(p);
            n.ends_with(".txt") && !n.starts_with('.') && p.is_file()
        })
        .map(|p| (file_name(&p), p))
        .collect()
}

fn run(argv: &[String]) -> Result<i32> {
    let args = parse_args(argv);
    mkdir(&meta())?;
    let Some(_lock) = common::try_lock(&lock_file())? else {
        return Ok(0); // another run is busy; the launch agent calls again
    };

    let cfg_value = common::load_config(&config_file(), &default_config(), true);
    let cfg = Cfg::from_value(&cfg_value)?;
    let mut state = load_json_obj(&state_file());
    let (vps, lib_sig) = {
        let con = db()?;
        sync_library(&con, &cfg)?
    };

    // New MacWhisper exports sit in the recordings folder; processed ones were moved to originals/.
    // A new export wins over an older original of the same name.
    let mut available: HashMap<String, PathBuf> = HashMap::new();
    if originals().exists() {
        available.extend(txt_files(&originals()));
    }
    available.extend(txt_files(&recordings()));
    let txts: Vec<PathBuf> = if !args.files.is_empty() {
        args.files.iter()
            .map(|f| if f.contains('/') { PathBuf::from(py::norm_path(f)) } else {
                available.get(f).cloned().unwrap_or_else(|| recordings().join(f))
            })
            .collect()
    } else {
        let mut names: Vec<&String> = available.keys().collect();
        names.sort();
        names.into_iter().map(|n| available[n].clone()).collect()
    };
    let force = args.force && (args.all || !args.files.is_empty());
    let pending = titles_pending();
    let (rec, orig) = (resolve(&recordings()), resolve(&originals()));
    for txt in txts {
        let d = resolve(&parent(&txt));
        if !txt.exists() || (d != rec && d != orig) {
            log(&format!("skipping {}: not a transcript in {} or {}", txt.display(), recordings().display(),
                         originals().display()));
            continue;
        }
        let name = file_name(&txt);
        let attempt = (|| -> Result<bool> {
            let mut changed = process(&txt, &cfg, &mut state, &vps, &lib_sig, force)?;
            changed |= move_to_originals(&txt, &state)?;
            changed |= finish(&name, &mut state, &cfg, &pending)?;
            Ok(changed)
        })();
        let changed = match attempt {
            Ok(c) => c,
            Err(e) => {
                // keep going with other files; retry this one later
                let src = if txt.exists() { txt.clone() } else { originals().join(&name) };
                let source = mtime_and_size(&src).map(|(m, s)| json!({"mtime": m, "size": s})).unwrap_or(Value::Null);
                let msg = format!("{e:#}");
                let mut entry = obj(state.get(&name));
                for (k, v) in [("status", json!("error")), ("error", json!(py::prefix(&msg, 500))),
                               ("at", json!(common::now_iso())), ("at_ts", json!(common::now_ts())), ("source", source)] {
                    entry.insert(k.into(), v);
                }
                state.insert(name.clone(), Value::Object(entry));
                log(&format!("{name}: ERROR {msg}"));
                true
            }
        };
        if changed {
            write_json(&state_file(), &Value::Object(state.clone()), 2)?;
        }
    }
    Ok(0)
}

/// One run of the job, as the program would do it; returns the exit code. `argv[0]` is the program
/// name, as in `std::env::args()`. The menu bar app calls this on its own timer.
pub fn job(argv: Vec<String>) -> i32 {
    match run(&argv) {
        Ok(code) => code,
        Err(e) => {
            // Python would crash here with a traceback in launchd-errors.log; log it where the menu bar looks.
            eprintln!("match_speakers: {e:#}");
            if meta().is_dir() {
                log(&format!("ERROR {e:#}"));
            }
            1
        }
    }
}

/// The command line: run once and exit with its code.
pub fn main_with(argv: Vec<String>) -> ! {
    std::process::exit(job(argv))
}
