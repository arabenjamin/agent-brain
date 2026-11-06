# developer

Codebase tools (read_codebase_file, search_codebase, get_git_log, write_codebase_doc, write_proposal, analyze_own_structure, workspace files), the architecture view and snapshot_architecture, the dev notes (dev_docs), the doc-update, brain-api-sync, diagnose and capability-mining workflows, and the code-analyst and self-analyst agents.

Built before the template and linked to it on 2026-11-06; `.cruft.json`
records the template commit.
`scripts/plugin_template.sh update developer` brings a later
template change in as a diff to review.

## Layout

| Path | Holds |
|---|---|
| `plugin.yaml` | name, version, description |
| `layout.yaml` | how the architecture view groups what the brain is made of: the groups, their edges and the container topology (the rest is read live) |
| `crate/src/lib.rs` | the `Plugin` impl (`Developer`): `start` applies the graph schema, seeds and embeds the dev notes, and takes the first architecture snapshot |
| `crate/src/codebase.rs` | `read_codebase_file`, `search_codebase`, `get_git_log`, `analyze_own_structure`, `write_codebase_doc`, `write_proposal` (a Feed item, never a code change), and `list_workspace_files` / `write_workspace_file` when `WORKSPACE_DIR` is set |
| `crate/src/dev_docs.rs` | `dev_docs`: query and write the dev notes |
| `crate/src/dev_notes.rs` | `(:DevNote)`: the store, seeded from `devdocs/devnotes.jsonl` when the graph has none, and rewriting that file on every write |
| `crate/src/arch.rs` | the living architecture: the inventory computed from what the brain knows about itself, and snapshots and their diffs |
| `crate/src/snapshot.rs` | `snapshot_architecture` (deterministic, no model) |
| `crate/src/scaffold.rs` | `scaffold_plugin`: a new plugin drafted from `templates/plugin`, rendered in the offline Python sandbox (`SANDBOX_URL`), staged under the workspace's `scaffolds/`, proposed in the Feed |
| `crate/src/routes.rs` | `GET /api/plugins/developer/architecture`, the System → Architecture view |
| `schemas/tools/` | the eleven tools' input schemas |
| `schemas/graph/schema.cypher` | `ArchSnapshot` and `DevNote`: constraints, the dev notes' vector and full-text indexes |
| `workflows/brain-api-sync.yaml` | weekly: an architecture snapshot, then the brain's API docs kept current |
| `workflows/capability-mining.yaml` | weekly: failing tasks grouped by capability, reasoned into workflow, plugin or skill candidates (a reflection note) |
| `workflows/codebase.yaml`, `diagnose.yaml` | self-analysis of the code; a failure's diagnosis staged as a proposal |
| `contexts/code-analyst.yaml`, `self-analyst.yaml` | the agents that work on the code and on the brain itself |

Graph access is in `dev_notes.rs` and `arch.rs`. The plugin reads code (`CODEBASE_DIR`) and writes only the workspace and the dev notes: it never changes a repo file.

## Enable and check

On by default (`PLUGINS_ENABLED` in `docker-compose.yml`). The codebase tools need `CODEBASE_DIR`; the workspace pair, `WORKSPACE_DIR`. Then
`cargo test -p agent-brain-plugin-developer --lib`, `scripts/check_schemas.sh`
and `scripts/check_plugin_deps.sh`, and rebuild the image.
