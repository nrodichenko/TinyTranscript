//! make_app: build TinyTranscript.app, the menu bar app's own identity (name, icon, bundle id).
//!
//!     make_app [OUTPUT.app [BINARY]] [--no-register]
//!
//! OUTPUT defaults to ~/Applications/TinyTranscript.app, BINARY to the `recordings_menubar` next to
//! this program. macOS shows notifications, Activity Monitor entries and permission prompts under
//! the app that runs the code; the bundle makes that "TinyTranscript":
//! - Contents/MacOS/TinyTranscript is a copy of the Rust `recordings_menubar` binary itself;
//! - the icon is the talking bubble from the menu bar icons, white on a dark rounded tile;
//! - LSUIElement: no Dock icon. Signed ad hoc after assembly;
//! - then registered with Launch Services (`lsregister -f`) so Finder and Notification Center
//!   pick up the icon; `--no-register` skips that (for test builds).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use menubar::icons;
use objc2::AllocAnyThread;
use objc2_app_kit::{NSAffineTransformNSAppKitAdditions, NSBezierPath, NSColor, NSCompositingOperation, NSGradient, NSGraphicsContext};
use objc2_foundation::{NSAffineTransform, NSPoint, NSRect, NSSize};

const BUNDLE_ID: &str = "local.recordings-pipeline";
/// The self-signed code-signing certificate in the login keychain (created 2026-11-24). Signed with
/// it, the app keeps one identity across rebuilds, so macOS keeps its Downloads permission; ad hoc
/// signing ("-") is the fallback, and then every rebuild counts as a new app.
const SIGN_IDENTITY: &str = "Recordings Pipeline (local)";

/// The certificate's SHA-1 if it is in the keychain (it is self-signed, so not "valid" for -v).
fn sign_identity() -> Option<String> {
    let out = Command::new("/usr/bin/security").args(["find-identity", "-p", "codesigning"]).output().ok()?;
    String::from_utf8_lossy(&out.stdout).lines()
        .find(|l| l.contains(&format!("\"{SIGN_IDENTITY}\"")))
        .and_then(|l| l.split_whitespace().nth(1).map(str::to_string))
        .filter(|h| h.len() == 40)
}

fn rect(x: f64, y: f64, w: f64, h: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))
}

/// Dark rounded tile (macOS icon grid: 824/1024 tile, ~185 radius) with the white bubble.
fn icon_png(path: &Path, px: f64) -> Result<()> {
    let scale = px / 1024.0;
    // glyph: drawn black on its own canvas at 28x (20 pt -> 560), then turned white
    let glyph = icons::bitmap(560.0, 560.0, scale);
    icons::draw_into(&glyph, || {
        let t = NSAffineTransform::transform();
        t.scaleBy(560.0 / 20.0);
        t.concat();
        NSColor::blackColor().set();
        icons::draw_glyph("bubble");
    });
    let glyph = icons::tinted(&glyph, 560.0, 560.0, scale, &NSColor::whiteColor());
    let rep = icons::bitmap(1024.0, 1024.0, scale);
    icons::draw_into(&rep, || {
        let tile = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(rect(100.0, 100.0, 824.0, 824.0), 185.0, 185.0);
        let top = NSColor::colorWithCalibratedRed_green_blue_alpha(0.27, 0.29, 0.34, 1.0);
        let bottom = NSColor::colorWithCalibratedRed_green_blue_alpha(0.09, 0.10, 0.12, 1.0);
        if let Some(g) = NSGradient::initWithStartingColor_endingColor(NSGradient::alloc(), &top, &bottom) {
            g.drawInBezierPath_angle(&tile, -90.0);
        }
        if let Some(ctx) = NSGraphicsContext::currentContext() {
            ctx.setCompositingOperation(NSCompositingOperation::SourceOver);
        }
        icons::draw_rep(&glyph, rect(232.0, 222.0, 560.0, 560.0), 1.0);
    });
    icons::save_png(&rep, path).with_context(|| format!("writing {}", path.display()))
}

