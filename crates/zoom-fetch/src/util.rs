//! Small helpers shared by zoom_fetch and drive_fetch (the same file is in both crates, as the two
//! Python scripts each had their own copy): HTTP with urllib-like errors, Python-style text
//! helpers, the cache folder, argument parsing, note_recovery and the heartbeat file.

use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant, SystemTime};

// ------------------------------------------------------------------ errors

/// An HTTP error status, like Python's urllib.error.HTTPError (`str()` is "HTTP Error 404: Not Found").
#[derive(Debug)]
pub struct HttpError {
    pub code: u16,
    pub reason: String,
    pub body: Vec<u8>,
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "HTTP Error {}: {}", self.code, self.reason)
    }
}
impl std::error::Error for HttpError {}

/// Python's SystemExit("message"): the message goes to stderr and the exit code is 1.
#[derive(Debug)]
pub struct Exit(pub String);

impl fmt::Display for Exit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Exit {}

/// A ureq error as urllib would have raised it. Transport errors never include the URL (the Zoom
/// download URL carries the access token).
pub fn http_err(e: ureq::Error) -> anyhow::Error {
    match e {
        ureq::Error::Status(code, resp) => {
            let reason = resp.status_text().to_string();
            let mut body = Vec::new();
            let _ = resp.into_reader().take(1 << 20).read_to_end(&mut body);
            anyhow::Error::new(HttpError { code, reason, body })
        }
        ureq::Error::Transport(t) => {
            let mut msg = t.kind().to_string();
            if let Some(m) = t.message() {
                msg += &format!(": {m}");
            }
            if let Some(s) = std::error::Error::source(&t) {
                msg += &format!(": {s}");
            }
            anyhow!("<urlopen error {msg}>")
        }
    }
}

/// An agent whose connect/read/write timeout is `secs` per socket operation, like urllib's `timeout`.
pub fn agent(secs: u64) -> ureq::Agent {
    let d = Duration::from_secs(secs);
    ureq::AgentBuilder::new().timeout_connect(d).timeout_read(d).timeout_write(d).build()
}

/// `urllib.parse.urlencode(params)`.
pub fn urlencode(params: &[(&str, &str)]) -> String {
    let mut s = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in params {
        s.append_pair(k, v);
    }
    s.finish()
}

/// Python's `repr()` of a bytes object: b'...'.
pub fn py_bytes_repr(b: &[u8]) -> String {
    let quote = if b.contains(&b'\'') && !b.contains(&b'"') { '"' } else { '\'' };
    let mut s = String::from("b");
    s.push(quote);
    for &c in b {
        match c {
            b'\\' => s.push_str("\\\\"),
            b'\t' => s.push_str("\\t"),
            b'\n' => s.push_str("\\n"),
            b'\r' => s.push_str("\\r"),
            c if c as char == quote => {
                s.push('\\');
                s.push(quote);
            }
            0x20..=0x7e => s.push(c as char),
            _ => s.push_str(&format!("\\x{c:02x}")),
        }
    }
    s.push(quote);
    s
}

// ------------------------------------------------------------------ Python-style text

/// Python's `str.isspace()` (Rust's whitespace plus the ASCII separators \x1c-\x1f).
pub fn py_space(c: char) -> bool {
    c.is_whitespace() || ('\x1c'..='\x1f').contains(&c)
}

/// `" ".join(s.split())`.
pub fn join_words(s: &str) -> String {
    s.split(py_space).filter(|w| !w.is_empty()).collect::<Vec<_>>().join(" ")
}

/// Python's truthiness of a JSON value.
#[allow(dead_code)]
pub fn truthy(v: &serde_json::Value) -> bool {
    use serde_json::Value::*;
    match v {
        Null => false,
        Bool(b) => *b,
        Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        String(s) => !s.is_empty(),
        Array(a) => !a.is_empty(),
        Object(o) => !o.is_empty(),
    }
}

