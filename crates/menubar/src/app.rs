//! The menu bar entry: status item, menu, 3-second refresh, ready notifications
//! (port of the `App` class in recordings_menubar.py).
//!
//! Telegram: the Python app runs a Telethon listener inside the menu bar process. This build has
//! no Telegram client (no MTProto); its row says "not set up" without the API keychain items and
//! "not supported" otherwise, with no buttons.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, Sel};
use objc2::{define_class, msg_send, sel, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSControlStateValueOff, NSControlStateValueOn, NSImage, NSMenu,
    NSMenuItem, NSStatusBar, NSStatusItem, NSVariableStatusItemLength,
};
use objc2_foundation::{NSRunLoop, NSRunLoopCommonModes, NSString, NSTimer};
use pipeline_common::{friendly_time, home, last_line};

use crate::icons;
use crate::notify::Notifier;
use crate::rows::{ButtonSpec, Callback, Header, Row, RowIcon};
use crate::status::{self, head, is_stamped, tail};

/// (name, launchd label, log file name in <meta>).
pub const JOBS: [(&str, &str, &str); 3] = [
    ("Speakers", "local.speaker-matching", "match.log"),
    ("Zoom", "local.zoom-recordings", "zoom.log"),
    ("Drive", "local.drive-recordings", "drive.log"),
];
const HEARTBEATS: [(&str, &str); 2] = [("local.zoom-recordings", ".zoom-checked"), ("local.drive-recordings", ".drive-checked")];
/// Telegram row states. The listener states of the Python app are kept for the column width.
const TG_LABELS: [(&str, &str); 8] = [
    ("starting", "starting…"),
    ("listening", "on"),
    ("downloading", "downloading…"),
    ("paused", "paused"),
    ("needs-login", "not logged in"),
    ("no-credentials", "not set up"),
    ("error", "error"),
    ("unsupported", "not supported"),
];
const TG_UNSUPPORTED_TIP: &str = "This build of the menu bar app has no Telegram listener: Telegram audio is not \
fetched. Send recordings through the Drive inbox, or run the Python menu bar app for Telegram.";
const TG_NO_CREDENTIALS_TIP: &str = "add telegram-api-id / telegram-api-hash to the keychain";

fn tg_label(status: &str) -> &'static str {
    TG_LABELS.iter().find(|(k, _)| *k == status).map(|(_, v)| *v).unwrap_or("error")
}

/// Where everything is. `icon_dir` is the PNG cache for the status icons and the Claude glyph.
#[derive(Clone)]
pub struct Paths {
    pub recordings: PathBuf,
    pub meta: PathBuf,
    pub processed: PathBuf,
    pub originals: PathBuf,
    pub appdir: PathBuf,
    pub settings: PathBuf,
    pub icon_dir: PathBuf,
    pub ready: PathBuf,
    pub mw_db: PathBuf,
}

impl Paths {
    pub fn live() -> Paths {
        let appdir = home().join("Library/Application Support/local.telegram-recordings");
        Paths {
            recordings: pipeline_common::recordings(),
            meta: pipeline_common::meta(),
            processed: pipeline_common::processed(),
            originals: pipeline_common::originals(),
            settings: appdir.join("menubar.json"), // {"icon": "<style key>"}
            appdir,
            icon_dir: home().join("Library/Caches/local.recordings-menubar"),
            ready: pipeline_common::meta().join("ready.jsonl"), // match_speakers appends one line per recording
            mw_db: home().join("Library/Application Support/MacWhisper/Database/main.sqlite"),
        }
    }
}

thread_local! {
    static APP: RefCell<Option<Rc<App>>> = const { RefCell::new(None) };
}

