#!/usr/bin/env bash
# How much an agent has to read, in tokens: core, each plugin, the whole repo.
# Fails when core is over its budget in scripts/core_token_budget. Only tracked
# files count, so a scratch file changes nothing.
#
#   scripts/size_budget.sh            # table + check
#   scripts/size_budget.sh --files    # also the 15 largest core files
#
# It also reports, per unit (each core crate and plugin crate), its own code
# and its reach: own code plus the code of every workspace crate it depends on,
# transitively. That is the most a change to the unit can make an agent read.
# Units over scripts/unit_token_limits are flagged, not failed
# (CODEBASE_SPLIT_PLAN Phase 0). Then the growth of the Rust code by month,
# from git history (skipped in a shallow clone).
#
# Tokens are bytes / 4. Against real tokenizers that is within about 6% on Rust
# (cl100k_base 4.24 bytes a token, Gemma 3.70), and it needs nothing installed.
# Compare these numbers with each other, not with a model's bill.
#
# Core is crates/ plus plugins/core, the data core seeds. Tests don't count
# against the budget: everything from a file's first `#[cfg(test)]` line is a
# test, so nobody has to delete a test to make room.
set -euo pipefail
cd "$(dirname "$0")/.."
export LC_ALL=C

tokens() { echo $(($1 / 4)); }

# Prints "<code bytes> <test bytes>" for the tracked .rs files under a path.
rust_bytes() {
  git ls-files -z -- "$1" | grep -z '\.rs$' | xargs -0 -r awk '
    FNR == 1 { t = 0 }
    /#\[cfg\(test\)\]/ { t = 1 }
    { if (t) test += length($0) + 1; else code += length($0) + 1 }
    END { printf "%d %d\n", code + 0, test + 0 }' || true
}

# Sums the sizes of NUL-separated paths on stdin; xargs may run wc in batches.
sum_bytes() {
  xargs -0 -r wc -c | awk '$2 == "total" { n += $1 } END { print n + 0 }'
}

# Bytes of the tracked files under a path, leaving out .rs if asked.
file_bytes() {
  if [ "${2:-}" = norust ]; then
    git ls-files -z -- "$1" | grep -zv '\.rs$' | sum_bytes
  else
    git ls-files -z -- "$1" | sum_bytes
  fi
}

row() { printf "%-22s %9s %9s %9s\n" "$@"; }
tok() { printf "%d" "$(tokens "$1")"; }

row "core" code tests data
core_code=0
core_test=0
for dir in crates/*/; do
  read -r c t < <(rust_bytes "$dir")
  [ -n "${c:-}" ] || continue
  core_code=$((core_code + c))
  core_test=$((core_test + t))
  row "$(basename "$dir")" "$(tok "$c")" "$(tok "$t")" ""
done
core_data=$(file_bytes plugins/core)
row "plugins/core" "" "" "$(tok "$core_data")"
core_total=$(tokens $((core_code + core_data)))
row "CORE (code + data)" "$(tok "$core_code")" "$(tok "$core_test")" "$(tok "$core_data")"

echo
row "plugin" code tests data
for dir in plugins/*/; do
  name=$(basename "$dir")
  [ "$name" = core ] && continue
  c=0 t=0
  if [ -n "$(git ls-files -- "$dir" | grep '\.rs$' | head -n 1)" ]; then
    read -r c t < <(rust_bytes "$dir")
  fi
  row "$name" "$(tok "$c")" "$(tok "$t")" "$(tok "$(file_bytes "$dir" norust)")"
done

# Units: what one change must read. Edges are normal (not dev or build)
# path dependencies between workspace crates, from cargo metadata.
limit() { grep -E "^$1=" scripts/unit_token_limits | cut -d= -f2 | tr -d '[:space:]'; }
own_limit=$(limit own)
reach_limit=$(limit reach)
declare -A unit_code unit_deps
meta=$(cargo metadata --format-version 1 --no-deps --offline 2>/dev/null || cargo metadata --format-version 1 --no-deps)
while IFS=$'\t' read -r kind a b; do
  if [ "$kind" = unit ]; then
    read -r c _ < <(rust_bytes "$a/")
    unit_code[$a]=${c:-0}
  else
    unit_deps[$a]+="$b "
  fi
