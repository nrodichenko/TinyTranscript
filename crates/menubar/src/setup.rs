//! `TinyTranscript setup`: the one Terminal step on a new Mac (for a Drive-only user).
//!
//! 1. creates ~/Downloads/recordings and its speaker-matching folder (config, logs, state);
//! 2. asks your name as MacWhisper labels you, so it is left out of the '_with_' part of names;
//! 3. stores the Google OAuth client (google-client.json, from whoever gave you the app) in the
//!    login keychain and signs you in to Google Drive in the browser;
//! 4. checks MacWhisper's watch folder settings and whether Claude Code is installed (read only);
//! 5. installs the launch agent: Recordings starts now and at every login.
//!
//! `TinyTranscript install-agent` does only step 5 (install.sh uses it). Opening TinyTranscript.app from
//! the Finder before setup opens Terminal with `TinyTranscript setup`.

use anyhow::{anyhow, Context, Result};
use pipeline_common as common;
use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const AGENT_LABEL: &str = "local.recordings-menubar";
const CLIENT_ITEM: &str = "google-drive-recordings-client";
const LOGIN_ITEM: &str = "google-drive-recordings";
const MACWHISPER: &str = "com.goodsnooze.MacWhisper";

pub fn agent_plist_path() -> PathBuf {
    common::home().join("Library/LaunchAgents").join(format!("{AGENT_LABEL}.plist"))
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
    let errors = common::meta().join("launchd-errors.log");
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

/// Write the launch agent for this program and (re)start it.
pub fn install_agent() -> Result<()> {
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

fn ask(question: &str) -> String {
    print!("{question}");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
    line.trim().to_string()
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

/// google-client.json next to TinyTranscript.app, in Downloads, or in the current folder.
fn find_client_file() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let beside_app = exe.ancestors().find(|p| p.extension().is_some_and(|e| e == "app")).and_then(Path::parent).map(Path::to_path_buf);
    [beside_app, Some(common::home().join("Downloads")), std::env::current_dir().ok()]
        .into_iter().flatten().map(|d| d.join("google-client.json")).find(|p| p.is_file())
}

fn step_folders() -> Result<()> {
    std::fs::create_dir_all(common::meta())?;
    ok(&format!("recordings folder: {}", common::recordings().display()));
    Ok(())
}

fn step_name() -> Result<()> {
    let path = common::meta().join("config.json");
    let mut cfg = common::load_config(&path, &match_speakers::default_config(), true);
    let now: Vec<String> = cfg["self_names"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default();
    let hint = if now.is_empty() { String::new() } else { format!(" [{}]", now.join(", ")) };
    let name = ask(&format!("Your name as MacWhisper shows it for you in transcripts{hint}: "));
    if !name.is_empty() {
        cfg["self_names"] = json!([name]);
        common::save_json(&path, &cfg)?;
    }
    ok("your name is left out of file names ('..._with_<the others>')");
    Ok(())
}

fn step_google() -> Result<()> {
    if dry() {
        let client = if common::in_keychain(CLIENT_ITEM) { "in the keychain".to_string() }
            else { find_client_file().map_or("not found".into(), |f| f.display().to_string()) };
        ok(&format!("[dry run] Google client: {client}; signed in: {}", common::in_keychain(LOGIN_ITEM)));
        return Ok(());
    }
    if !common::in_keychain(CLIENT_ITEM) {
        let file = match find_client_file() {
            Some(f) => f,
            None => PathBuf::from(ask("Path to google-client.json (drag the file here): ").trim_matches(|c| c == '\'' || c == '"')),
        };
        let client = client_from_file(&file)?;
        common::keychain_store(CLIENT_ITEM, &client.to_string())?;
        ok(&format!("Google client stored in the keychain (you can delete {})", file.display()));
    }
    if common::in_keychain(LOGIN_ITEM) && ask("Already signed in to Google Drive. Sign in again? [y/N] ").to_lowercase() != "y" {
        ok("Google Drive: signed in");
        return Ok(());
    }
    println!("  Signing in to Google Drive in your browser…");
    if drive_fetch::job(vec!["drive_fetch".into(), "--login".into()]) != 0 || !common::in_keychain(LOGIN_ITEM) {
        return Err(anyhow!("Google sign-in did not finish; run `TinyTranscript setup` again"));
    }
    ok("Google Drive: signed in. Drop recordings into the Drive folder 'Recordings inbox'");
    Ok(())
}

fn step_macwhisper() {
    if !Path::new("/Applications/MacWhisper.app").exists() {
        todo("MacWhisper is not in /Applications: install it first (macwhisper.com or the App Store)");
    }
    let folder = common::recordings();
    let folder_s = folder.to_string_lossy().trim_end_matches('/').to_string();
    match macwhisper_pref("watchedFolders") {
        Some(w) if w.contains(&format!("\"path\":\"{folder_s}/\"")) || w.contains(&format!("\"path\":\"{folder_s}\"")) => {
            ok("MacWhisper watches the recordings folder")
        }
        _ => todo(&format!("MacWhisper > Settings > Watch Folders: add {folder_s}")),
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
    DRY.store(dry_run, std::sync::atomic::Ordering::Relaxed);
    println!("TinyTranscript setup\n");
    // macOS runs a downloaded app that was not moved yet from a temporary copy (App Translocation);
    // the launch agent must point at where the app stays.
    let exe = std::env::current_exe().unwrap_or_default();
    if exe.to_string_lossy().contains("/AppTranslocation/") {
        todo("first drag TinyTranscript.app into your Applications folder, then open it from there");
        return 1;
    }
    let steps: [(&str, &dyn Fn() -> Result<()>); 3] =
        [("Folders", &step_folders), ("Your name", &step_name), ("Google Drive", &step_google)];
    for (title, step) in steps {
        println!("{title}");
        if let Err(e) = step() {
            todo(&format!("{e:#}"));
            return 1;
        }
    }
    println!("MacWhisper");
    step_macwhisper();
    println!("Claude");
    step_claude();
    println!("Start");
    if dry() {
        ok(&format!("[dry run] would write {} for {}", agent_plist_path().display(), exe.display()));
        return 0;
    }
    match install_agent() {
        Ok(()) => {
            ok("TinyTranscript runs in the menu bar (speech bubble) and starts at every login");
            println!("\nmacOS asks once whether TinyTranscript may access your Downloads folder: click Allow.");
            println!("Done. You can close this window.");
            0
        }
        Err(e) => {
            todo(&format!("{e:#}"));
            1
        }
    }
}