/// Run `f` on the app (no-op before it exists). The Rc is cloned out first, so `f` may re-enter.
fn with_app(f: impl FnOnce(&App)) {
    if let Some(app) = APP.with(|a| a.borrow().clone()) {
        f(&app);
    }
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; Controller does not implement Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    /// Target of the timer and of the plain menu items.
    pub(crate) struct Controller;

    impl Controller {
        #[unsafe(method(tick:))]
        fn tick(&self, _timer: &NSTimer) {
            with_app(|a| a.refresh());
            // outside with_app: it may show a dialog, during which this timer fires again
            crate::workfolder::after_start(self.mtm());
        }

        #[unsafe(method(openRecordings:))]
        fn open_recordings(&self, _sender: Option<&AnyObject>) {
            with_app(|a| open(&[a.paths.recordings.to_string_lossy().as_ref()]));
        }

        #[unsafe(method(openLogs:))]
        fn open_logs(&self, _sender: Option<&AnyObject>) {
            with_app(|a| open(&[a.paths.meta.to_string_lossy().as_ref()]));
        }

        #[unsafe(method(loginTelegram:))]
        fn login_telegram(&self, _sender: Option<&AnyObject>) {
            with_app(|a| {
                let py = a.paths.appdir.join("venv/bin/python");
                let script = a.paths.meta.join("telegram_login.py");
                in_terminal(&format!("'{}' '{}'", py.display(), script.display()));
            });
        }

        #[unsafe(method(loginGoogle:))]
        fn login_google(&self, _sender: Option<&AnyObject>) {
            with_app(|_| {
                // this same program runs the Drive job (TinyTranscript drive-fetch)
                let exe = std::env::current_exe().unwrap_or_default();
                in_terminal(&format!("'{}' drive-fetch --login", exe.display()));
            });
        }

        #[unsafe(method(openTranscript:))]
        fn open_transcript(&self, sender: &NSMenuItem) {
            if let Some(p) = crate::recent::transcript_path(sender.tag()) {
                open(&[p.to_string_lossy().as_ref()]);
            }
        }

        #[unsafe(method(renameSpeaker:))]
        fn rename_speaker(&self, sender: &NSMenuItem) {
            crate::recent::rename(self.mtm(), sender.tag());
        }

        #[unsafe(method(notInCall:))]
        fn not_in_call(&self, sender: &NSMenuItem) {
            crate::recent::not_in_call(sender.tag());
        }

        #[unsafe(method(editTitle:))]
        fn edit_title(&self, sender: &NSMenuItem) {
            crate::recent::edit_title(self.mtm(), sender.tag());
        }

        #[unsafe(method(renameSpeakerNew:))]
        fn rename_speaker_new(&self, sender: &NSMenuItem) {
            crate::recent::rename(self.mtm(), sender.tag());
        }

        #[unsafe(method(changeWorkFolder:))]
        fn change_work_folder(&self, _sender: Option<&AnyObject>) {
            crate::workfolder::change(self.mtm());
        }

        #[unsafe(method(requeue:))]
        fn requeue_action(&self, _sender: Option<&AnyObject>) {
            std::thread::spawn(|| {
                let n = crate::requeue::run(true);
                if n == 0 {
                    crate::update::notify("Nothing handed to MacWhisper",
                        "Every waiting recording is queued in MacWhisper, or a job is running; try again in a minute.");
                }
            });
        }

        #[unsafe(method(watchWorkFolder:))]
        fn watch_work_folder(&self, _sender: Option<&AnyObject>) {
            crate::workfolder::watch_work_folder(self.mtm());
        }

        #[unsafe(method(update:))]
        fn update_action(&self, _sender: Option<&AnyObject>) {
            crate::update::menu_action();
        }

        #[unsafe(method(setStyle:))]
        fn set_style_action(&self, sender: &NSMenuItem) {
            let i = sender.tag() as usize;
            if let Some((key, _)) = icons::styles().nth(i) {
                with_app(|a| a.set_style(key));
            }
        }

        #[unsafe(method(quit:))]
        fn quit(&self, _sender: Option<&AnyObject>) {
            NSApplication::sharedApplication(self.mtm()).terminate(None);
        }
    }
);

impl Controller {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        unsafe { msg_send![Self::alloc(mtm), init] }
    }
}

fn open(args: &[&str]) {
    let _ = Command::new("/usr/bin/open").args(args).output();
}