/// `dt.isoformat(timespec="seconds")` of an aware UTC datetime: 2026-11-23T17:37:37+00:00.
pub fn utc_iso(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%dT%H:%M:%S+00:00").to_string()
}

/// `Path(p).expanduser()` for "~" and "~/...".
#[allow(dead_code)]
pub fn expanduser(p: &str) -> PathBuf {
    if p == "~" {
        pipeline_common::home()
    } else if let Some(rest) = p.strip_prefix("~/") {
        pipeline_common::home().join(rest)
    } else {
        PathBuf::from(p)
    }
}

/// Whether any entry of the folder `d` has a name starting with `prefix` (false if `d` is not a folder).
pub fn dir_has_prefix(d: &Path, prefix: &str) -> Result<bool> {
    if !d.is_dir() {
        return Ok(false);
    }
    for e in fs::read_dir(d)? {
        if e?.file_name().to_string_lossy().starts_with(prefix) {
            return Ok(true);
        }
    }
    Ok(false)
}

// ------------------------------------------------------------------ files and folders

/// Where downloads land first: `~/Library/Caches/<name>` (outside the watch folder, same disk).
/// `RECORDINGS_CACHE` replaces `~/Library/Caches`, for tests against a scratch RECORDINGS_ROOT.
pub fn partial_dir(name: &str) -> PathBuf {
    match std::env::var("RECORDINGS_CACHE") {
        Ok(p) if !p.is_empty() => PathBuf::from(p).join(name),
        _ => pipeline_common::home().join("Library").join("Caches").join(name),
    }
}

/// Delete leftovers of interrupted runs (anything in `dir` older than a day).
pub fn clean_partial(dir: &Path) -> Result<()> {
    for e in fs::read_dir(dir)? {
        let p = e?.path();
        let mtime = fs::metadata(&p)?.modified()?;
        if SystemTime::now().duration_since(mtime).unwrap_or_default() > Duration::from_secs(86400) {
            fs::remove_file(&p).map_err(|e| anyhow!("{e}: '{}'", p.display()))?;
        }
    }
    Ok(())
}

/// `Path(p).touch()`: create the file or update its modification time.
pub fn touch(p: &Path) -> Result<()> {
    let f = OpenOptions::new().create(true).append(true).open(p)?;
    f.set_modified(SystemTime::now())?;
    Ok(())
}

/// After a run that worked, if the log still ends with an error, say it is fixed, so the menu bar
/// stops showing the old error (runs that find nothing new log nothing).
pub fn note_recovery(log_file: &Path) {
    if !log_file.exists() {
        return;
    }
    let last = pipeline_common::last_line(log_file);
    if last.contains("ERROR") || last.contains("login expired") {
        pipeline_common::log_to(log_file, "working again (the error above is resolved)");
    }
}

