#!/usr/bin/env bash
# How much an agent has to read, in tokens: core, each plugin, the whole repo.
# Fails when core is over its budget in scripts/core_token_budget. Only tracked
# files count, so a scratch file changes nothing.
#
#   scripts/size_budget.sh            # table + check
#   scripts/size_budget.sh --files    # also the 15 largest core files
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

echo
row "repo" tokens "" ""
for part in crates plugins hbi-frontend project-docs devdocs; do
  row "$part" "$(tok "$(file_bytes "$part")")" "" ""
done
row "everything tracked" "$(tok "$(git ls-files -z | sum_bytes)")" "" ""

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
