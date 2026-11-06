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
#
# DuckDB (schemas/duckdb/<database>.sql, read by duckdb_schema!):
#   - no CREATE TABLE / ALTER TABLE / CREATE INDEX for DuckDB in Rust outside
#     test code;
#   - every statement is idempotent (CREATE TABLE|INDEX|SEQUENCE IF NOT EXISTS,
#     ALTER TABLE ... ADD COLUMN IF NOT EXISTS), since the file runs on every open;
#   - each file is read by a duckdb_schema! call naming it, in that bundle.
#
# Seed files (schemas/seeds/<kind>.json, generated from the Rust type by each
# crate's seed_schemas test, which also validates every shipped seed file):
#   - every seed directory and bundle-root YAML file in any bundle has a line
#     in SEED_COVERAGE below: its generated schema, its hand-written one, or
#     why it has none yet; a new kind fails until it gets one;
#   - every schema a line names exists.
#
# Closed lists (schemas/lists/<name>.json, written from the Rust constant or
# enum by the owning crate's test; the HBI's copy is generated from them by
# scripts/gen_lists.sh):
#   - every file is {name, description, values}, name = its file stem, values
#     unique non-empty strings, and its name appears in its bundle's Rust;
#   - hbi-frontend/src/lib/lists.generated.ts is current;
#   - no tool schema enum, and no HBI source file, spells a whole list out
#     instead of taking it from the list (a "${NAME}" placeholder, or an import
#     from lists.generated.ts). The hand-written schemas/*.json hold theirs and
#     are compared with the list by crates/work/src/closed_lists.rs.
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

