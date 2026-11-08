#!/bin/bash
# Release build without this machine's absolute paths in the binary.
# Panic locations of dependencies (~/.cargo/registry/...), the standard library and this crate would
# otherwise carry /Users/<name>/... into the distributed app. Paths are remapped to neutral prefixes.
# Usage: scripts/build-release.sh [extra cargo build options]
set -euo pipefail
cd "$(dirname "$0")/.."
root="$(pwd -P)"
cargo_home="${CARGO_HOME:-$HOME/.cargo}"
rustup_home="${RUSTUP_HOME:-$HOME/.rustup}"
sep=$'\x1f'
# CARGO_ENCODED_RUSTFLAGS keeps paths with spaces intact (RUSTFLAGS would split them).
flags="--remap-path-prefix=${cargo_home}=/cargo${sep}--remap-path-prefix=${rustup_home}=/rustup${sep}--remap-path-prefix=${root}=/openspeech"
export CARGO_ENCODED_RUSTFLAGS="$flags"
# Marks the binary as built here; `--install-app` refuses binaries without it (they carry local paths).
export OPENSPEECH_RELEASE_BUILD=1
cargo build --release --locked "$@"
binary="target/release/openspeech"
if [ ! -f "$binary" ]; then
  echo "build-release: $binary was not built (a --target build puts it elsewhere)" >&2
  exit 1
fi
# Read the file itself (not `strings | grep -q`: with pipefail, grep stopping at the first hit makes
# strings die of SIGPIPE and the test would pass). -a also covers the link-edit segment.
leaks=(-e /Users/ -e "$HOME" -e "$root" -e shintaro -e /private/var/folders/ -e /private/tmp/)
if LC_ALL=C grep -a -i -F -q "${leaks[@]}" "$binary"; then
  echo "build-release: the binary still contains a local path or name:" >&2
  LC_ALL=C grep -a -i -F -o "${leaks[@]}" "$binary" | sort | uniq -c >&2 || true
  exit 1
fi
echo "build-release: $binary has no local paths"