fn in_terminal(cmd: &str) {
    let _ = Command::new("/usr/bin/osascript")
        .args(["-e", &format!("tell application \"Terminal\" to do script \"{cmd}\""), "-e", "tell application \"Terminal\" to activate"])
        .output();
}

pub struct App {
    mtm: MainThreadMarker,
    pub paths: Paths,
    /// Preview: no status item, no notifications, no timer, nothing written outside `icon_dir`.
    preview: bool,
    controller: Retained<Controller>,
    status_item: Option<Retained<NSStatusItem>>,
    pub menu: Retained<NSMenu>,
    pub icon_menu: Retained<NSMenu>,
    style: RefCell<String>,
    icons: RefCell<HashMap<&'static str, Retained<NSImage>>>,
    state: Cell<&'static str>,
    style_items: Vec<Retained<NSMenuItem>>,
    pub rows: HashMap<&'static str, Row>,
    i_inbox: Retained<NSMenuItem>,
    i_login: Retained<NSMenuItem>,
    i_glogin: Retained<NSMenuItem>,
    recent_menu: Retained<NSMenu>,
    header: RefCell<Option<Header>>,
    i_update: Retained<NSMenuItem>,
    i_watch: Retained<NSMenuItem>,
    i_requeue: Retained<NSMenuItem>,
    recent_sig: RefCell<String>,
    notifier: Option<Notifier>,
    ready_offset: Cell<u64>,
    tg_paused: Cell<bool>,
    timer: RefCell<Option<Retained<NSTimer>>>,
}

pub(crate) fn menu_item(mtm: MainThreadMarker, title: &str, action: Option<Sel>, target: &Controller) -> Retained<NSMenuItem> {
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(title),
            action,
            &NSString::from_str(""),
        )
    };
    if action.is_some() {
        unsafe { item.setTarget(Some(target)) };
    }
    item
}

fn load_icons(dir: &Path, style: &str) -> HashMap<&'static str, Retained<NSImage>> {
    icons::make_icons(dir, style)
        .into_iter()
        .filter_map(|(state, p)| icons::load_image(&p, 20.0, true).map(|img| (state, img)))
        .collect()
}

fn widths() -> Vec<Vec<String>> {
    let names = ["Telegram", "MacWhisper", "Claude"].into_iter().chain(JOBS.iter().map(|j| j.0));
    let states = TG_LABELS.iter().map(|(_, v)| *v).chain(
        ["running", "idle", "paused", "not loaded", "transcribing", "not running", "not watching", "working", "stuck"],
    );
    let third = ["last 2026-11-22 00:00", "last Yesterday 00:00", "checked 2026-11-22 00:00", "checked 59 min ago"];
    [names.collect::<Vec<_>>(), states.collect(), third.to_vec()]
        .into_iter()
        .map(|c| c.into_iter().map(String::from).collect())
        .collect()
}

fn cb(f: impl Fn(&App) + 'static) -> Callback {
    Rc::new(move || with_app(&f))
}

impl App {
    pub fn new(mtm: MainThreadMarker, paths: Paths, preview: bool) -> Rc<App> {
        let controller = Controller::new(mtm);
        let style = pipeline_common::load_json(&paths.settings)
            .and_then(|v| v.get("icon").and_then(|s| s.as_str()).map(String::from))
            .filter(|s| icons::is_style(s))
            .unwrap_or_else(|| icons::DEFAULT.to_string());
        let icon_set = load_icons(&paths.icon_dir, &style);

        // Icon submenu: every style with its preview, the current one checked
        let icon_menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str("Icon"));
        let mut style_items = Vec::new();
        for (i, (key, name)) in icons::styles().enumerate() {
            let item = menu_item(mtm, name, Some(sel!(setStyle:)), &controller);
            item.setTag(i as isize);
            item.setState(if key == style { NSControlStateValueOn } else { NSControlStateValueOff });
            item.setImage(icons::style_image(&paths.icon_dir, key, 16.0).as_deref());
            icon_menu.addItem(&item);
            style_items.push(item);
        }
        let i_icon = menu_item(mtm, "Icon", None, &controller);
        let recent_menu = NSMenu::new(mtm);
        let i_recent = menu_item(mtm, "Recent transcripts", None, &controller);
        i_recent.setSubmenu(Some(&recent_menu));
        i_icon.setSubmenu(Some(&icon_menu));

