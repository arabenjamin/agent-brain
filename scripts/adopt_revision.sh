#!/usr/bin/env bash
# Bring a runtime edit back into the repo (OUTSIDE_REVIEW_PLAN §9.4). Writes what
# the live workflow, profile or program says to its bundle file and shows the
# diff. It does not commit: review it, then commit, and git holds the history.
# Once the file matches what is live, the next start makes the file the owner.
#
#   scripts/adopt_revision.sh workflow <name> [path]
#   scripts/adopt_revision.sh profile  <name> [path]
#   scripts/adopt_revision.sh program  <name> [path]
#
# `path` is required for something with no file yet, if plugins/core is the wrong
# home for it. A comment in the file is not carried over; the diff shows that.
# BRAIN_URL (default http://localhost:3001); the key comes from scripts/brain_key.sh.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
kind="${1:-}"; name="${2:-}"; path="${3:-}"
if [ -z "$kind" ] || [ -z "$name" ]; then sed -n '2,12p' "$0"; exit 2; fi
H=()
KEY=$("$ROOT/scripts/brain_key.sh")
[ -n "$KEY" ] && H+=(-H "Authorization: Bearer $KEY")
out=$(curl -fsS "${H[@]}" "${BRAIN_URL:-http://localhost:3001}/api/revisions/$kind/$name/file")

PRIVATE="${PLUGINS_PRIVATE_HOST_DIR:-$ROOT/../agent-brain-personal}"
if [ -z "$path" ]; then
  said=$(jq -r .path <<<"$out")
  # A source is written relative to the repo, to the plugins directory, or (the
  # private bundle) as its path in the container.
  said="${said#/home/agent/plugins-private/}"
  case "$said" in /*) echo "Don't know where $said is on this machine: pass the path." >&2; exit 1 ;; esac
  path=""
  for try in "$ROOT/$said" "$ROOT/plugins/$said" "$PRIVATE/$said"; do
    [ -f "$try" ] && path="$try" && break
  done
  if [ -z "$path" ]; then
    [ "$(jq -r .new_file <<<"$out")" = "true" ] || { echo "Can't find $said: pass the path." >&2; exit 1; }
    path="$ROOT/$said"
  fi
fi
if [ "$(jq -r .new_file <<<"$out")" = "true" ]; then
  echo "No file yet; writing $path. If it belongs in another bundle, pass the path." >&2
elif [ "$(jq -r .matches_file <<<"$out")" = "true" ]; then
  echo "$kind '$name' already matches its file. Nothing to do."
  exit 0
fi
mkdir -p "$(dirname "$path")"
jq -r .yaml <<<"$out" > "$path"
echo "Wrote $path (live version $(jq -r .version <<<"$out")). Not committed."
# Nothing is staged: the index is the owner's.
if git -C "$(dirname "$path")" ls-files --error-unmatch "$path" >/dev/null 2>&1; then
  git -C "$(dirname "$path")" --no-pager diff -- "$path" || true
else
  echo "New file, not tracked yet:"; cat "$path"
fi
