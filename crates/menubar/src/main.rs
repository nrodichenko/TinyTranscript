//! recordings_menubar: the menu bar entry of the recordings pipeline.
//!
//!     recordings_menubar                        the menu bar app (normally TinyTranscript.app), which
//!                                               also runs every job on its own timer (jobs.rs)
//!     recordings_menubar zoom-fetch|drive-fetch|match-speakers [ARGS]
//!                                               run one job once by hand (--dry-run, --force, ...)
//!     recordings_menubar setup                  first-time setup on a new Mac (setup.rs)
//!     recordings_menubar install-agent          (re)install the launch agent for this program
//!     recordings_menubar --render-preview DIR   draw the menu (light/dark) and icons to PNGs in DIR
//!     recordings_menubar --icon-sheet FILE.png  every icon style in every state on one sheet
//!
//! Other arguments are ignored (a launch agent that still passes the Python script path works), except
//! unknown options, which print the usage instead of starting a second menu bar app.

use std::path::Path;

fn main() {
    let all: Vec<String> = std::env::args().collect();
    if all.get(1).is_some_and(|a| a == "--version") {
        println!("TinyTranscript {}", menubar::VERSION);
        return;
    }
    if let Some(job) = all.get(1) {
        // the job's own name as argv[0], so its usage and messages read as before
        let argv = |name: &str| std::iter::once(name.to_string()).chain(all[2..].iter().cloned()).collect();
        match job.as_str() {
            "zoom-fetch" => zoom_fetch::main_with(argv("zoom_fetch")),
            "drive-fetch" => drive_fetch::main_with(argv("drive_fetch")),
            "match-speakers" => match_speakers::main_with(argv("match_speakers")),
            // rename-speaker SESSION_HEX SPEAKER_HEX NAME TRANSCRIPT.txt (what the menu does; for testing)
            "rename-speaker" if all.len() == 6 => {
                match match_speakers::mw_write::rename_speaker(&all[2], &all[3], &all[4], &all[5]) {
                    Ok(r) => {
                        println!("'{}' -> '{}'{}{}", r.from, r.to, if r.new_person { " (new person)" } else { "" },
                                 if r.voiceprint_added { "; voiceprint added" } else { "" });
                        std::process::exit(0)
                    }
                    Err(e) => die(&format!("{e:#}")),
                }
            }
            // update [--check]: look for a newer release; without --check install it (and restart)
            "update" => {
                let check_only = all.get(2).is_some_and(|a| a == "--check");
                match menubar::update::check() {
                    Ok(None) => println!("up to date ({})", menubar::VERSION),
                    Ok(Some(r)) if check_only => println!("{} available (this is {})", r.version, menubar::VERSION),
                    Ok(Some(r)) => {
                        println!("installing {} …", r.version);
                        if let Err(e) = menubar::update::install(&r) {
                            die(&format!("{e:#}"));
                        }
                    }
                    Err(e) => die(&format!("{e:#}")),
                }
                std::process::exit(0)
            }
            "setup" => std::process::exit(menubar::setup::run(all.get(2).is_some_and(|a| a == "--dry-run"))),
            "install-agent" => match menubar::setup::install_agent() {
                Ok(()) => std::process::exit(0),
                Err(e) => die(&format!("{e:#}")),
            },
            _ => {}
        }
    }
    let args: Vec<String> = all[1..].to_vec();
    let value = |flag: &str| args.iter().position(|a| a == flag).map(|i| args.get(i + 1).cloned());
    if let Some(dir) = value("--render-preview") {
        let Some(dir) = dir else { die("--render-preview needs a folder") };
        if let Err(e) = menubar::preview::render(Path::new(&dir)) {
            die(&format!("{e:#}"));
        }
        return;
    }
    if let Some(file) = value("--icon-sheet") {
        let Some(file) = file else { die("--icon-sheet needs a file name") };
        if let Err(e) = menubar::icons::preview_sheet(Path::new(&file)) {
            die(&e.to_string());
        }
        return;
    }
    if let Some(a) = args.iter().find(|a| a.starts_with('-')) {
        die(&format!("unknown option {a}\nusage: recordings_menubar [--version | setup | install-agent | zoom-fetch|drive-fetch|match-speakers ARGS | --render-preview DIR | --icon-sheet FILE.png]"));
    }
    // Opened from the Finder before setup: run the setup in Terminal instead.
    if !menubar::setup::started_by_launchd() && !menubar::setup::agent_plist_path().exists() {
        menubar::setup::open_in_terminal();
        return;
    }
    // One menu bar entry: a second copy (started by hand next to the launch agent's) quits.
    let lock = pipeline_common::meta().join(".menubar.lock");
    let _single = match pipeline_common::try_lock(&lock) {
        Ok(Some(l)) => l,
        Ok(None) => die("already running"),
        Err(e) => die(&format!("{e:#}")),
    };
    menubar::jobs::start(); // Zoom, Drive and speaker matching run in this process
    menubar::update::start(); // looks for a newer release now and then
    menubar::app::run();
}

fn die(msg: &str) -> ! {
    eprintln!("recordings_menubar: {msg}");
    std::process::exit(2);
}
