#!/bin/zsh
# Publish a new version: build it (package.sh) and make the GitHub release v<version> of this
# repository with the app-only zip and latest.json, which installed apps check every 6 hours.
#
#   ./release.sh            needs a clean, pushed working copy and the `gh` login
#   TT_RELEASE_NOTES=text   put before the list of commits in the release notes
#
# The first-install package (dist/TinyTranscript-<version>.zip, optionally with the Google client)
# is not published; give it to people directly.
set -euo pipefail
REPO=${0:A:h}
cd "$REPO"
[[ -z "$(git status --porcelain)" ]] || { echo "commit your changes first"; exit 1; }
git fetch -q origin
[[ "$(git rev-parse HEAD)" == "$(git rev-parse @{u})" ]] || { echo "push your commits first"; exit 1; }
source "$REPO/version.sh"
[[ -n "$TT_UPDATE_REPO" ]] || { echo "no GitHub remote"; exit 1; }
"$REPO/package.sh" "$@"
UPDATE="dist/TinyTranscript-$TT_VERSION-update.zip"
SHA=$(shasum -a 256 "$UPDATE" | cut -d' ' -f1)
printf '{"version": "%s", "zip": "%s", "sha256": "%s"}\n' "$TT_VERSION" "${UPDATE:t}" "$SHA" > dist/latest.json
LAST=$(git describe --tags --abbrev=0 2>/dev/null || true)
NOTES=$(git log --format='- %s' ${LAST:+$LAST..}HEAD)
[[ -n "${TT_RELEASE_NOTES:-}" ]] && NOTES="$TT_RELEASE_NOTES"$'\n\n'"$NOTES"
gh release create "v$TT_VERSION" "$UPDATE" dist/latest.json --repo "$TT_UPDATE_REPO" --target "$(git rev-parse HEAD)" \
  --title "TinyTranscript $TT_VERSION" --notes "${NOTES:-TinyTranscript $TT_VERSION}"
git fetch -q --tags origin
echo "released $TT_VERSION"