/// Run a command (stdin closed) with a time limit; Err("timeout") if it runs too long.
#[allow(dead_code)]
pub fn run_capture(cmd: &mut Command, timeout: Duration) -> Result<(ExitStatus, Vec<u8>, Vec<u8>)> {
    let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let mut out = child.stdout.take().unwrap();
    let mut err = child.stderr.take().unwrap();
    let out_t = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = out.read_to_end(&mut v);
        v
    });
    let err_t = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = err.read_to_end(&mut v);
        v
    });
    let start = Instant::now();
    let status = loop {
        if let Some(st) = child.try_wait()? {
            break st;
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(anyhow!("timeout"));
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    Ok((status, out_t.join().unwrap_or_default(), err_t.join().unwrap_or_default()))
}

// ------------------------------------------------------------------ arguments

/// Parsed command line: flags given, and option values.
#[derive(Default, Debug)]
pub struct Args {
    pub flags: Vec<String>,
    pub values: Vec<(String, String)>,
}

impl Args {
    pub fn has(&self, flag: &str) -> bool {
        self.flags.iter().any(|f| f == flag)
    }
    #[allow(dead_code)]
    pub fn value(&self, opt: &str) -> Option<&str> {
        self.values.iter().rev().find(|(k, _)| k == opt).map(|(_, v)| v.as_str())
    }
}

/// A small argparse: `options` are (name, takes a value); unique prefixes are accepted as argparse
/// does; `-h`/`--help` prints `help` and exits 0; anything else is an error (exit 2).
pub fn parse_args(prog: &str, usage: &str, help: &str, options: &[(&str, bool)], argv: &[String]) -> Args {
    let fail = |msg: String| -> ! {
        eprintln!("usage: {usage}\n{prog}: error: {msg}");
        std::process::exit(2);
    };
    let mut args = Args::default();
    let mut unknown = Vec::new();
    let mut i = 0;
    while i < argv.len() {
        let a = &argv[i];
        i += 1;
        if a == "-h" || a == "--help" {
            println!("usage: {usage}\n\n{help}");
            std::process::exit(0);
        }
        if !a.starts_with("--") || a == "--" {
            unknown.push(a.clone());
            continue;
        }
        let (key, inline) = match a.split_once('=') {
            Some((k, v)) => (k, Some(v.to_string())),
            None => (a.as_str(), None),
        };
        let exact: Vec<_> = options.iter().filter(|(n, _)| *n == key).collect();
        let matches: Vec<_> = if exact.is_empty() {
            options.iter().filter(|(n, _)| n.starts_with(key)).collect()
        } else {
            exact
        };
        match matches.as_slice() {
            [] => unknown.push(a.clone()),
            [(name, takes)] => {
                if *takes {
                    let v = match inline {
                        Some(v) => v,
                        None if i < argv.len() && !argv[i].starts_with('-') => {
                            i += 1;
                            argv[i - 1].clone()
                        }
                        None => fail(format!("argument {name}: expected one argument")),
                    };
                    args.values.push((name.to_string(), v));
                } else if inline.is_some() {
                    fail(format!("argument {name}: ignored explicit argument '{}'", inline.unwrap()));
                } else {
                    args.flags.push(name.to_string());
                }
            }
            many => {
                let names: Vec<_> = many.iter().map(|(n, _)| *n).collect();
                fail(format!("ambiguous option: {key} could match {}", names.join(", ")));
            }
        }
    }
    if !unknown.is_empty() {
        fail(format!("unrecognized arguments: {}", unknown.join(" ")));
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_repr() {
        assert_eq!(py_bytes_repr(b"{\"error\": \"x\"}"), "b'{\"error\": \"x\"}'");
        assert_eq!(py_bytes_repr(b"it's"), "b\"it's\"");
        assert_eq!(py_bytes_repr(b"a'b\"c"), "b'a\\'b\"c'");
        assert_eq!(py_bytes_repr(b"x\n\x00\xff"), "b'x\\n\\x00\\xff'");
        assert_eq!(py_bytes_repr(b""), "b''");
    }

    #[test]
    fn words_and_truth() {
        assert_eq!(join_words("  a \t b\u{1f}c  "), "a b c");
        assert!(!truthy(&serde_json::json!(0)));
        assert!(truthy(&serde_json::json!(true)));
        assert!(!truthy(&serde_json::json!("")));
        assert!(!truthy(&serde_json::json!([])));
    }

    #[test]
    fn args() {
        let opts = [("--dry-run", false), ("--since", true)];
        let v: Vec<String> = ["--dry", "--since", "2026-11-16T00:00:00Z"].iter().map(|s| s.to_string()).collect();
        let a = parse_args("p", "p", "", &opts, &v);
        assert!(a.has("--dry-run"));
        assert_eq!(a.value("--since"), Some("2026-11-16T00:00:00Z"));
        let v: Vec<String> = vec!["--since=X".into()];
        assert_eq!(parse_args("p", "p", "", &opts, &v).value("--since"), Some("X"));
    }

    #[test]
    fn urlencode_like_python() {
        assert_eq!(urlencode(&[("q", "'a' in parents"), ("n", "x/y")]), "q=%27a%27+in+parents&n=x%2Fy");
    }
}