        let i_inbox = menu_item(mtm, "Inbox: …", None, &controller);
        let i_login = menu_item(mtm, "Log in to Telegram…", Some(sel!(loginTelegram:)), &controller);
        let i_glogin = menu_item(mtm, "Log in to Google Drive…", Some(sel!(loginGoogle:)), &controller);
        let open_rec = menu_item(mtm, "Open recordings folder", Some(sel!(openRecordings:)), &controller);
        let open_logs = menu_item(mtm, "Open logs", Some(sel!(openLogs:)), &controller);
        let i_update = menu_item(mtm, "Check for updates", Some(sel!(update:)), &controller);
        let i_folder = menu_item(mtm, "Change work folder…", Some(sel!(changeWorkFolder:)), &controller);
        let i_watch = menu_item(mtm, "Make MacWhisper watch the work folder…", Some(sel!(watchWorkFolder:)), &controller);
        i_watch.setHidden(true);
        let i_requeue = menu_item(mtm, "Hand waiting recordings to MacWhisper", Some(sel!(requeue:)), &controller);
        i_requeue.setHidden(true);
        i_folder.setToolTip(Some(&NSString::from_str(&format!("Now: {}", pipeline_common::recordings().display()))));
        let quit = menu_item(mtm, "Quit", Some(sel!(quit:)), &controller);

        // one row per source/job: symbol · name · state · last run, then its buttons
        let claude_icon = icons::app_glyph(
            Path::new("/Applications/Claude.app"),
            &paths.icon_dir.join("claude-glyph.png"),
            16.0,
        )
        .map(RowIcon::Image)
        .unwrap_or(RowIcon::Symbol("sparkles"));
        let mut order: Vec<(&'static str, RowIcon)> = vec![
            ("telegram", RowIcon::Symbol("paperplane")),
            ("local.zoom-recordings", RowIcon::Symbol("video")),
            ("local.drive-recordings", RowIcon::Symbol("icloud")),
            ("macwhisper", RowIcon::Symbol("waveform")),
            ("local.speaker-matching", RowIcon::Symbol("person.wave.2")),
            ("claude", claude_icon),
        ];
        let widths = widths();
        let mut rows = HashMap::new();
        let mut row_items = Vec::new();
        for (key, icon) in order.drain(..) {
            let item = menu_item(mtm, &format!("row {key}"), None, &controller);
            let buttons = match key {
                "telegram" => vec![
                    None,
                    Some(ButtonSpec { key: "pause", symbol: "pause.fill", tip: "Pause Telegram".into(), action: cb(|a| a.toggle_tg_pause()) }),
                ],
                "claude" => vec![None, None],
                "macwhisper" => vec![
                    None,
                    Some(ButtonSpec { key: "open", symbol: "macwindow", tip: "Open MacWhisper".into(), action: cb(|_| open(&["-a", "MacWhisper"])) }),
                ],
                label => {
                    let name = JOBS.iter().find(|j| j.1 == label).map(|j| j.0).unwrap_or(label);
                    let check = if label == "local.speaker-matching" { "Match speakers now".to_string() } else { format!("Check {name} now") };
                    vec![
                        Some(ButtonSpec { key: "run", symbol: "arrow.clockwise", tip: check, action: cb(move |a| a.kick(label)) }),
                        Some(ButtonSpec { key: "pause", symbol: "pause.fill", tip: format!("Pause {name}"), action: cb(move |a| a.toggle_job_pause(label)) }),
                    ]
                }
            };
            let row = Row::new(mtm, &item, icon, &widths, buttons, Some(cb(move |a| a.open_source(key))));
            rows.insert(key, row);
            row_items.push(item);
        }

