//! `TinyTranscript setup`: the one Terminal step on a new Mac (for a Drive-only user).
//!
//! 1. creates ~/Downloads/recordings and its speaker-matching folder (config, logs, state);
//! 2. asks your name as MacWhisper labels you, so it is left out of the '_with_' part of names;
//! 3. Google Drive (optional; Return skips it): stores the Google OAuth client
//!    (google-client.json, from whoever gave you the app) in the login keychain and signs you in;
//! 4. checks MacWhisper's watch folder settings and whether Claude Code is installed (read only);
//! 5. installs the launch agent: Recordings starts now and at every login.
//!
//! Only steps 1 and 5 can stop the setup: a skipped or failed optional step is listed at the end,
//! and the launch agent is installed anyway (the app opens setup in Terminal until it exists).
//! `TinyTranscript install-agent` does only step 5 (install.sh uses it). Opening TinyTranscript.app from
//! the Finder before setup opens Terminal with `TinyTranscript setup`; once setup has been
//! started, opening it again installs the launch agent instead (see `setup_started`).

use anyhow::{anyhow, Context, Result};
use pipeline_common as common;
use serde_json::{json, Value};
use std::cell::RefCell;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

pub const AGENT_LABEL: &str = "local.recordings-menubar";
const CLIENT_ITEM: &str = "google-drive-recordings-client";
const LOGIN_ITEM: &str = "google-drive-recordings";
const MACWHISPER: &str = "com.goodsnooze.MacWhisper";

pub fn agent_plist_path() -> PathBuf {
    common::home().join("Library/LaunchAgents").join(format!("{AGENT_LABEL}.plist"))
}

/// Written when a (real) setup starts: from then on, opening the app installs the launch agent
/// rather than opening setup again, so an aborted setup cannot keep the app from starting.
fn setup_marker() -> PathBuf {
    common::settings_file().with_file_name("setup-started")
}

pub fn setup_started() -> bool {
    setup_marker().exists()
}

/// Whether this process was started by its launch agent (not from the Finder or a shell).
pub fn started_by_launchd() -> bool {
    std::env::var("XPC_SERVICE_NAME").is_ok_and(|v| v == AGENT_LABEL)
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// The launch agent for this Mac: this program, started at login and after a crash (not after
/// Quit), with errors in speaker-matching/launchd-errors.log.
pub fn agent_plist(exe: &Path) -> String {
    let errors = errors_log();
    format!(r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>KeepAlive</key>
	<dict>
		<key>SuccessfulExit</key>
		<false/>
	</dict>
	<key>Label</key>
	<string>{AGENT_LABEL}</string>
	<key>LimitLoadToSessionType</key>
	<string>Aqua</string>
	<key>ProcessType</key>
	<string>Interactive</string>
	<key>ProgramArguments</key>
	<array>
		<string>{exe}</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>StandardErrorPath</key>
	<string>{errors}</string>
	<key>StandardOutPath</key>
	<string>/dev/null</string>
</dict>
</plist>
"#, exe = xml_escape(&exe.to_string_lossy()), errors = xml_escape(&errors.to_string_lossy()))
}

/// Crashes (launchd's stderr), outside the work folder so it can move: ~/Library/Logs/TinyTranscript.
pub fn errors_log() -> PathBuf {
    common::home().join("Library/Logs/TinyTranscript/launchd-errors.log")
}

/// Write the launch agent for this program and (re)start it.
pub fn install_agent() -> Result<()> {
    std::fs::create_dir_all(errors_log().parent().unwrap())?;
    let exe = std::env::current_exe()?.canonicalize()?;
    let plist = agent_plist_path();
    std::fs::create_dir_all(plist.parent().unwrap())?;
    std::fs::create_dir_all(common::meta())?;
    common::write_atomic(&plist, &agent_plist(&exe))?;
    let domain = format!("gui/{}", crate::status::uid());
    let _ = Command::new("/bin/launchctl").args(["bootout", &format!("{domain}/{AGENT_LABEL}")]).output();
    let st = Command::new("/bin/launchctl").args(["bootstrap", &domain]).arg(&plist).output()?;
    if !st.status.success() {
        return Err(anyhow!("launchctl bootstrap failed: {}", String::from_utf8_lossy(&st.stderr).trim()));
    }
    Ok(())
}

/// Open Terminal with `TinyTranscript setup` (TinyTranscript.app opened from the Finder before setup):
/// a .command file opened with `open`, which needs no permission to control Terminal.
pub fn open_in_terminal() {
    let exe = std::env::current_exe().unwrap_or_default();
    let script = std::env::temp_dir().join("TinyTranscript setup.command");
    let body = format!("#!/bin/zsh\nclear\n'{}' setup\n", exe.to_string_lossy().replace('\'', "'\\''"));
    if std::fs::write(&script, body).is_ok() {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700));
        let _ = Command::new("/usr/bin/open").arg(&script).output();
    }
}

