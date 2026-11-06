#!/usr/bin/env bash
# Stamp, update and check plugins against templates/plugin (CODEBASE_SPLIT_PLAN
# Phase 2). cookiecutter 2.7.1 and cruft 2.16.0 run through uvx, so nothing is
# installed on the host; `check` needs only git.
#
#   scripts/plugin_template.sh new <name> --kind rust|data --description "..."
#       [--tools a_tool,b_tool] [--graph-labels Thing,Other] [--env KEY,URL]
#     Stamp plugins/<name>, register a Rust one with the host (crates/host
#     Cargo.toml and plugins.rs, the Dockerfile's dependency stage), format,
#     and run its tests and the repo checks. Leaves everything unstaged.
#
#   scripts/plugin_template.sh update <name>
#     Bring the template's changes since the plugin's recorded commit into it
#     (cruft's three-way update; a conflict is left marked for review).
#
#   scripts/plugin_template.sh link <name>
#     Put a plugin built before the template on it (Phase 2 step 4): record
#     its answers, read from its own files, in .cruft.json, and add the
#     template's README.md if it has none. Changes none of its other files;
#     `uvx ... cruft diff --project-dir plugins/<name>` then shows what differs.
#
#   scripts/plugin_template.sh check
#     Fail when a stamped plugin is behind the template: templates/plugin
#     changed after the commit its .cruft.json records. CI runs this. (cruft's
#     own `check` compares commit hashes, so with the template in this repo
#     every commit would flag every plugin.) Also fails on a bundle with no
#     .cruft.json that is not in NOT_YET_LINKED.
#
# The template is this repo (".", with directory templates/plugin), so cruft
# runs from the repo root and reads the template as committed: commit a
# template change before stamping or updating with it.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

CC_VERSION=2.7.1
CRUFT_VERSION=2.16.0
cruft() { uvx --quiet --from "cruft==$CRUFT_VERSION" --with "cookiecutter==$CC_VERSION" cruft "$@"; }

die() { echo "plugin_template: $*" >&2; exit 1; }

# Bundles built before the template and not yet linked to it (Phase 2 step
# 4). A bundle leaves this list in the commit that links it; a new one is
# stamped. `core` is the brain's own bundle, not a plugin, and is never
# stamped.
NOT_YET_LINKED="epistemics maps media"

# The template must be committed: cruft clones it from HEAD.
template_committed() {
  git diff --quiet HEAD -- templates/plugin &&
    [ -z "$(git ls-files --others --exclude-standard -- templates/plugin)" ] ||
    die "templates/plugin has uncommitted changes; cruft reads the committed template, so commit it first"
}

# cruft writes the template's absolute path; keep it "." so the file is the
# same on every checkout. And mark the files that are the plugin's own.
relative_template() {
  python3 - "$1/.cruft.json" <<'PYEOF'
import json, sys
p = sys.argv[1]
d = json.load(open(p))
d["template"] = "."
d["context"]["cookiecutter"]["_template"] = "."
# The plugin's own once written: an update never patches these (a template
# change to the skeleton tool would only conflict with the real one).
d["skip"] = ["crate/src/skill.rs", "schemas/tools", "schemas/graph"]
with open(p + ".new", "w") as f:
    f.write(json.dumps(d, indent=2) + "\n")
import os
os.replace(p + ".new", p)
PYEOF
}

