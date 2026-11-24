//! Updates from the GitHub releases of the repository this build came from (TT_UPDATE_REPO,
//! "owner/name", set by version.sh from the git remote; empty: no updates).
//!
//! Each release (release.sh) has `latest.json` ({"version", "zip", "sha256"}) and the zip with the
//! app alone. The app looks a minute after it starts and then every 6 hours; a newer version shows
//! in the menu ("Update to …") and once as a notification. Installing: download, check the
//! SHA-256, unpack, check that the new app is signed with the same certificate and identifier as
//! this one (so only a build signed with the maker's key, which never leaves the maker's Mac, is
//! ever installed), swap it in place of this app and restart. The same signature keeps macOS's
//! permissions (Downloads) for the new version.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::Duration;

pub const REPO: &str = env!("TT_UPDATE_REPO");
const EVERY: Duration = Duration::from_secs(6 * 3600);

#[derive(Clone, Debug, PartialEq)]
pub struct Release {
    pub version: String,
    pub zip: String,
    pub sha256: String,
}

#[derive(Default)]
struct State {
    available: Option<Release>,
    announced: Option<String>,
    busy: bool,
    messages: Vec<(String, String)>,
}

static STATE: Mutex<State> = Mutex::new(State { available: None, announced: None, busy: false, messages: Vec::new() });

fn with<T>(f: impl FnOnce(&mut State) -> T) -> T {
    f(&mut STATE.lock().unwrap_or_else(|e| e.into_inner()))
}

fn say(title: &str, text: &str) {
    with(|s| s.messages.push((title.to_string(), text.to_string())));
}

/// "20261124.3" -> (20261124, 3); "dev" and anything else -> None.
pub fn parse_version(v: &str) -> Option<(u32, u32)> {
    let (d, n) = v.split_once('.')?;
    (d.len() == 8).then_some(())?;
    Some((d.parse().ok()?, n.parse().ok()?))
}

/// Whether `candidate` is newer than `current` (a "dev" build is never updated).
pub fn newer(candidate: &str, current: &str) -> bool {
    match (parse_version(candidate), parse_version(current)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new().timeout(Duration::from_secs(60)).redirects(5).user_agent("TinyTranscript-updater").build()
}

/// The latest release's latest.json.
pub fn latest() -> Result<Release> {
    if REPO.is_empty() {
        bail!("this build has no update source");
    }
    let url = format!("https://github.com/{REPO}/releases/latest/download/latest.json");
    let v: Value = agent().get(&url).call().map_err(|e| anyhow!("checking for updates: {e}"))?.into_json()?;
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string).ok_or_else(|| anyhow!("latest.json has no '{k}'"));
    let r = Release { version: s("version")?, zip: s("zip")?, sha256: s("sha256")?.to_lowercase() };
    if parse_version(&r.version).is_none() || r.zip.contains('/') || r.zip.contains("..") || r.sha256.len() != 64 {
        bail!("latest.json is not valid");
    }
    Ok(r)
}

/// Look now: Some(release) if it is newer than this build (also remembered for the menu).
pub fn check() -> Result<Option<Release>> {
    let r = latest()?;
    let newer = newer(&r.version, crate::VERSION);
    with(|s| s.available = newer.then(|| r.clone()));
    Ok(newer.then_some(r))
}

/// Look a minute after start, then every 6 hours; tell once per new version.
pub fn start() {
    if REPO.is_empty() || parse_version(crate::VERSION).is_none() {
        return;
    }
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(60));
        loop {
            if let Ok(Some(r)) = check() {
                let first = with(|s| (s.announced.as_deref() != Some(r.version.as_str())).then(|| s.announced = Some(r.version.clone())).is_some());
                if first {
                    say("Update available", &format!("TinyTranscript {} is ready to install (menu: Update to {}).", r.version, r.version));
                }
            }
            std::thread::sleep(EVERY);
        }
    });
}

pub fn available() -> Option<Release> {
    with(|s| s.available.clone())
}

pub fn busy() -> bool {
    with(|s| s.busy)
}

pub fn take_messages() -> Vec<(String, String)> {
    with(|s| std::mem::take(&mut s.messages))
}

/// The menu item: install the update found, or look for one now.
pub fn menu_action() {
    if with(|s| std::mem::replace(&mut s.busy, true)) {
        return;
    }
    std::thread::spawn(|| {
        let result = match available() {
            Some(r) => install(&r).map(|_| ()),
            None => match check() {
                Ok(Some(r)) => {
                    say("Update available", &format!("TinyTranscript {} is ready to install (menu: Update to {}).", r.version, r.version));
                    Ok(())
                }
                Ok(None) => {
                    say("TinyTranscript is up to date", &format!("Version {}", crate::VERSION));
                    Ok(())
                }
                Err(e) => Err(e),
            },
        };
        if let Err(e) = result {
            say("Update failed", &format!("{e:#}"));
        }
        with(|s| s.busy = false);
    });
}

/// The .app this program runs from.
fn running_app() -> Result<PathBuf> {
    let exe = std::env::current_exe()?.canonicalize()?;
    exe.ancestors().find(|p| p.extension().is_some_and(|e| e == "app")).map(Path::to_path_buf)
        .ok_or_else(|| anyhow!("not running from an app bundle"))
}

