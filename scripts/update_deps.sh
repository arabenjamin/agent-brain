#!/usr/bin/env bash
# Bring dependencies (and optionally the compiler) up to date, prove it, and
# leave the result staged-ready. Cargo.lock is committed and the Docker build
# is --locked, so the image only ever gets versions this script tested.
#
#   scripts/update_deps.sh                 # latest semver-compatible crates
#   scripts/update_deps.sh --rust 1.99.0   # also move the pinned compiler
#
# On a failed build/lint/test the previous Cargo.lock and toolchain come back.
# Major-version bumps ("available: vX") are listed, never applied: each one is
# a code change, done on its own.
set -euo pipefail
cd "$(dirname "$0")/.."

rust=""
[ "${1:-}" = "--rust" ] && rust="${2:?--rust needs a version, e.g. 1.99.0}"

backup=$(mktemp -d)
cp Cargo.lock rust-toolchain.toml plugins/sandbox/rust/Dockerfile "$backup/"
restore() {
  echo "!! $1 failed: restoring the previous Cargo.lock and compiler pins" >&2
  cp "$backup/Cargo.lock" "$backup/rust-toolchain.toml" .
  cp "$backup/Dockerfile" plugins/sandbox/rust/Dockerfile
  exit 1
}

if [ -n "$rust" ]; then
  sed -i "s/^channel = \".*\"/channel = \"$rust\"/" rust-toolchain.toml
  # The Rust sandbox builds from its own context and names its own image.
  sed -i "s/^FROM rust:.*-slim-trixie$/FROM rust:$rust-slim-trixie/" plugins/sandbox/rust/Dockerfile
  echo "== compiler pinned to $rust (brain and Rust sandbox)"
fi
echo "== toolchain: $(rustc --version)"
newest=$({ rustup check 2>/dev/null || true; } | grep -m1 '^stable' | sed 's/.*available: //')
echo "   newest stable: ${newest:-this one}"

echo "== cargo update"
cargo update 2>&1 | grep -E '(Updating|Adding|Removing) [a-z]' | grep -v 'crates.io index' | sed 's/^ */   /' || echo "   nothing to update"

echo "== build";  cargo build --all-targets --quiet || restore build
echo "== clippy"; cargo clippy --all-targets --quiet -- -D warnings || restore clippy
echo "== tests";  cargo test --lib --quiet 2>&1 | grep -E '^test result' | awk '{p+=$4; f+=$6} END {print "   passed", p, "failed", f}'
cargo test --lib --quiet >/dev/null 2>&1 || restore tests

echo "== newer versions held back by version requirements (not applied)"
cargo update --dry-run --verbose 2>&1 | grep 'available:' | sed 's/^ */   /' || echo "   none"

rm -rf "$backup"
echo "== done. Next: docker compose up -d --build agent-brain, check /health,"
echo "   then commit Cargo.lock (and the compiler pins if they moved). A moved"
echo "   sandbox pin needs: docker compose build rust-sandbox"
