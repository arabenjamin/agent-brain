#!/usr/bin/env bash
# One check for the schema files each bundle keeps in schemas/
# (CODEBASE_SPLIT_PLAN Phase 1). Run in CI and the pre-commit hook.
#
#   scripts/check_schemas.sh              # check; exits non-zero on a problem
#   scripts/check_schemas.sh --inventory  # where every kind of schema lives now
#
# Tools (schemas/tools/<tool>.json, loaded by tool_schema!):
#   - every file is JSON (jq), and is used by exactly one tool_schema! call;
#   - a file's "${NAME}" placeholders are exactly the ones its call fills;
#   - no tool schema is written inline (input_schema: json!) outside test code,
#     except the ones built from data at runtime, listed in INLINE_OK below.
# That each file is a valid JSON Schema is a unit test
# (crates/host/src/schema_files.rs), since it needs a validator.
#
# Graph (schemas/graph/schema.cypher, read by graph_schema!):
#   - no Neo4j constraint or index is written in Rust outside test code;
#   - every statement is `CREATE <kind> <name> IF NOT EXISTS FOR ...`, so it is
#     safe to run on every start, and no name is defined twice across bundles;
#   - a bundle's file is read by its code: graph_schema!(core) in a core crate,
#     graph_schema!(plugin) in that plugin's crate.
set -euo pipefail
cd "$(dirname "$0")/.."
export LC_ALL=C

# Tool schemas still built in Rust, because their values come from data
# loaded at runtime. file<TAB>tool<TAB>why
INLINE_OK=$(cat <<'EOF'
crates/host/src/views.rs	lookup	its slots are the views loaded from every bundle's views/
crates/retrieve/src/search.rs	search_web	its engine choices are the search-engines/ seeds
plugins/research/crate/src/skill.rs	deep_research	its processors come from parallel.yaml
plugins/sandbox/crate/src/skill.rs	execute_code	its languages come from the sandboxes configured
plugins/websocket/crate/src/skill.rs	ws_receive	its description is formatted from the default timeout
EOF
)

# Tracked and new (not ignored) Rust source, leaving out whole test modules
# (a `tests.rs` is declared `#[cfg(test)] mod tests;` by its parent).
rust_files() { git ls-files --cached --others --exclude-standard -- 'crates/*.rs' 'plugins/*/crate/*.rs' | grep -v '/tests\.rs$'; }
# Tracked and new files under a pattern.
# Glob pathspecs, so `*` stays inside one directory.
files_like() {
  local p args=()
  for p in "$@"; do args+=(":(glob)$p"); done
  git ls-files --cached --others --exclude-standard -- "${args[@]}"
}

# A Neo4j schema statement (not DuckDB's `CREATE INDEX IF NOT EXISTS t ON ...`).
NEO4J_DDL='CREATE (CONSTRAINT|VECTOR INDEX|FULLTEXT INDEX|RANGE INDEX|TEXT INDEX|POINT INDEX|LOOKUP INDEX|INDEX [A-Za-z_0-9]+ IF NOT EXISTS)'

# A schema file's statements, one per line: comment lines dropped, each
# statement's lines joined, split on `;`.
cypher_statements() {
  grep -v '^[[:space:]]*//' "$1" | tr '\n' ' ' | tr ';' '\n' | sed -E 's/[[:space:]]+/ /g; s/^ //; s/ $//' | grep . || true
}

# Line number of a file's first #[cfg(test)], or a huge one when it has none.
test_start() { awk '/#\[cfg\(test\)\]/ { print NR; found = 1; exit } END { if (!found) print 999999999 }' "$1"; }

