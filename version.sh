# Sourced by install.sh and package.sh: export TT_VERSION=YYYYMMDD.N (N counts this day's builds,
# kept in .build-number, not tracked), unless it is already set (package.sh -> install.sh --build).
if [[ -z "${TT_VERSION:-}" ]]; then
  _day=$(date +%Y%m%d) _n=1
  if [[ -f "$REPO/.build-number" ]]; then
    read -r _d _c < "$REPO/.build-number"
    [[ "$_d" == "$_day" ]] && _n=$((_c + 1))
  fi
  echo "$_day $_n" > "$REPO/.build-number"
  export TT_VERSION="$_day.$_n"
fi
# Updates come from the GitHub releases of this repository (owner/name from the git remote).
if [[ -z "${TT_UPDATE_REPO:-}" ]]; then
  _url=$(git -C "$REPO" remote get-url origin 2>/dev/null || true)
  export TT_UPDATE_REPO=$(print -r -- "$_url" | sed -nE 's#^(https://github\.com/|git@github\.com:)([^/]+/[^/.]+)(\.git)?$#\2#p')
fi