        // sources (Telegram, Zoom, Drive), then the processing steps (MacWhisper, Speakers, Claude)
        // the top row: inbox count, and the build version on the right, as wide as the rows
        let width = rows.values().next().map(|r| r.view.frame().size.width).unwrap_or(320.0);
        let header = Header::new(mtm, width, "tray", crate::VERSION);
        i_inbox.setView(Some(&header.view));
        let header = Some(header);
        let menu = NSMenu::new(mtm);
        let sep = || NSMenuItem::separatorItem(mtm);
        menu.addItem(&i_inbox);
        menu.addItem(&sep());
        for item in &row_items[..3] {
            menu.addItem(item);
        }
        menu.addItem(&sep());
        for item in &row_items[3..] {
            menu.addItem(item);
        }
        menu.addItem(&sep());
        for item in [&i_recent, &i_login, &i_glogin, &i_watch, &i_requeue, &open_rec, &i_folder, &open_logs, &i_icon, &i_update] {
            menu.addItem(item);
        }
        menu.addItem(&sep());
        menu.addItem(&quit);
        for (item, sym) in [
            (&i_inbox, "tray"),
            (&i_recent, "person.2.badge.gearshape"),
            (&i_login, "person.badge.key"),
            (&i_glogin, "key"),
            (&open_rec, "folder"),
            (&open_logs, "doc.text"),
            (&i_update, "arrow.down.circle"),
            (&i_folder, "folder.badge.gearshape"),
            (&i_watch, "eye"),
            (&i_requeue, "arrow.clockwise"),
            (&i_icon, "paintbrush"),
            (&quit, "power"),
        ] {
            item.setImage(icons::symbol(sym, 13.0).as_deref());
        }

        let status_item = (!preview).then(|| {
            let item = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
            if let Some(button) = item.button(mtm) {
                button.setImage(icon_set.get("idle").map(|i| &**i));
                button.setToolTip(Some(&NSString::from_str("TinyTranscript")));
            }
            item.setMenu(Some(&menu));
            item
        });
        let notifier = if preview { None } else { Notifier::new(mtm, &paths.processed) };
        // announce only recordings finished from now on
        let ready_offset = fs::metadata(&paths.ready).map(|m| m.len()).unwrap_or(0);

