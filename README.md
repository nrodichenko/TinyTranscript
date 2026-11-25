# TinyTranscript

A small macOS menu bar app that turns call recordings into transcripts with the speakers' names.

Recordings arrive from Zoom cloud recordings or from a Google Drive folder, MacWhisper transcribes
them, and TinyTranscript names the speakers by voice, gives each transcript a title and files it as
`<date>_-_<title>_with_<people>.txt`. Everything runs on your Mac except the optional title step
(Claude).

## What it does

- **Sources.** Zoom: finished cloud recordings of your account (audio only), every 10 minutes.
  Google Drive: audio or video dropped into the Drive folder "Recordings inbox", every minute; the
  file is moved to the Drive trash once it is fetched. Both are optional; a source that is not set
  up does not run and is not shown.
- **Transcription.** MacWhisper watches the work folder (`~/Downloads/recordings` by default) and writes a `.txt` with speaker
  labels next to each recording.
- **Speaker names.** For each transcript, every speaker's voice is compared with the voices of the
  people you have named in MacWhisper (MacWhisper's own voice model, run by the `vpembed` helper).
  A clear match is named, a close one gets "Name?", the rest stay "Speaker N".
- **Naming from the menu.** "Recent transcripts" lists the latest transcripts and their speakers;
  any speaker can be named from the people MacWhisper knows or with a new name. The name goes into
  MacWhisper's library (see below) together with the speaker's voiceprint, so later recordings are
  recognized at once.
- **Titles and files.** Recordings without a meaningful name get a short title written by Claude
  (Claude Code, `claude -p`, Haiku) from the transcript; free-form Drive file names are read the
  same way. The other people in the call are added to the name (`_with_Anna_Peter`, transliterated
  to Latin letters). Finished transcripts go to `processed/`, the audio to `processed/audio/`, and a
  notification opens the transcript.
- **Menu bar.** The state of every source and step, "check now" and "pause" buttons, recent
  transcripts, logs, and updates. The build version is shown at the top right.

## Requirements

- A Mac with Apple silicon, macOS 14 or later.
- [MacWhisper](https://goodsnooze.gumroad.com/l/macwhisper) with speaker recognition, its watch
  folder set to `~/Downloads/recordings` with subfolder depth 0 and `.txt` export with speaker
  names.
- For Drive: a Google account and an OAuth client ("Desktop app", Drive API) in a Google Cloud
  project, as `google-client.json`.
- For Zoom: a Zoom Server-to-Server OAuth app with `cloud_recording:read` scopes.
- Optional: [Claude Code](https://claude.com/claude-code), logged in, for titles.

## Installing

From a package (`TinyTranscript-<version>.zip`, made with `./package.sh`):

1. Drag `TinyTranscript.app` into Applications and open it. The app is signed with its maker's own
   certificate, not an Apple Developer ID: confirm it once in System Settings > Privacy & Security >
   Open Anyway.
2. A Terminal window runs `TinyTranscript setup`: your name as MacWhisper labels you (left out of
   file names), Google Drive (optional: the Google client, `google-client.json` next to the app or
   in Downloads, and the Drive sign-in; Return skips it), a check of MacWhisper's settings and of
   Claude Code, and the launch agent (the app starts at login). A skipped or failed optional step
   is listed at the end and does not stop the setup; `TinyTranscript setup` again adds it later.
   `TinyTranscript setup --dry-run` asks the same questions and changes nothing.
3. Allow TinyTranscript to access the Downloads folder when macOS asks.

Opening the app again after a setup that was started but not finished installs the launch agent
and starts the app. By hand: `/Applications/TinyTranscript.app/Contents/MacOS/TinyTranscript
install-agent` writes the launch agent (`~/Library/LaunchAgents/local.recordings-menubar.plist`)
and starts the app.

Zoom keys go into the login keychain: `security add-generic-password -U -a "$USER" -s
zoom-s2s-account-id -w` (and `zoom-s2s-client-id`, `zoom-s2s-client-secret`).

## Work folder

`~/Downloads/recordings` unless changed with "Change work folder…" in the menu (saved in
`~/Library/Application Support/TinyTranscript/settings.json`). Changing it can move the waiting
recordings, transcripts, settings and logs along, or start empty (choosing a folder that already
is a work folder switches to it); not while MacWhisper is transcribing.

MacWhisper then has to watch the new folder. It keeps each watch folder as a security-scoped
bookmark, which macOS lets only MacWhisper itself create, so the folder can only be added in
MacWhisper's Settings > Watch Folder (an entry written into its preferences by another app is
listed but never watched). TinyTranscript shows the steps, puts the path on the clipboard, opens
MacWhisper and says when the watch is in place. The menu warns when MacWhisper does not watch the
work folder or cannot open its entry for it, and "Make MacWhisper watch the work folder…" shows
the steps again. `TinyTranscript work-folder` prints both.

MacWhisper transcribes only files that arrive in its watch folder, not those already there when
it starts or when the folder is added. A recording that arrived while MacWhisper was not running
or could not open the folder would wait forever, so TinyTranscript moves such a recording out of
the work folder and back (once per MacWhisper run, after 2 minutes, never one MacWhisper's log
shows it has taken), which MacWhisper sees as a new arrival. "Hand waiting recordings to
MacWhisper" in the menu (shown when a recording waits too long) does it at once;
`TinyTranscript requeue --check` lists them.

## Updates

Each release of this repository (made with `./release.sh`) carries the app alone and a
`latest.json` with its version and SHA-256. The app asks the GitHub API for the latest release a minute after it
starts and every 6 hours; "Update to …" in the menu installs it. An update is installed only if its
checksum matches and it is signed with the same certificate and identifier as the running app, so
only builds signed with the maker's key are ever installed. The same signature keeps macOS's
permissions for the new version.

## Naming speakers in MacWhisper's library

MacWhisper has no API for renaming speakers (its `mw` command line and Shortcuts actions transcribe
and read only), so TinyTranscript writes the change into MacWhisper's database the way MacWhisper's
own rename does: the recording's lines and speaker link move from the "Speaker N" placeholder to
the person (a new name turns the placeholder into a new person). The speaker's voiceprint is added
in MacWhisper's own format and model. Before writing, the database structure is checked (nothing
is written if a MacWhisper update changed it); a copy of the database is kept once a day
(`macwhisper-backups/`, the last 3); every change is logged with what it replaced
(`macwhisper-writes.jsonl`). An open MacWhisper window shows the new name once it reloads that
recording.

## Files

Everything lives in `speaker-matching/` inside the work folder:

- `config.json` (speaker matching), `drive_config.json`, `zoom_config.json`: created with defaults
  on first run; each key is explained in its `_help` entry.
- `match.log`, `drive.log`, `zoom.log`, `claude.log`: what each step did; the menu bar shows the
  last line. Crashes: `~/Library/Logs/TinyTranscript/launchd-errors.log`.
- `state.json`, `reports/<name>.json` (every speaker's best matches), `originals/` (MacWhisper's
  unnamed transcripts), `voiceprints/` (a copy of MacWhisper's voiceprints and a log of changes).

Logins are in the login keychain only.

## Privacy

Audio, transcripts and voiceprints stay on your Mac. With Claude Code installed, transcripts (for
titles) and Drive file names are sent to Anthropic under your Claude account; the call runs without
tools, connectors or saved sessions, and the text is treated as data. Set `title_generator` and
`name_reader` to `none` to keep everything local.

## Building

Rust (`brew install rust`) and Xcode's command line tools.

- `./install.sh`: build and install `~/Applications/TinyTranscript.app` on this Mac, write its
  launch agent and restart it. `./install.sh --build` only builds.
- `./package.sh [--with-google-client]`: `dist/TinyTranscript-<version>.zip` to give to someone
  (with `google-client.json` from this Mac's keychain if asked) and the app-only update zip.
- `./release.sh`: publish a release with the update zip (clean, pushed working copy; `gh` login).
- `cargo test`: unit tests.

Each build gets a version `YYYYMMDD.N` (N counts the day's builds). Signing: `make_app` signs with a
certificate named "Recordings Pipeline (local)" from the login keychain if there is one, so macOS
keeps the app's permissions across versions; without it, the app is signed ad hoc and updates are
off. To make one: `openssl req -x509 -newkey rsa:2048 -nodes -days 7300 -subj "/CN=Recordings
Pipeline (local)" -addext "extendedKeyUsage=critical,codeSigning" ...`, then `security import` the
`.p12` with `-T /usr/bin/codesign` and answer the first signing prompt with "Always Allow".

### Layout

- `crates/common`: paths, logs, JSON state and config, locks, keychain, the Claude call.
- `crates/zoom-fetch`, `crates/drive-fetch`, `crates/match-speakers`: the jobs (also standalone
  binaries for development); `match-speakers/src/mw_write.rs` writes speaker names into MacWhisper.
- `crates/menubar`: the app (AppKit through objc2): menu, job scheduler (`jobs.rs`), setup,
  recent transcripts, updater; `make_app` builds the bundle.
- `vpembed.swift`: runs MacWhisper's speaker embedding model on given speech spans.

The app's one process runs every job on its own thread: `TinyTranscript zoom-fetch`,
`drive-fetch` or `match-speakers` runs one by hand (`--dry-run`, `--force` …).
