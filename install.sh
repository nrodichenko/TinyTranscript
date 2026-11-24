#!/bin/zsh
# Build the Rust pipeline and install it on this Mac in place of the running one (Python or an
# older Rust build). To give the app to someone else, use ./package.sh instead.
#
#   ./install.sh            build, install TinyTranscript.app, (re)start it
#   ./install.sh --build    only build (target/release, including the vpembed helper)
#
# Installs:
#   ~/Applications/TinyTranscript.app      the menu bar app, which also runs Zoom, Drive and speaker
#                                      matching on its own timers: one process, one launch agent,
#                                      one Downloads permission ("TinyTranscript"); the vpembed voice
#                                      helper is inside it (Contents/Helpers)
#   ~/Library/LaunchAgents/local.recordings-menubar.plist    (written by `TinyTranscript install-agent`)
# The per-job launch agents of earlier installs (local.zoom-recordings, local.drive-recordings,
# local.speaker-matching) are stopped and removed.
# The app is signed with the "Recordings Pipeline (local)" certificate in the login keychain if it
# is there (see README), so a rebuild keeps the app's permissions.
# Logs, state, config, voiceprints and the Google/Zoom logins are not touched. Jobs paused in the
# menu stay paused (speaker-matching/.paused-jobs).
set -euo pipefail
REPO=${0:A:h}
META="$HOME/Downloads/recordings/speaker-matching"
APP="$HOME/Applications/TinyTranscript.app"
UIDN=$(id -u)
source "$REPO/version.sh"   # TT_VERSION=YYYYMMDD.N, shown in the menu

cargo build --release --manifest-path "$REPO/Cargo.toml"
VPEMBED="$REPO/target/release/vpembed"
[[ "$VPEMBED" -nt "$REPO/vpembed.swift" ]] || swiftc -O "$REPO/vpembed.swift" -o "$VPEMBED"
[[ "${1:-}" == "--build" ]] && exit 0

for label in local.zoom-recordings local.drive-recordings local.speaker-matching; do
  launchctl bootout "gui/$UIDN/$label" 2>/dev/null || true   # these jobs now run inside the app
  rm -f "$HOME/Library/LaunchAgents/$label.plist"
done
launchctl bootout "gui/$UIDN/local.recordings-menubar" 2>/dev/null || true
"$REPO/target/release/make_app" "$APP" "$REPO/target/release/recordings_menubar" "$VPEMBED"
"$APP/Contents/MacOS/TinyTranscript" install-agent
# the app's name before 2026-11-24 (same bundle id and signature, so macOS keeps the permission)
rm -rf "$HOME/Applications/Recordings.app" "$HOME/Applications/Tiny Transcript.app"
# separate job programs of earlier installs (each needed its own Downloads permission)
rm -f "$META/bin/zoom_fetch" "$META/bin/drive_fetch" "$META/bin/match_speakers"
echo "installed $TT_VERSION; the menu bar app is $APP"