# Add a Rust plugin to the host and the image's dependency stage. Each edit
# is anchored on the existing plugins' lines and asserts it found them.
register_rust() {
  python3 - "$1" "$2" "$3" <<'PYEOF'
import os, re, sys
name, crate, struct = sys.argv[1:4]
ident = crate.replace("-", "_")

def edit(path, fn):
    s = open(path).read()
    t = fn(s)
    assert t != s, f"{path}: nothing changed"
    with open(path + ".new", "w") as f:
        f.write(t)
    os.replace(path + ".new", path)

def host_toml(s):
    lines = s.split("\n")
    deps = [i for i, l in enumerate(lines) if re.match(r'agent-brain-plugin-[a-z0-9-]+ = \{ path = "\.\./\.\./plugins/', l)]
    assert deps, "crates/host/Cargo.toml: no plugin dependency lines"
    lines.insert(deps[-1] + 1, f'{crate} = {{ path = "../../plugins/{name}/crate", optional = true }}')
    feats = [i for i, l in enumerate(lines) if re.match(r'plugin-[a-z0-9-]+ = \["dep:agent-brain-plugin-', l)]
    assert feats, "crates/host/Cargo.toml: no plugin feature lines"
    lines.insert(feats[-1] + 1, f'plugin-{name} = ["dep:{crate}"]')
    i = next(i for i, l in enumerate(lines) if l.startswith("plugins = ["))
    assert lines[i].endswith("]"), "crates/host/Cargo.toml: the plugins feature is not one line"
    lines[i] = lines[i][:-1] + f', "plugin-{name}"]'
    return "\n".join(lines)

def plugins_rs(s):
    anchor = "    plugins\n}"
    assert s.count(anchor) == 1, "plugins.rs: compiled() has changed shape"
    add = f'    #[cfg(feature = "plugin-{name}")]\n    plugins.push(Arc::new({ident}::{struct}));\n'
    return s.replace(anchor, add + anchor)

def dockerfile(s):
    copies = list(re.finditer(r"^COPY plugins/[a-z0-9-]+/crate/Cargo\.toml \./plugins/[a-z0-9-]+/crate/\n", s, re.M))
    assert copies, "Dockerfile: no plugin COPY lines"
    at = copies[-1].end()
    s = s[:at] + f"COPY plugins/{name}/crate/Cargo.toml ./plugins/{name}/crate/\n" + s[at:]
    # The mkdir list: the last entry keeps its line ending (` \` or
    # ` && \`), which moves to the new last entry.
    dirs = list(re.finditer(r"^( +)plugins/[a-z0-9-]+/crate/src( .*)$", s, re.M))
    assert dirs, "Dockerfile: no plugin stub directories"
    last = dirs[-1]
    indent, ending = last.group(1), last.group(2)
    entry = last.group(0)[: -len(ending)]
    s = s[: last.start()] + entry + " \\\n" + f"{indent}plugins/{name}/crate/src{ending}" + s[last.end():]
    stubs = list(re.finditer(r'^ +echo "" > plugins/[a-z0-9-]+/crate/src/lib\.rs && \\\n', s, re.M))
    assert stubs, "Dockerfile: no plugin stub lib.rs lines"
    at = stubs[-1].end()
    s = s[:at] + f'    echo "" > plugins/{name}/crate/src/lib.rs && \\\n' + s[at:]
    return s

edit("crates/host/Cargo.toml", host_toml)
edit("crates/host/src/plugins.rs", plugins_rs)
edit("Dockerfile", dockerfile)
print(f"registered {crate}: crates/host/Cargo.toml, crates/host/src/plugins.rs, Dockerfile")
PYEOF
}

# Tool names belong to one skill each (dispatch is first match).
tool_taken() {
  local t=$1
  git ls-files --cached --others --exclude-standard -- ":(glob)plugins/*/schemas/tools/$t.json" ":(glob)plugins/*/tools/$t.yaml" | grep -q . && return 0
  git grep -qE "(name: \"$t\"\.(to_string|into)\(\)|_TOOL: &str = \"$t\")" -- 'crates/*.rs' 'plugins/*/crate/*.rs' && return 0
  # The running brain's tools, when it is up (the enabled plugins only).
  local key
  key=$(scripts/brain_key.sh 2>/dev/null) || return 1
  curl -sf -m 5 -H "Authorization: Bearer $key" "${BRAIN_URL:-http://localhost:3001}/api/tools" 2>/dev/null |
    jq -e --arg t "$t" '[.tools[]? | (.name? // .)] | index($t) != null' >/dev/null 2>&1
}

checks() {
  local name=$1 crate=$2
  if [ -d "plugins/$name/crate" ]; then
    cargo fmt
    cargo test -p "$crate" --lib
    cargo clippy -p "$crate" -p agent-brain-host --all-targets -- -D warnings
    scripts/check_plugin_deps.sh
  fi
  scripts/check_schemas.sh
  "$0" check
}

