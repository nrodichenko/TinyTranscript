#!/bin/zsh
# Build TinyTranscript.app for someone else's Mac (Drive inbox only; Apple silicon):
#
#   ./package.sh                         dist/TinyTranscript-<version>.zip: the app and "Read me first.txt"
# and dist/TinyTranscript-<version>-update.zip (the app alone; release.sh publishes it).
#   ./package.sh --with-google-client    also google-client.json, the Google OAuth client from
#                                        this Mac's keychain, which the Drive sign-in needs
#
# The client ID and secret identify the app to Google (for an installed app Google does not treat
# the secret as confidential), not anyone's account; each person still signs in with their own
# Google account. In the client's Cloud project the person must be a test user (or the app "In
# production"). Without the flag, send them google-client.json another way.
#
# The app is signed with the local "Recordings Pipeline (local)" certificate, which other Macs do
# not know: on the first open macOS asks to confirm it (System Settings > Privacy & Security >
# Open Anyway). Nothing about this Mac (config, logs, logins, voiceprints) goes into the package.
set -euo pipefail
REPO=${0:A:h}
OUT="$REPO/dist/TinyTranscript"
source "$REPO/version.sh"   # one version for both builds below

# own build folder, with this Mac's paths (user name) taken out of the program
T="$REPO/target/dist"
RUSTFLAGS="--remap-path-prefix=$HOME=/build" cargo build --release --manifest-path "$REPO/Cargo.toml" --target-dir "$T"
swiftc -O "$REPO/vpembed.swift" -o "$T/vpembed"
rm -rf "$OUT"
mkdir -p "$OUT"
"$T/release/make_app" "$OUT/TinyTranscript.app" "$T/release/recordings_menubar" "$T/vpembed" --no-register
cp "$REPO/RECIPIENT.txt" "$OUT/Read me first.txt"
if [[ "${1:-}" == "--with-google-client" ]]; then
  umask 077
  security find-generic-password -s google-drive-recordings-client -w > "$OUT/google-client.json"
fi
ZIP="TinyTranscript-$TT_VERSION.zip"
(cd "$REPO/dist" && rm -f "$ZIP" && ditto -c -k --norsrc --noextattr --keepParent "TinyTranscript" "$ZIP")
# the app alone, for the updater (release.sh publishes it; no Google client in it)
UPDATE="TinyTranscript-$TT_VERSION-update.zip"
(cd "$OUT" && rm -f "$REPO/dist/$UPDATE" && ditto -c -k --norsrc --noextattr --keepParent "TinyTranscript.app" "$REPO/dist/$UPDATE")
echo "packaged: $REPO/dist/$ZIP (to give, first install) and $REPO/dist/$UPDATE (update)"
