#!/usr/bin/env bash
# A plugin crate may depend on agent-brain-plugin-api and external crates
# only — never on the brain's app, repository, models or protocol crates
# (it reaches protocol through plugin-api's re-exports). CORE_AND_PLUGINS_PLAN
# §2.5. Run in CI; exits non-zero naming each offending dependency.
set -euo pipefail
cd "$(dirname "$0")/.."

out=$(mktemp)
trap 'rm -f "$out"' EXIT
cargo metadata --format-version 1 --no-deps | jq -r '
  .packages[]
  | select(.manifest_path | test("/plugins/[^/]+/crate/Cargo.toml$"))
  | . as $p
  | .dependencies[]
  | select(.path != null)
  | select(.name != "agent-brain-plugin-api")
  | "\($p.name) depends on \(.name) (\(.path))"' > "$out"

if [ -s "$out" ]; then
  echo "Plugin crates may depend only on agent-brain-plugin-api among the brain's crates:" >&2
  cat "$out" >&2
  exit 1
fi
n=$(cargo metadata --format-version 1 --no-deps | jq '[.packages[] | select(.manifest_path | test("/plugins/[^/]+/crate/Cargo.toml$"))] | length')
echo "plugin crates checked: $n, all depend only on agent-brain-plugin-api"