        Rc::new(App {
            mtm,
            paths,
            preview,
            controller,
            status_item,
            menu,
            icon_menu,
            style: RefCell::new(style),
            icons: RefCell::new(icon_set),
            state: Cell::new("idle"),
            style_items,
            rows,
            i_inbox,
            i_login,
            i_glogin,
            recent_menu,
            header: RefCell::new(header),
            i_update,
            i_watch,
            i_requeue,
            recent_sig: RefCell::new(String::new()),
            notifier,
            ready_offset: Cell::new(ready_offset),
            tg_paused: Cell::new(false),
            timer: RefCell::new(None),
        })
    }

    /// Refresh now and every 3 s, in the common run-loop modes so it also runs while the menu is open.
    fn start(&self) {
        self.refresh();
        let timer = unsafe {
            NSTimer::timerWithTimeInterval_target_selector_userInfo_repeats(3.0, &self.controller, sel!(tick:), None, true)
        };
        unsafe { NSRunLoop::currentRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes) };
        *self.timer.borrow_mut() = Some(timer);
    }

    fn set_status_icon(&self, state: &str) {
        if let (Some(item), Some(img)) = (&self.status_item, self.icons.borrow().get(state)) {
            if let Some(button) = item.button(self.mtm) {
                button.setImage(Some(img));
            }
        }
    }

    pub fn set_style(&self, key: &str) {
        *self.style.borrow_mut() = key.to_string();
        *self.icons.borrow_mut() = load_icons(&self.paths.icon_dir, key);
        for (item, (k, _)) in self.style_items.iter().zip(icons::styles()) {
            item.setState(if k == key { NSControlStateValueOn } else { NSControlStateValueOff });
        }
        self.set_status_icon(self.state.get());
        if !self.preview {
            let _ = fs::write(&self.paths.settings, format!("{{\"icon\": {}}}\n", serde_json::Value::from(key)));
        }
    }

    /// Clicking a row: open the app, web folder or folder it stands for.
    pub fn open_source(&self, key: &str) {
        match key {
            "telegram" => open(&["-a", "Telegram"]),
            "local.zoom-recordings" => open(&["-a", "zoom.us"]),
            "macwhisper" => open(&["-a", "MacWhisper"]),
            "claude" => open(&[self.paths.meta.join("claude.log").to_string_lossy().as_ref()]), // what it was asked, how it went
            "local.drive-recordings" => {
                let folder = pipeline_common::load_json(&self.paths.meta.join("drive_config.json"))
                    .and_then(|v| v.get("folder_id").and_then(|f| f.as_str()).map(String::from))
                    .unwrap_or_default();
                let url = if folder.is_empty() {
                    "https://drive.google.com/".to_string()
                } else {
                    format!("https://drive.google.com/drive/folders/{folder}")
                };
                open(&[&url]);
            }
            _ => open(&[self.paths.processed.to_string_lossy().as_ref()]), // speaker matching: the named transcripts
        }
    }

    pub fn kick(&self, label: &str) {
        status::kick(label);
        self.refresh();
    }

    pub fn toggle_tg_pause(&self) {
        self.tg_paused.set(!self.tg_paused.get());
        self.refresh();
    }

    pub fn toggle_job_pause(&self, label: &str) {
        status::set_job_paused(label, !status::job_paused(label));
        self.refresh();
    }

    /// A notification for each recording match_speakers finished since the last look.
    fn announce_ready(&self) {
        let Ok(size) = fs::metadata(&self.paths.ready).map(|m| m.len()) else { return };
        let offset = self.ready_offset.get();
        if size <= offset {
            self.ready_offset.set(offset.min(size));
            return;
        }
        let mut chunk = Vec::new();
        let Ok(mut f) = fs::File::open(&self.paths.ready) else { return };
        if f.seek(SeekFrom::Start(offset)).is_err() || f.take(size - offset).read_to_end(&mut chunk).is_err() {
            return;
        }
        // only whole lines; a line still being written is read on the next refresh
        let Some(end) = chunk.iter().rposition(|&b| b == b'\n') else { return };
        chunk.truncate(end + 1);
        self.ready_offset.set(offset + chunk.len() as u64);
        let Some(notifier) = &self.notifier else { return };
        for line in String::from_utf8_lossy(&chunk).lines() {
            let Ok(ev) = serde_json::from_str::<serde_json::Value>(line) else { continue };
            let s = |k: &str| ev.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
            let people: Vec<String> = ev
                .get("people")
                .and_then(|p| p.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                .unwrap_or_default();
            let with = (!people.is_empty()).then(|| format!("with {}", people.join(", ")));
            notifier.deliver("Transcript ready", &s("title"), with.as_deref(), &self.paths.processed.join(s("name")));
        }
    }

    /// What the Telegram row shows (no listener in this build).
    fn telegram_status(&self) -> &'static str {
        if status::in_keychain("telegram-api-id") && status::in_keychain("telegram-api-hash") {
            "unsupported"
        } else {
            "no-credentials"
        }
    }

    /// The "Recent transcripts" submenu as indented text (preview), people lists shortened.
    pub fn recent_tree(&self) -> String {
        fn walk(menu: &NSMenu, depth: usize, out: &mut String) {
            let n = menu.numberOfItems();
            for i in 0..n {
                let Some(it) = menu.itemAtIndex(i) else { continue };
                if it.isSeparatorItem() {
                    continue;
                }
                if depth == 2 && i > 4 {
                    out.push_str(&format!("{}… {} more\n", "  ".repeat(depth + 1), n - i));
                    break;
                }
                let on = if it.state() == objc2_app_kit::NSControlStateValueOn { " ✓" } else { "" };
                out.push_str(&format!("{}{}{on}\n", "  ".repeat(depth + 1), it.title()));
                if let Some(sub) = it.submenu() {
                    walk(&sub, depth + 1, out);
                }
            }
        }
        let mut out = String::from("recent transcripts:\n");
        walk(&self.recent_menu, 0, &mut out);
        out
    }

    /// Show or hide a source/job row (its menu item is titled "row <key>").
    fn show_row(&self, key: &str, show: bool) {
        if let Some(item) = self.menu.itemWithTitle(&NSString::from_str(&format!("row {key}"))) {
            if item.isHidden() == show {
                item.setHidden(!show);
            }
        }
    }

    pub fn refresh(&self) {
        self.announce_ready();
        let sig = crate::recent::signature(&self.paths.meta);
        if *self.recent_sig.borrow() != sig {
            crate::recent::rebuild(&self.recent_menu, self.mtm, &self.controller, &self.paths.meta, &self.paths.processed);
            *self.recent_sig.borrow_mut() = sig;
        }
        let update_title = if crate::update::busy() {
            "Updating…".to_string()
        } else {
            crate::update::available().map_or("Check for updates".to_string(), |r| format!("Update to {}", r.version))
        };
        if self.i_update.title().to_string() != update_title {
            self.i_update.setTitle(&NSString::from_str(&update_title));
        }
        self.i_update.setHidden(crate::update::REPO.is_empty());
        for (title, text) in crate::update::take_messages().into_iter().chain(crate::recent::take_results()) {
            if let Some(n) = &self.notifier {
                n.deliver(&title, &text, None, &self.paths.processed);
            }
        }
        let p = &self.paths;
        let (mut working, mut error, mut paused) = (false, false, false);

        let tg = self.telegram_status();
        let row = &self.rows["telegram"];
        row.set(&["Telegram", tg_label(tg), ""]);
        let (sym, tip) = if self.tg_paused.get() { ("play.fill", "Resume Telegram") } else { ("pause.fill", "Pause Telegram") };
        row.set_button("pause", sym, tip);
        row.show_buttons(!matches!(tg, "no-credentials" | "needs-login" | "unsupported"));
        row.set_alert(tg == "error", None);
        row.set_tooltip(Some(if tg == "unsupported" { TG_UNSUPPORTED_TIP } else { TG_NO_CREDENTIALS_TIP }));
        // Telegram is not part of this build: its row only shows if Telegram API keys are added
        self.show_row("telegram", tg == "unsupported");
        paused |= self.tg_paused.get();

        for (name, label, logf) in JOBS {
            let running = status::job_running(label);
            let off = running.is_none() && status::job_paused(label);
            let line = last_line(&p.meta.join(logf));
            let when = if is_stamped(&line) { head(&line, 16) } else { String::new() };
            let failed = line.contains("ERROR") && running != Some(true);
            let state = if off {
                "paused"
            } else {
                match running {
                    None => "not loaded",
                    Some(true) => "running",
                    Some(false) if failed => "error",
                    Some(false) => "idle",
                }
            };
            let (connected, why) = status::job_connected(label, &p.meta);
            let state = if connected { state } else { why };
            // a Drive-only Mac has no Zoom row
            self.show_row(label, connected || label != "local.zoom-recordings");
            let row = &self.rows[label];
            row.show_buttons(connected && (running.is_some() || off));
            let beat = HEARTBEATS.iter().find(|h| h.0 == label).and_then(|h| status::checked_ago(&p.meta.join(h.1)));
            let third = beat.unwrap_or_else(|| if when.is_empty() { String::new() } else { format!("last {}", friendly_time(&when)) });
            row.set(&[name, state, &third]);
            let msg = tail(&line, 20).replacen("ERROR ", "", 1);
            row.set_alert(failed && connected, failed.then_some(msg.as_str()));
            let (sym, tip) = if off { ("play.fill", format!("Resume {name}")) } else { ("pause.fill", format!("Pause {name}")) };
            row.set_button("pause", sym, &tip);
            if let Some(run) = row.buttons.get("run") {
                run.setEnabled(!off);
            }
            working |= running == Some(true);
            error |= line.contains("ERROR");
            paused |= off;
        }

        let mw = status::macwhisper_status(&p.recordings, &p.originals, &p.mw_db);
        let row = &self.rows["macwhisper"];
        row.set(&["MacWhisper", mw.state, &mw.detail]);
        row.set_alert(matches!(mw.state, "not running" | "stuck"), mw.tip.as_deref());
        error |= mw.state == "stuck";
        working |= mw.state == "transcribing";
        if mw.state == "not running" {
            row.set_button("open", "play.fill", "Start MacWhisper");
        } else {
            row.set_button("open", "macwindow", "Open MacWhisper");
        }
        error |= mw.state == "not running"; // nothing gets transcribed until it runs again
        // MacWhisper must watch the work folder, or nothing arriving there is transcribed
        use crate::workfolder::Watch;
        let watch = crate::workfolder::macwhisper_watch(&p.recordings);
        let not_watching = matches!(watch, Watch::No | Watch::Unusable);
        self.i_watch.setHidden(!not_watching);
        // recordings waiting longer than they should: maybe MacWhisper never saw them arrive
        self.i_requeue.setHidden(not_watching || mw.state != "stuck");
        if not_watching && mw.state != "not running" {
            row.set(&["MacWhisper", "not watching", "the work folder"]);
            let tip = if watch == Watch::Unusable {
                format!("MacWhisper cannot open its watch folder entry for {}: remove it in MacWhisper > Settings > Watch Folder and add the folder again (subfolder depth 0)", p.recordings.display())
            } else {
                format!("In MacWhisper > Settings > Watch Folder, add {} (subfolder depth 0)", p.recordings.display())
            };
            row.set_alert(true, Some(&tip));
            error = true;
        } else if mw.state == "stuck" && crate::workfolder::macwhisper_log_cannot_open(&p.recordings) {
            let tip = format!("MacWhisper could not open its watch folder when it started: remove {} in MacWhisper > Settings > Watch Folder and add it again", p.recordings.display());
            row.set_alert(true, Some(&tip));
        }

        let (cl_state, cl_detail) = if pipeline_common::claude::available() {
            status::claude_status(&pipeline_common::claude::busy_file(), &pipeline_common::claude::log_file())
        } else {
            ("not installed", "optional".to_string())
        };
        let row = &self.rows["claude"];
        row.set(&["Claude", cl_state, &cl_detail]);
        row.set_alert(cl_state == "error", Some(&tail(&last_line(&pipeline_common::claude::log_file()), 20)));
        working |= cl_state == "working";
        error |= cl_state == "error";

        let waiting = status::files_in(&p.recordings).len();
        let title = if waiting > 0 { format!("Inbox: {waiting} file(s) waiting") } else { "Inbox: empty".to_string() };
        self.i_inbox.setTitle(&NSString::from_str(&title));
        if let Some(h) = &*self.header.borrow() {
            h.set_text(&title);
        }
        working |= waiting > 0;
        self.i_login.setHidden(tg != "needs-login");
        self.i_glogin.setHidden(!last_line(&p.meta.join("drive.log")).contains("login expired"));

        let state = if error || tg == "needs-login" {
            "error"
        } else if working {
            "working"
        } else if paused {
            "paused"
        } else {
            "idle"
        };
        if state != self.state.get() {
            self.state.set(state);
            self.set_status_icon(state);
        }
    }

    pub fn icon_state(&self) -> &'static str {
        self.state.get()
    }

    pub fn style(&self) -> String {
        self.style.borrow().clone()
    }
}

/// The menu bar app: runs until Quit.
pub fn run() {
    let mtm = MainThreadMarker::new().expect("must run on the main thread");
    let nsapp = NSApplication::sharedApplication(mtm);
    nsapp.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    let app = App::new(mtm, Paths::live(), false);
    APP.with(|a| *a.borrow_mut() = Some(app.clone()));
    app.start();
    nsapp.run();
}

/// Build the app for an off-screen preview (no status item, timer or notifications).
pub fn for_preview(mtm: MainThreadMarker, paths: Paths) -> Rc<App> {
    let app = App::new(mtm, paths, true);
    APP.with(|a| *a.borrow_mut() = Some(app.clone()));
    app
}