fn run(cmd: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(cmd).args(args).output().with_context(|| format!("running {cmd}"))?;
    if !out.status.success() {
        bail!("{cmd} failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
}

/// This app's designated requirement (identifier and signing certificate).
fn own_requirement(app: &Path) -> Result<String> {
    let out = run("/usr/bin/codesign", &["-d", "-r-", &app.to_string_lossy()])?;
    let req = out.lines().find_map(|l| l.trim().strip_prefix("designated => ")).map(str::to_string)
        .ok_or_else(|| anyhow!("this app has no designated requirement"))?;
    if req.contains("cdhash") {
        bail!("this build is signed ad hoc, so updates cannot be checked against its certificate");
    }
    Ok(req)
}

fn sha256_file(p: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut f = std::fs::File::open(p)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Download, check and install `r` in place of this app, then restart. Returns only on error
/// (or, when `restart` is false, after installing).
pub fn install(r: &Release) -> Result<PathBuf> {
    install_into(r, &running_app()?, true)
}

pub fn install_into(r: &Release, app: &Path, restart: bool) -> Result<PathBuf> {
    let req = own_requirement(app)?;
    let work = pipeline_common::home().join("Library/Caches/local.recordings-menubar/update");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work)?;
    let zip = work.join(&r.zip);
    let url = format!("https://github.com/{REPO}/releases/download/v{}/{}", r.version, r.zip);
    say("Updating TinyTranscript", &format!("Downloading {}…", r.version));
    let resp = agent().get(&url).call().map_err(|e| anyhow!("downloading {}: {e}", r.zip))?;
    let mut body = Vec::new();
    resp.into_reader().take(500 << 20).read_to_end(&mut body)?;
    std::fs::write(&zip, &body)?;
    if sha256_file(&zip)? != r.sha256 {
        bail!("the download does not match its checksum; nothing was installed");
    }
    let unpacked = work.join("unpacked");
    run("/usr/bin/ditto", &["-x", "-k", &zip.to_string_lossy(), &unpacked.to_string_lossy()])?;
    let new_app = unpacked.join("TinyTranscript.app");
    if !new_app.is_dir() {
        bail!("the update has no TinyTranscript.app");
    }
    run("/usr/bin/codesign", &["--verify", "--deep", "--strict", &new_app.to_string_lossy()])
        .context("the update's signature is not valid; nothing was installed")?;
    run("/usr/bin/codesign", &["--verify", &format!("-R=={req}"), &new_app.to_string_lossy()])
        .context("the update is not signed by the same certificate as this app; nothing was installed")?;
    let plist = new_app.join("Contents/Info.plist");
    let v = run("/usr/bin/plutil", &["-extract", "CFBundleShortVersionString", "raw", &plist.to_string_lossy()])?;
    if v.trim() != r.version {
        bail!("the update says it is {} instead of {}; nothing was installed", v.trim(), r.version);
    }
    // swap next to the app (same volume): new copy in, old one out, old one removed
    let dir = app.parent().ok_or_else(|| anyhow!("no folder"))?;
    let staged = dir.join(".TinyTranscript-update.app");
    let old = dir.join(".TinyTranscript-old.app");
    let _ = std::fs::remove_dir_all(&staged);
    let _ = std::fs::remove_dir_all(&old);
    run("/usr/bin/ditto", &[&new_app.to_string_lossy(), &staged.to_string_lossy()])?;
    std::fs::rename(app, &old).context("moving the old version aside")?;
    if let Err(e) = std::fs::rename(&staged, app) {
        let _ = std::fs::rename(&old, app); // put the old one back
        return Err(anyhow!(e).context("putting the new version in place"));
    }
    let _ = std::fs::remove_dir_all(&old);
    let _ = std::fs::remove_dir_all(&work);
    if restart {
        restart_app(app);
    }
    Ok(app.to_path_buf())
}

/// Start the new version: through the launch agent when it runs us, else with `open`.
fn restart_app(app: &Path) {
    if crate::setup::started_by_launchd() {
        let target = format!("gui/{}/{}", crate::status::uid(), crate::setup::AGENT_LABEL);
        let _ = Command::new("/bin/launchctl").args(["kickstart", "-k", &target]).spawn();
        std::thread::sleep(Duration::from_secs(10)); // launchd stops this process
    } else {
        let _ = Command::new("/bin/sh").arg("-c").arg(format!("sleep 1; /usr/bin/open '{}'", app.display())).spawn();
    }
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(parse_version("20261124.12"), Some((20261124, 12)));
        assert_eq!(parse_version("dev"), None);
        assert_eq!(parse_version("2026112.1"), None);
        assert!(newer("20261124.3", "20261124.2"));
        assert!(newer("20261125.1", "20261124.9"));
        assert!(newer("20261124.10", "20261124.9"));
        assert!(!newer("20261124.2", "20261124.2"));
        assert!(!newer("20261124.1", "20261124.2"));
        assert!(!newer("20261124.3", "dev"));
    }
}