fn run(cmd: &str, args: &[&str]) -> Result<()> {
    let out = Command::new(cmd).args(args).stdin(Stdio::null()).output().with_context(|| format!("running {cmd}"))?;
    if !out.status.success() {
        bail!("{cmd} failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

fn make_icns(dest: &Path, work: &Path) -> Result<()> {
    let iconset = work.join("AppIcon.iconset");
    let _ = fs::remove_dir_all(work);
    fs::create_dir_all(&iconset)?;
    for size in [16u32, 32, 128, 256, 512] {
        icon_png(&iconset.join(format!("icon_{size}x{size}.png")), size as f64)?;
        icon_png(&iconset.join(format!("icon_{size}x{size}@2x.png")), (size * 2) as f64)?;
    }
    let result = run("/usr/bin/iconutil", &["-c", "icns", &iconset.to_string_lossy(), "-o", &dest.to_string_lossy()]);
    let _ = fs::remove_dir_all(work);
    result
}

fn info_plist() -> String {
    let s = |k: &str, v: &str| format!("\t<key>{k}</key>\n\t<string>{v}</string>\n");
    let t = |k: &str| format!("\t<key>{k}</key>\n\t<true/>\n");
    [
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n".to_string(),
        "<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n".into(),
        "<plist version=\"1.0\">\n<dict>\n".into(),
        s("CFBundleDisplayName", "TinyTranscript"),
        s("CFBundleExecutable", "TinyTranscript"),
        s("CFBundleIconFile", "AppIcon"),
        s("CFBundleIdentifier", BUNDLE_ID),
        s("CFBundleName", "TinyTranscript"),
        s("CFBundlePackageType", "APPL"),
        s("CFBundleShortVersionString", menubar::VERSION),
        s("CFBundleVersion", menubar::VERSION),
        t("LSUIElement"),
        t("NSHighResolutionCapable"),
        "</dict>\n</plist>\n".into(),
    ]
    .concat()
}

fn main() -> Result<()> {
    let mut register = true;
    let mut pos = Vec::new();
    for a in std::env::args().skip(1) {
        match a.as_str() {
            "--no-register" => register = false,
            "-h" | "--help" => {
                println!("usage: make_app [OUTPUT.app [BINARY [VPEMBED]]] [--no-register]");
                return Ok(());
            }
            s if s.starts_with('-') => bail!("unknown option {s}"),
            _ => pos.push(PathBuf::from(a)),
        }
    }
    let app = pos.first().cloned().unwrap_or_else(|| pipeline_common::home().join("Applications/TinyTranscript.app"));
    let binary = match pos.get(1) {
        Some(b) => b.clone(),
        None => std::env::current_exe()?.with_file_name("recordings_menubar"),
    };
    if app.extension().and_then(|e| e.to_str()) != Some("app") {
        bail!("the output must end in .app: {}", app.display());
    }
    if !binary.is_file() {
        bail!("menu bar binary not found: {} (cargo build -p menubar --release)", binary.display());
    }
    let name = app.file_name().unwrap().to_string_lossy().into_owned();
    let tmp_app = app.with_file_name(format!("{name}.new"));
    let _ = fs::remove_dir_all(&tmp_app);
    fs::create_dir_all(tmp_app.join("Contents/MacOS"))?;
    fs::create_dir_all(tmp_app.join("Contents/Resources"))?;
    fs::copy(&binary, tmp_app.join("Contents/MacOS/TinyTranscript")).with_context(|| format!("copying {}", binary.display()))?;
    // the voice helper ships inside the app (Contents/Helpers), so a new Mac needs no Xcode tools
    let vpembed = pos.get(2).cloned().unwrap_or_else(|| pipeline_common::meta().join("bin/vpembed"));
    if vpembed.is_file() {
        fs::create_dir_all(tmp_app.join("Contents/Helpers"))?;
        fs::copy(&vpembed, tmp_app.join("Contents/Helpers/vpembed")).with_context(|| format!("copying {}", vpembed.display()))?;
    } else {
        eprintln!("make_app: no vpembed at {}; speaker matching will look in speaker-matching/bin", vpembed.display());
    }
    make_icns(&tmp_app.join("Contents/Resources/AppIcon.icns"), &app.with_file_name(format!("{name}.iconwork")))?;
    fs::write(tmp_app.join("Contents/Info.plist"), info_plist())?;
    let identity = sign_identity();
    if identity.is_none() {
        eprintln!("make_app: no '{SIGN_IDENTITY}' certificate in the keychain; signing ad hoc");
    }
    run("/usr/bin/codesign", &["--force", "--deep", "-s", identity.as_deref().unwrap_or("-"), &tmp_app.to_string_lossy()])?;
    let _ = fs::remove_dir_all(&app);
    fs::rename(&tmp_app, &app)?;
    if register {
        // make Finder / Notification Center pick up the new icon
        let _ = Command::new("/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister")
            .args(["-f", &app.to_string_lossy()])
            .output();
    }
    println!("built {}", app.display());
    Ok(())
}