fn ok(msg: &str) {
    println!("  ✓ {msg}");
}

fn todo(msg: &str) {
    println!("  ✗ {msg}");
}

/// `defaults read com.goodsnooze.MacWhisper KEY`, with JSON escapes removed; None if unset.
fn macwhisper_pref(key: &str) -> Option<String> {
    let out = Command::new("/usr/bin/defaults").args(["read", MACWHISPER, key]).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).replace('\\', ""))
}

/// The OAuth client from Google's download format ({"installed": {...}}) or a flat
/// {"client_id", "client_secret"}.
fn client_from_file(path: &Path) -> Result<Value> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path)?).with_context(|| format!("reading {}", path.display()))?;
    let inner = v.get("installed").or_else(|| v.get("web")).unwrap_or(&v);
    let id = inner.get("client_id").and_then(Value::as_str).ok_or_else(|| anyhow!("no client_id in {}", path.display()))?;
    let secret = inner.get("client_secret").and_then(Value::as_str).ok_or_else(|| anyhow!("no client_secret in {}", path.display()))?;
    Ok(json!({"client_id": id, "client_secret": secret}))
}

/// A path dragged into Terminal: quoted ('…' or "…") or with backslash escapes (My\ Folder).
fn unescape_path(s: &str) -> PathBuf {
    let s = s.trim();
    for q in ['\'', '"'] {
        if s.len() >= 2 && s.starts_with(q) && s.ends_with(q) {
            return PathBuf::from(&s[1..s.len() - 1]);
        }
    }
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(n) = chars.next() {
                out.push(n);
            }
        } else {
            out.push(c);
        }
    }
    PathBuf::from(out)
}

/// google-client.json next to TinyTranscript.app, in Downloads, or in the current folder.
fn find_client_file() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let beside_app = exe.ancestors().find(|p| p.extension().is_some_and(|e| e == "app")).and_then(Path::parent).map(Path::to_path_buf);
    [beside_app, Some(common::home().join("Downloads")), std::env::current_dir().ok()]
        .into_iter().flatten().map(|d| d.join("google-client.json")).find(|p| p.is_file())
}

/// What setup reads and changes outside itself, so a test can run it.
pub struct Io<'a> {
    pub input: RefCell<&'a mut dyn BufRead>,
    pub in_keychain: &'a dyn Fn(&str) -> bool,
    pub client_file: &'a dyn Fn() -> Option<PathBuf>,
    pub store_client: &'a dyn Fn(&Value) -> Result<()>,
    /// sign in to Google Drive in the browser; true if signed in
    pub sign_in: &'a dyn Fn() -> bool,
    pub install_agent: &'a dyn Fn() -> Result<()>,
}

impl Io<'_> {
    /// A line of input; "" at the end of the input (closed stdin).
    fn ask(&self, question: &str) -> String {
        print!("{question}");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        let _ = self.input.borrow_mut().read_line(&mut line);
        line.trim().to_string()
    }
}

