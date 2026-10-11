#!/usr/bin/env bash
# Lines of Rust per crate, split into code and inline tests (everything after a
# `#[cfg(test)]` line counts as test). Fails when a crate's code lines exceed
# its hard limit. Budget: project-docs/CORE_AND_PLUGINS_PLAN.md §3.
#
#   scripts/size_budget.sh            # table + check
#   HARD_LIMIT=8000 scripts/size_budget.sh
#
# Phase 6f split `app` into crates under the limit; nothing is exempt by default.
set -euo pipefail
cd "$(dirname "$0")/.."

HARD_LIMIT="${HARD_LIMIT:-8000}"
EXEMPT="${EXEMPT:-}"

count() {
  find "$1" -name '*.rs' -print0 | xargs -0 awk '
    FNR == 1 { t = 0 }
    /#\[cfg\(test\)\]/ { t = 1 }
    { if (t) test++; else code++ }
    END { printf "%d %d\n", code + 0, test + 0 }'
}

printf "%-24s %8s %8s %8s\n" crate code tests total
fail=0
tc=0
tt=0
for dir in crates/*/ plugins/*/crate/; do
  [ -d "$dir" ] || continue
  name=$(basename "${dir%/crate/}")
  read -r code test < <(count "$dir")
  tc=$((tc + code))
  tt=$((tt + test))
  flag=""
  if [ "$code" -gt "$HARD_LIMIT" ]; then
    if [[ " $EXEMPT " == *" $name "* ]]; then flag="(over, exempt)"; else flag="OVER ${HARD_LIMIT}"; fail=1; fi
  fi
  printf "%-24s %8d %8d %8d %s\n" "$name" "$code" "$test" "$((code + test))" "$flag"
done
printf "%-24s %8d %8d %8d\n" TOTAL "$tc" "$tt" "$((tc + tt))"

if [ -d hbi-frontend/src ]; then
  fe=$(find hbi-frontend/src -type f \( -name '*.ts' -o -name '*.tsx' -o -name '*.css' \) -print0 | xargs -0 cat | wc -l)
  printf "%-24s %8d\n" frontend "$fe"
fi
exit "$fail"
