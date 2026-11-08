#!/bin/bash
# Runs the tests marked #[ignore = "モデルが要る"] (they need the recognition models on this Mac).
# The names are read from the source, so a newly marked test is picked up without editing this file.
# Other ignored tests (real API calls, bundle install) are never run by this script.
# Usage: scripts/test-with-models.sh [cargo test options, e.g. --release]
set -euo pipefail
cd "$(dirname "$0")/.."

names=()
while IFS= read -r name; do names+=("$name"); done < <(
  grep -rh -A3 '#\[ignore = "モデルが要る"\]' src tests | sed -nE 's/.*fn ([a-z0-9_]+)\(.*/\1/p' | sort -u
)
[ ${#names[@]} -gt 0 ] || { echo "no model tests found" >&2; exit 1; }
echo "model tests: ${names[*]}"
cargo test "$@" -- --ignored "${names[@]}"
