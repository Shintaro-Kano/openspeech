#!/bin/bash
# Fails when the tree contains a path of this machine: a home folder path, the repository's own path,
# or (outside CI) the user name of whoever runs it.
# Usage: scripts/check-public.sh [root]   (default: the repository root)
set -euo pipefail

root="${1:-$(cd "$(dirname "$0")/.." && pwd)}"
cd "$root"
root="$(pwd -P)"

paths=()
for p in src tests tools assets scripts site .github Cargo.toml README.md SECURITY.md THIRD-PARTY-NOTICES.md; do
  [ -e "$p" ] && paths+=("$p")
done

# Fixed strings, case-insensitive: this machine's home folder and the repository's path.
needles=(-e "${HOME%/}/" -e "$root")
# The user name alone only on a personal Mac (a CI runner's name is an ordinary word).
if [ -z "${CI:-}" ]; then
  needles+=(-e "$(id -un)")
fi

# -I skips binary files (icons, dylibs, the built helper).
hits="$( { grep -rnIiF --exclude=check-public.sh "${needles[@]}" "${paths[@]}" || true
           # Any home folder path with a name after it, other than /Users/me/ (made up in the tests).
           grep -rnIE --exclude=check-public.sh '/Users/[A-Za-z]' "${paths[@]}" | grep -v '/Users/me/' || true; } | sort -u)"

if [ -n "$hits" ]; then
  echo "check-public: paths of this machine in the tree:" >&2
  echo "$hits" >&2
  exit 1
fi
echo "check-public: 0 hits"