fn step_folders() -> Result<()> {
    std::fs::create_dir_all(common::meta())?;
    ok(&format!("recordings folder: {}", common::recordings().display()));
    Ok(())
}

fn step_name(io: &Io) -> Result<()> {
    let path = common::meta().join("config.json");
    let mut cfg = common::load_config(&path, &match_speakers::default_config(), !dry());
    let now: Vec<String> = cfg["self_names"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default();
    let hint = if now.is_empty() { String::new() } else { format!(" [{}]", now.join(", ")) };
    let name = io.ask(&format!("Your name as MacWhisper shows it for you in transcripts{hint}: "));
    if !name.is_empty() {
        cfg["self_names"] = json!([name]);
        if dry() {
            ok(&format!("[dry run] would save your name: {name}"));
            return Ok(());
        }
        common::save_json(&path, &cfg)?;
    }
    ok("your name is left out of file names ('..._with_<the others>')");
    Ok(())
}

const DRIVE_LATER: &str = "to add Google Drive later, put google-client.json next to the app or in Downloads and run `TinyTranscript setup` again";

/// Google Drive, optional: Ok(false) if skipped.
fn step_google(io: &Io) -> Result<bool> {
    if !(io.in_keychain)(CLIENT_ITEM) {
        let mut file = (io.client_file)();
        let mut tries = 0;
        let client = loop {
            let f = match file.take() {
                Some(f) => f,
                None => {
                    let a = io.ask("Path to google-client.json (drag the file here; Return skips Google Drive): ");
                    if a.is_empty() {
                        println!("  – Google Drive skipped: {DRIVE_LATER}");
                        return Ok(false);
                    }
                    unescape_path(&a)
                }
            };
            match client_from_file(&f) {
                Ok(c) => break (c, f),
                Err(e) => {
                    todo(&format!("{}: {e:#}", f.display()));
                    tries += 1;
                    if tries >= 3 {
                        println!("  – Google Drive skipped: {DRIVE_LATER}");
                        return Ok(false);
                    }
                }
            }
        };
        if dry() {
            ok(&format!("[dry run] would store the Google client from {} in the keychain", client.1.display()));
        } else {
            (io.store_client)(&client.0)?;
            ok(&format!("Google client stored in the keychain (you can delete {})", client.1.display()));
        }
    }
    if (io.in_keychain)(LOGIN_ITEM) && io.ask("Already signed in to Google Drive. Sign in again? [y/N] ").to_lowercase() != "y" {
        ok("Google Drive: signed in");
        return Ok(true);
    }
    if dry() {
        ok("[dry run] would sign in to Google Drive in the browser");
        return Ok(true);
    }
    println!("  Signing in to Google Drive in your browser…");
    if !(io.sign_in)() {
        return Err(anyhow!("Google sign-in did not finish; run `TinyTranscript setup` again to sign in"));
    }
    ok("Google Drive: signed in. Drop recordings into the Drive folder 'Recordings inbox'");
    Ok(true)
}

fn step_macwhisper() {
    if !Path::new("/Applications/MacWhisper.app").exists() {
        todo("MacWhisper is not in /Applications: install it first (macwhisper.com or the App Store)");
    }
    let folder = common::recordings();
    use crate::workfolder::Watch;
    match crate::workfolder::macwhisper_watch(&folder) {
        Watch::Yes => ok("MacWhisper watches the recordings folder"),
        Watch::Unusable => todo(&format!(
            "MacWhisper cannot open its watch folder entry for {}: in MacWhisper > Settings > Watch Folder remove it and add the folder again",
            folder.display())),
        _ => todo(&format!("MacWhisper > Settings > Watch Folder: add {} (+, then choose the folder)", folder.display())),
    }
    match macwhisper_pref("watchFolderSubfolderDepth").map(|s| s.trim().to_string()) {
        Some(d) if d == "0" => ok("MacWhisper watches only the top level (subfolder depth 0)"),
        _ => todo("MacWhisper > Settings > Watch Folders: set subfolder depth to 0 (finished files move into subfolders)"),
    }
    match macwhisper_pref("defaultWatchFolderFormats") {
        Some(f) if f.contains("\"format\":\"txt\"") && f.contains("\"showSpeakerNames\":true") => {
            ok("MacWhisper exports .txt with speaker names")
        }
        _ => todo("MacWhisper > Settings > Watch Folders: export as .txt, with speaker names"),
    }
    println!("    Name the voices you know in MacWhisper (People); recordings then get their names.");
}

fn step_claude() {
    if common::claude::available() {
        ok("Claude Code found: free-form Drive file names are read and untitled recordings get titles");
    } else {
        println!("  – Claude Code not installed (optional): recordings keep their file names; with it");
        println!("    (https://claude.com/claude-code, then `claude` once to log in) they also get titles.");
    }
}

static DRY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// `setup --dry-run`: every question and check, but no keychain write, no sign-in, no launch agent.
fn dry() -> bool {
    DRY.load(std::sync::atomic::Ordering::Relaxed)
}

/// The interactive setup; the exit code.
pub fn run(dry_run: bool) -> i32 {
    let translocated = std::env::current_exe().unwrap_or_default().to_string_lossy().contains("/AppTranslocation/");
    if !dry_run && !translocated {
        let m = setup_marker();
        let _ = std::fs::create_dir_all(m.parent().unwrap());
        let _ = std::fs::write(&m, "");
    }
    let stdin = std::io::stdin();
    let mut lock = stdin.lock();
    let io = Io {
        input: RefCell::new(&mut lock),
        in_keychain: &common::in_keychain,
        client_file: &find_client_file,
        store_client: &|c: &Value| common::keychain_store(CLIENT_ITEM, &c.to_string()),
        sign_in: &|| drive_fetch::job(vec!["drive_fetch".into(), "--login".into()]) == 0 && common::in_keychain(LOGIN_ITEM),
        install_agent: &install_agent,
    };
    run_with(dry_run, &io)
}

/// The setup with its effects in `io`. Only a missing work folder, an app not yet moved out of
/// App Translocation, or a failed launch agent stop it (exit 1); optional steps that are skipped
/// or fail are listed at the end, after the launch agent is installed.
pub fn run_with(dry_run: bool, io: &Io) -> i32 {
    DRY.store(dry_run, std::sync::atomic::Ordering::Relaxed);
    println!("TinyTranscript setup{}\n", if dry_run { " (dry run: nothing is changed)" } else { "" });
    // macOS runs a downloaded app that was not moved yet from a temporary copy (App Translocation);
    // the launch agent must point at where the app stays.
    let exe = std::env::current_exe().unwrap_or_default();
    if exe.to_string_lossy().contains("/AppTranslocation/") {
        todo("first drag TinyTranscript.app into your Applications folder, then open it from there");
        return 1;
    }
    println!("Folders");
    if dry() {
        ok(&format!("[dry run] recordings folder: {}", common::recordings().display()));
    } else if let Err(e) = step_folders() {
        todo(&format!("{e:#}"));
        return 1;
    }
    let mut unmet: Vec<String> = Vec::new();
    println!("Your name");
    if let Err(e) = step_name(io) {
        todo(&format!("{e:#}"));
        unmet.push(format!("your name: {e:#}"));
    }
    println!("Google Drive (optional)");
    match step_google(io) {
        Ok(true) => {}
        Ok(false) => unmet.push("Google Drive: skipped".into()),
        Err(e) => {
            todo(&format!("{e:#}"));
            unmet.push(format!("Google Drive: {e:#}"));
        }
    }
    println!("MacWhisper");
    step_macwhisper();
    println!("Claude");
    step_claude();
    println!("Start");
    if dry() {
        ok(&format!("[dry run] would write {} for {}", agent_plist_path().display(), exe.display()));
    } else if let Err(e) = (io.install_agent)() {
        todo(&format!("{e:#}"));
        return 1;
    } else {
        ok("TinyTranscript runs in the menu bar (speech bubble) and starts at every login");
        println!("\nmacOS asks once whether TinyTranscript may access your Downloads folder: click Allow.");
    }
    if !unmet.is_empty() {
        println!("\nNot set up (TinyTranscript runs without it; `TinyTranscript setup` again adds it):");
        for u in &unmet {
            println!("  - {u}");
        }
    }
    println!("{}", if dry() { "\nDry run done." } else { "\nDone. You can close this window." });
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn with_root<T>(f: impl FnOnce(&Path) -> T) -> T {
        let root = std::env::temp_dir().join(format!("tt-setup-{}-{:?}", std::process::id(), std::thread::current().id()));
        std::fs::create_dir_all(&root).unwrap();
        std::env::set_var("RECORDINGS_ROOT", &root);
        let r = f(&root);
        std::env::remove_var("RECORDINGS_ROOT");
        let _ = std::fs::remove_dir_all(&root);
        r
    }

    /// No Google client anywhere and nothing typed (closed stdin): setup skips Drive and still
    /// installs the launch agent.
    #[test]
    fn finishes_without_google_drive() {
        let _g = crate::TEST_ENV.lock().unwrap_or_else(|e| e.into_inner());
        with_root(|_| {
            for input in ["", "\n\n", "\n/no/such/google-client.json\n/nope\n/nope again\n"] {
                let installed = Cell::new(false);
                let mut inp = input.as_bytes();
                let io = Io {
                    input: RefCell::new(&mut inp),
                    in_keychain: &|_| false,
                    client_file: &|| None,
                    store_client: &|_| panic!("nothing to store"),
                    sign_in: &|| panic!("no sign-in without a client"),
                    install_agent: &|| { installed.set(true); Ok(()) },
                };
                assert_eq!(run_with(false, &io), 0, "input {input:?}");
                assert!(installed.get(), "launch agent installed for input {input:?}");
            }
        });
    }

    /// A failed Google sign-in does not stop the setup either; the dry run asks the same
    /// questions and changes nothing.
    #[test]
    fn sign_in_failure_and_dry_run() {
        let _g = crate::TEST_ENV.lock().unwrap_or_else(|e| e.into_inner());
        with_root(|root| {
            let client = root.join("my client.json");
            std::fs::write(&client, r#"{"installed": {"client_id": "id", "client_secret": "s"}}"#).unwrap();
            let typed = format!("\n{}\n", client.to_string_lossy().replace(' ', "\\ "));
            let (stored, installed) = (Cell::new(false), Cell::new(false));
            let mut inp = typed.as_bytes();
            let io = Io {
                input: RefCell::new(&mut inp),
                in_keychain: &|_| false,
                client_file: &|| None,
                store_client: &|c| { assert_eq!(c["client_id"], "id"); stored.set(true); Ok(()) },
                sign_in: &|| false,
                install_agent: &|| { installed.set(true); Ok(()) },
            };
            assert_eq!(run_with(false, &io), 0);
            assert!(stored.get() && installed.get());

            let mut inp = typed.as_bytes();
            let io = Io {
                input: RefCell::new(&mut inp),
                in_keychain: &|_| false,
                client_file: &|| None,
                store_client: &|_| panic!("dry run stores nothing"),
                sign_in: &|| panic!("dry run signs nothing in"),
                install_agent: &|| panic!("dry run installs nothing"),
            };
            assert_eq!(run_with(true, &io), 0);
            assert!(inp.is_empty(), "the dry run read the same answers");
            DRY.store(false, std::sync::atomic::Ordering::Relaxed);
        });
    }

    #[test]
    fn unescapes_dragged_paths() {
        assert_eq!(unescape_path("/a/My\\ Folder/x.json"), PathBuf::from("/a/My Folder/x.json"));
        assert_eq!(unescape_path("'/a/My Folder/x.json'"), PathBuf::from("/a/My Folder/x.json"));
        assert_eq!(unescape_path("\"/a/b.json\" "), PathBuf::from("/a/b.json"));
    }
}