# Where each seed kind's shape is written down. kind<TAB>how<TAB>file or why.
# kind is a bundle directory (`views`) or a bundle-root file (`plugin.yaml`).
SEED_COVERAGE=$(cat <<'EOF'
apis	generated	plugins/core/schemas/seeds/apis.json plugins/core/schemas/seeds/api-kinds.json
areas	generated	plugins/maps/schemas/seeds/areas.json
contexts	generated	plugins/core/schemas/seeds/contexts.json plugins/core/schemas/seeds/context-protocol.json plugins/core/schemas/seeds/context-extension.json
entities	generated	plugins/core/schemas/seeds/entities.json
exact-patterns	generated	plugins/core/schemas/seeds/exact-patterns.json
layers	generated	plugins/maps/schemas/seeds/layers.json
mcp-servers	generated	plugins/core/schemas/seeds/mcp-servers.json
page-readers	generated	plugins/core/schemas/seeds/page-readers.json
search-engines	generated	plugins/core/schemas/seeds/search-engines.json
sources	generated	plugins/core/schemas/seeds/sources.json
sources-media	generated	plugins/media/schemas/seeds/sources-media.json
tools	generated	plugins/core/schemas/seeds/tools.json
topics	generated	plugins/core/schemas/seeds/topics.json plugins/core/schemas/seeds/topic-scope.json
views	generated	plugins/core/schemas/seeds/views.json
plugin.yaml	generated	plugins/core/schemas/seeds/plugin.json
read-rules.yaml	generated	plugins/media/schemas/seeds/read-rules.json
internal-claims.yaml	generated	plugins/epistemics/schemas/seeds/internal-claims.json
said-here-claims.yaml	generated	plugins/epistemics/schemas/seeds/said-here-claims.json
workflows	hand	plugins/core/schemas/workflow.json
programs	hand	plugins/core/schemas/program.json
schemas	n/a	the schema files themselves (this script)
architecture	pending	core-only self-model files (components.yaml, removed-tools.yaml), parsed generically
relations.yaml	pending	the epistemics relation vocabulary, parsed as untyped YAML
kinds.yaml	pending	the reference plugin's entity kinds
parallel.yaml	pending	the research plugin's processor config
layout.yaml	pending	the developer plugin's architecture layout
crate	not-seed	a plugin's Rust code
python	not-seed	the sandbox's Python image
rust	not-seed	the Rust sandbox's image
runner	not-seed	the ML lab's runner
yt-proxy	not-seed	the media plugin's YouTube proxy
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

# DuckDB DDL in Rust (the graph's `CREATE INDEX <name> IF NOT EXISTS FOR` is
# Neo4j's and is caught above).
DUCKDB_DDL='(CREATE TABLE|ALTER TABLE|CREATE INDEX IF NOT EXISTS [A-Za-z_0-9]+ ON|CREATE SEQUENCE)'

# A DuckDB file's statements, one per line: `--` comments dropped, joined, split on `;`.
sql_statements() {
  sed -E 's/--.*$//' "$1" | tr '\n' ' ' | tr ';' '\n' | sed -E 's/[[:space:]]+/ /g; s/^ //; s/ $//' | grep . || true
}

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
  printf "%-44s %s\n" "hand-written seed JSON Schemas (schemas/*.json)" "$n"
  n=$(files_like 'plugins/*/schemas/seeds/*.json' | wc -l)
  printf "%-44s %s\n" "generated seed schemas (schemas/seeds/)" "$n"
  n=$(awk -F'\t' '$2 == "pending"' <<<"$SEED_COVERAGE" | wc -l)
  printf "%-44s %s\n" "seed kinds still without a schema" "$n ($(awk -F'\t' '$2 == "pending" { printf "%s ", $1 }' <<<"$SEED_COVERAGE"))"
  n=0
  while read -r f; do
    t=$(test_start "$f")
    n=$((n + $(awk -v t="$t" -v re="$NEO4J_DDL" 'NR < t && $0 !~ /^[[:space:]]*\/\// && $0 ~ re' "$f" | wc -l)))
  done < <(rust_files)
  printf "%-44s %s\n" "Neo4j constraints/indexes in Rust strings" "$n"
  n=0
  local g
  for g in $(files_like 'plugins/*/schemas/graph/*.cypher'); do
    n=$((n + $(cypher_statements "$g" | wc -l)))
  done
  printf "%-44s %s\n" "Neo4j statements in schemas/graph/ files" "$n ($(files_like 'plugins/*/schemas/graph/*.cypher' | wc -l) files)"
  n=0
  while read -r f; do
    t=$(test_start "$f")
    n=$((n + $(awk -v t="$t" -v re="$DUCKDB_DDL" 'NR < t && $0 !~ /^[[:space:]]*\/\// && $0 ~ re' "$f" | wc -l)))
  done < <(rust_files)
  printf "%-44s %s\n" "DuckDB DDL lines in Rust strings" "$n"
  n=0
  for g in $(files_like 'plugins/*/schemas/duckdb/*.sql'); do
    n=$((n + $(sql_statements "$g" | grep -c '^CREATE TABLE' || true)))
  done
  printf "%-44s %s\n" "DuckDB tables in schemas/duckdb/ files" "$n ($(files_like 'plugins/*/schemas/duckdb/*.sql' | wc -l) files)"
  n=$(files_like 'plugins/*/schemas/lists/*.json' | wc -l)
  printf "%-44s %s\n" "closed lists in schemas/lists/ files" "$n"
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
  done < <(awk -v t="$t" -v re="$NEO4J_DDL" 'NR < t && $0 !~ /^[[:space:]]*\/\// && $0 ~ re { print NR }' "$f")
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

# 7. DuckDB: no DDL in Rust outside tests.
while read -r f; do
  t=$(test_start "$f")
  while IFS= read -r n; do
    [ -n "$n" ] || continue
    problems+=("$f:$n writes DuckDB DDL in Rust: put it in the bundle's schemas/duckdb/<database>.sql")
  done < <(awk -v t="$t" -v re="$DUCKDB_DDL" 'NR < t && $0 !~ /^[[:space:]]*\/\// && $0 ~ re { print NR }' "$f")
done < <(rust_files)

# 8. DuckDB files: idempotent statements, read by their code.
duck_count=0
for g in $(files_like 'plugins/*/schemas/duckdb/*.sql'); do
  bundle=$(cut -d/ -f2 <<<"$g")
  db=$(basename "$g" .sql)
  while IFS= read -r stmt; do
    duck_count=$((duck_count + 1))
    grep -qE '^(CREATE (TABLE|INDEX|SEQUENCE|UNIQUE INDEX) IF NOT EXISTS |ALTER TABLE [A-Za-z_0-9]+ ADD COLUMN IF NOT EXISTS |CREATE OR REPLACE VIEW )' <<<"$stmt" ||
      problems+=("$g: not idempotent (IF NOT EXISTS / ADD COLUMN IF NOT EXISTS): $stmt")
  done < <(sql_statements "$g")
  if [ "$bundle" = core ]; then
    rust_files | grep '^crates/' | xargs grep -l "duckdb_schema!(core \"$db\")" >/dev/null 2>&1 ||
      problems+=("$g is read by no duckdb_schema!(core \"$db\") call")
  else
    rust_files | grep "^plugins/$bundle/crate/" | xargs -r grep -l "duckdb_schema!(plugin \"$db\")" >/dev/null 2>&1 ||
      problems+=("$g is read by no duckdb_schema!(plugin \"$db\") call in plugins/$bundle/crate")
  fi
done

# 9. Seed coverage: every bundle directory and root YAML file has a line.
seed_entries=$( (files_like 'plugins/*/*/*' | cut -d/ -f3; files_like 'plugins/*/*.yaml' 'plugins/*/*.yml' | cut -d/ -f3) | sort -u)
while IFS= read -r k; do
  [ -n "$k" ] || continue
  awk -F'\t' -v k="$k" '$1 == k { ok = 1 } END { exit !ok }' <<<"$SEED_COVERAGE" ||
    problems+=("plugins/*/$k has no line in SEED_COVERAGE (scripts/check_schemas.sh): give the kind a schema (a seed_schemas test generating schemas/seeds/<kind>.json), or say why not")
done <<<"$seed_entries"
while IFS=$'\t' read -r k how what; do
  [ "$how" = generated ] || [ "$how" = hand ] || continue
  for f in $what; do
    [ -f "$f" ] || problems+=("SEED_COVERAGE says $k's schema is $f, which does not exist")
  done
done <<<"$SEED_COVERAGE"
# 10. Closed lists: well-formed, owned, generated for the HBI, not copied.
list_count=0
hbi_flat=$(mktemp)
trap 'rm -f "$hbi_flat"' EXIT
# Each HBI source file on one line, whitespace dropped, so a list spelled over
# several lines is still found. <file><TAB><text>
while IFS= read -r f; do
  printf '%s\t%s\n' "$f" "$(tr -d ' \n\t' <"$f")"
done < <(files_like 'hbi-frontend/src/**/*.ts' 'hbi-frontend/src/**/*.tsx' | grep -v 'lists\.generated\.ts$') >"$hbi_flat"
tool_enums=$(for t in $(files_like 'plugins/*/schemas/tools/*.json'); do
  jq -r --arg f "$t" '.. | objects | select(has("enum")) | .enum | select(type == "array") | [$f, ([.[] | select(type == "string" and . != "")] | tojson)] | @tsv' "$t" 2>/dev/null
done)
for l in $(files_like 'plugins/*/schemas/lists/*.json'); do
  list_count=$((list_count + 1))
  bundle=$(cut -d/ -f2 <<<"$l")
  stem=$(basename "$l" .json)
  if ! jq -e --arg n "$stem" '(.name == $n) and (.description | type == "string" and length > 0) and (.values | type == "array" and length > 0 and all(type == "string" and length > 0) and (unique | length) == length)' "$l" >/dev/null 2>&1; then
    problems+=("$l: not {name: \"$stem\", description, values: [unique non-empty strings]}")
    continue
  fi
  if [ "$bundle" = core ]; then owner=$(rust_files | grep '^crates/'); else owner=$(rust_files | grep "^plugins/$bundle/crate/"); fi
  xargs -r grep -lF "\"$stem\"" <<<"$owner" >/dev/null 2>&1 ||
    problems+=("$l: no Rust in its bundle names \"$stem\" (which test writes it?)")
  values=$(jq -c .values "$l")
  while IFS=$'\t' read -r t e; do
    [ -n "$t" ] || continue
    [ "$e" = "$values" ] && problems+=("$t spells out the $stem list: fill it with a \"\${NAME}\" placeholder from the Rust constant")
  done <<<"$tool_enums"
  needle=$(jq -r '.values | map("\"" + . + "\"") | join(",")' "$l")
  while IFS=$'\t' read -r f _; do
    problems+=("$f spells out the $stem list: import it from src/lib/lists.generated.ts")
  done < <(grep -F -- "[$needle]" "$hbi_flat" || true)
done
scripts/gen_lists.sh --check 2>/dev/null ||
  problems+=("hbi-frontend/src/lib/lists.generated.ts is stale: run scripts/gen_lists.sh")

seed_generated=$(files_like 'plugins/*/schemas/seeds/*.json' | wc -l)
seed_pending=$(awk -F'\t' '$2 == "pending"' <<<"$SEED_COVERAGE" | wc -l)

files=$(files_like 'plugins/*/schemas/tools/*.json' | wc -l)
if [ ${#problems[@]} -gt 0 ]; then
  echo "schema check: ${#problems[@]} problem(s):" >&2
  printf '  %s\n' "${problems[@]}" >&2
  exit 1
fi
echo "schemas checked: $files tool schema files, each used once, placeholders matching; $(grep -c . <<<"$INLINE_OK") inline by design; $graph_count Neo4j statements in $(files_like 'plugins/*/schemas/graph/*.cypher' | wc -l) graph files, idempotent, names unique; $duck_count DuckDB statements in $(files_like 'plugins/*/schemas/duckdb/*.sql' | wc -l) files, idempotent; $seed_generated generated seed schemas, $seed_pending seed kinds still without one; $list_count closed lists, none copied."