cmd_new() {
  local name=${1:-} kind="" description="" tools="" labels="" env=""
  [ -n "$name" ] || die "usage: new <name> --kind rust|data --description \"...\" [--tools a,b] [--graph-labels A,B] [--env K,V]"
  shift
  while [ $# -gt 0 ]; do
    case "$1" in
      --kind) kind=$2; shift 2 ;;
      --description) description=$2; shift 2 ;;
      --tools) tools=$2; shift 2 ;;
      --graph-labels) labels=$2; shift 2 ;;
      --env) env=$2; shift 2 ;;
      *) die "unknown option $1" ;;
    esac
  done
  [ "$kind" = rust ] || [ "$kind" = data ] || die "--kind rust or data"
  [ -n "$description" ] || die "--description is required"
  [ ! -e "plugins/$name" ] || die "plugins/$name exists"
  local t
  for t in ${tools//,/ }; do
    tool_taken "$t" && die "tool name '$t' is taken (a tool name belongs to one skill)"
  done
  template_committed
  local ctx
  ctx=$(jq -n --arg n "$name" --arg d "$description" --arg k "$kind" --arg t "$tools" --arg l "$labels" --arg e "$env" \
    '{name: $n, description: $d, kind: $k, tools: $t, graph_labels: $l, needs_env: $e}')
  cruft create . --directory templates/plugin --output-dir plugins --no-input --extra-context "$ctx"
  relative_template "plugins/$name"
  local crate struct
  crate=$(jq -r '.context.cookiecutter.__crate' "plugins/$name/.cruft.json")
  struct=$(jq -r '.context.cookiecutter.__struct' "plugins/$name/.cruft.json")
  if [ "$kind" = rust ]; then
    register_rust "$name" "$crate" "$struct"
  fi
  checks "$name" "$crate"
  cat <<EOF

plugins/$name is stamped and checks pass. Left to do:
  - enable it: add $name to PLUGINS_ENABLED in .env (and the default in docker-compose.yml if it ships on)
$( [ "$kind" = rust ] && [ -n "$tools" ] && echo "  - build the tools in plugins/$name/crate/src/skill.rs and their schemas in plugins/$name/schemas/tools/" )
  - seeds, if any, in the bundle's seed directories; README.md says what it holds
  - docs: README.md's plugin list and project-docs as the change needs
  - review and stage: git status
EOF
}

cmd_update() {
  local name=${1:-}
  [ -n "$name" ] && [ -f "plugins/$name/.cruft.json" ] || die "usage: update <name> (a plugin with a .cruft.json)"
  template_committed
  cruft update --project-dir "plugins/$name" --skip-apply-ask --allow-untracked-files
  relative_template "plugins/$name"
  if grep -rlE '^(<<<<<<<|>>>>>>>) ' "plugins/$name" >/dev/null 2>&1; then
    echo "plugins/$name: the update conflicts with the plugin's own changes; resolve the marked files:" >&2
    grep -rlE '^(<<<<<<<|>>>>>>>) ' "plugins/$name" >&2
    exit 1
  fi
  checks "$name" "$(jq -r '.context.cookiecutter.__crate' "plugins/$name/.cruft.json")"
}