done < <(jq -r '.workspace_root as $r
  | .packages[] | (.manifest_path | ltrimstr($r + "/") | rtrimstr("/Cargo.toml")) as $u
  | (["unit", $u, ""] | @tsv),
    (.dependencies[] | select(.path != null and .kind == null)
     | ["dep", $u, (.path | ltrimstr($r + "/"))] | @tsv)' <<<"$meta")

# Own code of the unit and everything it reaches, each crate counted once.
reach_bytes() {
  local -A seen=()
  local stack=("$1") u d total=0
  while [ ${#stack[@]} -gt 0 ]; do
    u=${stack[-1]}
    unset 'stack[-1]'
    [ -n "${seen[$u]:-}" ] && continue
    seen[$u]=1
    total=$((total + ${unit_code[$u]:-0}))
    for d in ${unit_deps[$u]:-}; do
      [ -n "${unit_code[$d]:-}" ] && [ -z "${seen[$d]:-}" ] && stack+=("$d")
    done
  done
  echo "$total"
}

label() { case "$1" in crates/*) echo "${1#crates/}" ;; plugins/*/crate) echo "plugin ${1#plugins/}" | sed 's|/crate$||' ;; *) echo "$1" ;; esac; }

echo
row "unit" own reach ""
over=()
while read -r r u; do
  o=$(tokens "${unit_code[$u]}")
  rt=$(tokens "$r")
  flag=""
  [ "$o" -gt "$own_limit" ] && flag="own>$((own_limit / 1000))K"
  [ "$rt" -gt "$reach_limit" ] && flag="${flag:+$flag }reach>$((reach_limit / 1000))K"
  [ -n "$flag" ] && over+=("$(label "$u")")
  printf "%-22s %9s %9s  %s\n" "$(label "$u")" "$o" "$rt" "$flag"
done < <(for u in "${!unit_code[@]}"; do echo "$(reach_bytes "$u") $u"; done | sort -rn)
echo "limits: own $own_limit, reach $reach_limit (scripts/unit_token_limits; reported, not gated)."
echo "plugin contract: plugin-api reaches $(tokens "$(reach_bytes crates/plugin-api)") tokens, which every plugin reads."
echo "units over a limit: ${#over[@]} of ${#unit_code[@]}${over:+ (${over[*]})}"

echo
row "repo" tokens "" ""
for part in crates plugins hbi-frontend project-docs devdocs; do
  row "$part" "$(tok "$(file_bytes "$part")")" "" ""
done
row "everything tracked" "$(tok "$(git ls-files -z | sum_bytes)")" "" ""

# Growth: tracked Rust in crates/ and plugins/ (tests included) on the first
# of each of the last six months, and now.
echo
if [ "$(git rev-parse --is-shallow-repository)" = true ]; then
  echo "growth: not available in a shallow clone."
else
  echo "growth, Rust in crates/ + plugins/ incl. tests, tokens:"
  for m in 5 4 3 2 1 0; do
    d=$(date -u -d "$(date -u +%Y-%m-01) -$m month" +%Y-%m-01)
    c=$(git rev-list -1 --before="$d 00:00" HEAD)
    [ -n "$c" ] || continue
    b=$(git ls-tree -r -l "$c" -- crates plugins | awk '$5 ~ /\.rs$/ { s += $4 } END { print s + 0 }')
    printf "  %-10s %9d\n" "$d" "$(tokens "$b")"
  done
  b=$(git ls-files -z -- crates plugins | grep -z '\.rs$' | sum_bytes)
  printf "  %-10s %9d\n" "now" "$(tokens "$b")"
fi

if [ "${1:-}" = --files ]; then
  echo
  echo "largest core files, tokens to read each one whole:"
  git ls-files -z -- crates | grep -z '\.rs$' | xargs -0 wc -c | grep -v ' total$' | sort -rn | head -n 15 |
    awk '{ printf "%9d  %s\n", $1 / 4, $2 }'
fi

budget=$(grep -v '^#' scripts/core_token_budget | tr -d '[:space:]')
echo
if [ "$core_total" -gt "$budget" ]; then
  echo "core is $core_total tokens, $((core_total - budget)) over its budget of $budget."
  echo "Make room, or ask the owner to raise scripts/core_token_budget."
  exit 1
fi
echo "core is $core_total tokens, budget $budget ($((budget - core_total)) to spare)."