inventory() {
  local n
  printf "%-44s %s\n" "kind" "count"
  n=$(files_like 'plugins/*/schemas/tools/*.json' | wc -l)
  printf "%-44s %s\n" "tool schemas in schemas/tools/ files" "$n"
  n=0
  while read -r f; do
    t=$(test_start "$f")
    n=$((n + $(awk -v t="$t" 'NR < t && /input_schema:[[:space:]]*(serde_json::)?json!/' "$f" | wc -l)))
  done < <(rust_files)
  printf "%-44s %s\n" "tool schemas inline in Rust (outside tests)" "$n"
  n=$(files_like 'plugins/*/tools/*.yaml' | wc -l)
  printf "%-44s %s\n" "declared tools (YAML, schema in the file)" "$n"
  n=$(rust_files | xargs grep -h 'deny_unknown_fields' | wc -l)
  printf "%-44s %s\n" "seed-file structs (deny_unknown_fields)" "$n"
  n=$(files_like 'plugins/*/schemas/*.json' | wc -l)
  printf "%-44s %s\n" "seed JSON Schemas (schemas/*.json)" "$n"
  n=0
  while read -r f; do
    t=$(test_start "$f")
    n=$((n + $(awk -v t="$t" -v re="$NEO4J_DDL" 'NR < t && $0 ~ re' "$f" | wc -l)))
  done < <(rust_files)
  printf "%-44s %s\n" "Neo4j constraints/indexes in Rust strings" "$n"
  n=0
  local g
  for g in $(files_like 'plugins/*/schemas/graph/*.cypher'); do
    n=$((n + $(cypher_statements "$g" | wc -l)))
  done
  printf "%-44s %s\n" "Neo4j statements in schemas/graph/ files" "$n ($(files_like 'plugins/*/schemas/graph/*.cypher' | wc -l) files)"
  n=$(rust_files | xargs grep -h 'CREATE TABLE' | wc -l)
  printf "%-44s %s\n" "DuckDB tables in Rust strings" "$n"
  n=$(files_like 'plugins/*/schemas/telemetry/*.sql' | wc -l)
  printf "%-44s %s\n" "DuckDB schema files (schemas/telemetry/)" "$n"
}

if [ "${1:-}" = --inventory ]; then
  inventory
  exit 0
fi

problems=()