# A pre-template plugin's answers, from its own files: plugin.yaml's
# description and requires.env, crate/ for the kind, the tools with a
# schemas/tools file or an inline schema, and the labels its
# schemas/graph/schema.cypher names.
answers() {
  python3 - "plugins/$1" <<'EOF'
import json, os, re, sys
import yaml
d = sys.argv[1]
y = yaml.safe_load(open(os.path.join(d, "plugin.yaml")))
tools_dir = os.path.join(d, "schemas/tools")
tools = [f[:-5] for f in os.listdir(tools_dir) if f.endswith(".json")] if os.path.isdir(tools_dir) else []
# Tools whose schema is built in Rust from data: check_schemas.sh's INLINE_OK.
for line in open("scripts/check_schemas.sh"):
    parts = line.rstrip("\n").split("\t")
    if len(parts) == 3 and parts[0].startswith(d + "/crate/"):
        tools.append(parts[1])
tools = sorted(set(tools))
labels = []
cy = os.path.join(d, "schemas/graph/schema.cypher")
if os.path.isfile(cy):
    for m in re.finditer(r"FOR \(\w+:(\w+)\)", open(cy).read()):
        if m.group(1) not in labels:
            labels.append(m.group(1))
print(json.dumps({
    "name": os.path.basename(d),
    "description": " ".join(str(y.get("description", "")).split()).replace('"', "'"),
    "kind": "rust" if os.path.isdir(os.path.join(d, "crate")) else "data",
    "tools": ",".join(tools),
    "graph_labels": ",".join(labels),
    "needs_env": ",".join((y.get("requires") or {}).get("env") or []),
}))
EOF
}

cmd_link() {
  local name=${1:-}
  [ -n "$name" ] && [ -f "plugins/$name/plugin.yaml" ] || die "usage: link <name> (an existing plugin)"
  [ ! -e "plugins/$name/.cruft.json" ] || die "plugins/$name is already on the template"
  template_committed
  local ctx tmp args=() kv
  ctx=$(answers "$name")
  cruft link . --directory templates/plugin --project-dir "plugins/$name" --no-input --extra-context "$ctx"
  relative_template "plugins/$name"
  if [ ! -e "plugins/$name/README.md" ]; then
    while IFS= read -r kv; do args+=("$kv"); done < <(jq -r 'to_entries[] | "\(.key)=\(.value)"' <<<"$ctx")
    tmp=$(mktemp -d)
    uvx --quiet --from "cookiecutter==$CC_VERSION" cookiecutter templates/plugin --no-input -o "$tmp" "${args[@]}"
    cp "$tmp/$name/README.md" "plugins/$name/README.md"
    rm -rf "$tmp"
  fi
  echo "linked plugins/$name: $(jq -c . <<<"$ctx")"
  echo "next: edit plugins/$name/README.md's layout to its real modules; add the template's name test to crate/src/lib.rs; drop $name from NOT_YET_LINKED"
}

cmd_check() {
  local f dir commit n=0 behind=() b
  for f in $(git ls-files --cached --others --exclude-standard -- ':(glob)plugins/*/.cruft.json'); do
    n=$((n + 1))
    dir=$(dirname "$f")
    commit=$(jq -r .commit "$f")
    if ! git cat-file -e "$commit^{commit}" 2>/dev/null; then
      behind+=("$dir records commit ${commit:0:12}, which this clone lacks (CI: checkout with fetch-depth 0)")
    elif ! git diff --quiet "$commit" HEAD -- templates/plugin; then
      behind+=("$dir is behind templates/plugin (changed since ${commit:0:12}): scripts/plugin_template.sh update $(basename "$dir")")
    fi
  done
  local pending=0
  for b in plugins/*/; do
    b=$(basename "$b")
    [ "$b" = core ] && continue
    [ -f "plugins/$b/plugin.yaml" ] || continue
    if [[ " $NOT_YET_LINKED " == *" $b "* ]]; then
      pending=$((pending + 1))
      [ -f "plugins/$b/.cruft.json" ] && behind+=("plugins/$b is linked: drop it from NOT_YET_LINKED (scripts/plugin_template.sh)")
    elif [ ! -f "plugins/$b/.cruft.json" ]; then
      behind+=("plugins/$b has no .cruft.json: stamp it (scripts/plugin_template.sh new) or link it (link)")
    fi
  done
  if [ ${#behind[@]} -gt 0 ]; then
    printf '%s\n' "${behind[@]}" >&2
    exit 1
  fi
  echo "plugin template: $n stamped plugins, all current with templates/plugin; $pending still to link"
}

case "${1:-}" in
  new) shift; cmd_new "$@" ;;
  update) shift; cmd_update "$@" ;;
  link) shift; cmd_link "$@" ;;
  check) shift; cmd_check "$@" ;;
  *) die "usage: $0 new|update|link|check (see the header)" ;;
esac