# 1. Every tool_schema! call: which file it reads and what it fills.
#    Output lines: <schema file><TAB><source file><TAB><filled keys, space-separated>
calls=$(rust_files | xargs grep -HnE 'tool_schema!\((core|plugin) "' 2>/dev/null |
  grep -v '^crates/protocol/src/tool_schema.rs:' |
  while IFS= read -r line; do
    src=${line%%:*}
    rest=${line#*:}
    rest=${rest#*:}
    kind=$(sed -E 's/.*tool_schema!\((core|plugin) "([a-z0-9_]+)".*/\1/' <<<"$rest")
    tool=$(sed -E 's/.*tool_schema!\((core|plugin) "([a-z0-9_]+)".*/\2/' <<<"$rest")
    keys=$( (grep -oE '"[A-Za-z0-9_:]+" =>' <<<"$rest" || true) | sed -E 's/"(.*)" =>/\1/' | sort | tr '\n' ' ')
    if [ "$kind" = core ]; then
      bundle=core
    else
      bundle=$(cut -d/ -f2 <<<"$src")
    fi
    printf "plugins/%s/schemas/tools/%s.json\t%s\t%s\n" "$bundle" "$tool" "$src" "$keys"
  done)

# 2. Every file: JSON, used once, placeholders match.
while IFS= read -r f; do
  if ! jq -e . "$f" >/dev/null 2>&1; then
    problems+=("$f is not JSON")
    continue
  fi
  uses=$(awk -F'\t' -v f="$f" '$1 == f' <<<"$calls")
  count=$(grep -c . <<<"$uses" || true)
  if [ "$count" -eq 0 ]; then
    problems+=("$f is used by no tool_schema! call (delete it, or load it)")
    continue
  fi
  if [ "$count" -gt 1 ]; then
    problems+=("$f is loaded by $count calls: $(cut -f2 <<<"$uses" | tr '\n' ' ')")
  fi
  want=$( (grep -oE '"\$\{[A-Za-z0-9_:]+\}"' "$f" || true) | sed -E 's/"\$\{(.*)\}"/\1/' | sort -u | tr '\n' ' ')
  have=$(cut -f3 <<<"$uses" | head -n 1 | tr ' ' '\n' | grep . | sort -u | tr '\n' ' ' || true)
  if [ "$want" != "$have" ]; then
    problems+=("$f has placeholders [${want% }] but $(cut -f2 <<<"$uses" | head -n 1) fills [${have% }]")
  fi
done < <(files_like 'plugins/*/schemas/tools/*.json')

# 3. Calls naming a file that does not exist (the build fails too; this says why).
while IFS=$'\t' read -r f src _; do
  [ -n "$f" ] || continue
  [ -f "$f" ] || problems+=("$src loads $f, which does not exist")
done <<<"$calls"

# 4. Inline tool schemas outside test code, other than the listed ones.
while read -r f; do
  t=$(test_start "$f")
  while IFS= read -r n; do
    [ -n "$n" ] || continue
    if ! awk -F'\t' -v f="$f" '$1 == f { ok = 1 } END { exit !ok }' <<<"$INLINE_OK"; then
      problems+=("$f:$n writes a tool schema inline: move it to the bundle's schemas/tools/<tool>.json and load it with tool_schema!")
    fi
  done < <(awk -v t="$t" 'NR < t && /input_schema:[[:space:]]*(serde_json::)?json!/ { print NR }' "$f")
done < <(rust_files)

# 5. Graph: no Neo4j DDL in Rust outside tests.
while read -r f; do
  t=$(test_start "$f")
  while IFS= read -r n; do
    [ -n "$n" ] || continue
    problems+=("$f:$n writes a Neo4j constraint or index in Rust: put it in the bundle's schemas/graph/schema.cypher")
  done < <(awk -v t="$t" -v re="$NEO4J_DDL" 'NR < t && $0 ~ re { print NR }' "$f")
done < <(rust_files)

# 6. Graph files: idempotent statements, unique names, read by their code.
graph_names=""
graph_count=0
for g in $(files_like 'plugins/*/schemas/graph/*.cypher'); do
  bundle=$(cut -d/ -f2 <<<"$g")
  if [ "$(basename "$g")" != schema.cypher ]; then
    problems+=("$g: a bundle's graph schema is one file, schemas/graph/schema.cypher (graph_schema! reads only that)")
  fi
  while IFS= read -r stmt; do
    graph_count=$((graph_count + 1))
    name=$(sed -nE 's/^CREATE ([A-Z]+ )?(CONSTRAINT|INDEX) ([A-Za-z_0-9]+) IF NOT EXISTS FOR .*/\3/p' <<<"$stmt")
    if [ -z "$name" ]; then
      problems+=("$g: not an idempotent CREATE ... <name> IF NOT EXISTS FOR ...: $stmt")
      continue
    fi
    graph_names+="$name	$g"$'\n'
  done < <(cypher_statements "$g")
  if [ "$bundle" = core ]; then
    rust_files | grep '^crates/' | xargs grep -l 'graph_schema!(core)' >/dev/null 2>&1 ||
      problems+=("$g is read by no graph_schema!(core) call")
  else
    rust_files | grep "^plugins/$bundle/crate/" | xargs -r grep -l 'graph_schema!(plugin)' >/dev/null 2>&1 ||
      problems+=("$g is read by no graph_schema!(plugin) call in plugins/$bundle/crate")
  fi
done
while IFS= read -r dup; do
  [ -n "$dup" ] || continue
  problems+=("Neo4j name $dup is defined more than once: $(awk -F'\t' -v n="$dup" '$1 == n { print $2 }' <<<"$graph_names" | tr '\n' ' ')")
done < <(cut -f1 <<<"$graph_names" | grep . | sort | uniq -d)

files=$(files_like 'plugins/*/schemas/tools/*.json' | wc -l)
if [ ${#problems[@]} -gt 0 ]; then
  echo "schema check: ${#problems[@]} problem(s):" >&2
  printf '  %s\n' "${problems[@]}" >&2
  exit 1
fi
echo "schemas checked: $files tool schema files, each used once, placeholders matching; $(grep -c . <<<"$INLINE_OK") inline by design; $graph_count Neo4j statements in $(files_like 'plugins/*/schemas/graph/*.cypher' | wc -l) graph files, idempotent, names unique."
