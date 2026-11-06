# epistemics

Claims (the claim tool: extract, verify against independent web evidence, list, sources) and typed relationships between entities (extract_relations, the review routes), the claim-verification and relation-extraction workflows, and the platforms and state-media source lists the verifier reads.

Built before the template and linked to it on 2026-11-06; `.cruft.json`
records the template commit.
`scripts/plugin_template.sh update epistemics` brings a later
template change in as a diff to review.

## Layout

| Path | Holds |
|---|---|
| `plugin.yaml` | name, version, description |
| `crate/src/lib.rs` | the `Plugin` impl (`Epistemics`): two skills and the routes; `start` folds old `RELATED_ENTITY` answers into `related_to` relationships |
| `crate/src/claim_skill.rs` | `claim`: extract, verify, list, sources, contested, and the owner's confirm or dismiss of a refutation |
| `crate/src/claims.rs` | claim storage and status: attributed, stored unverified, status derived from evidence edges only (the `claim-statuses` list) |
| `crate/src/grounding.rs` | the second opinion on every "supports", by a local guardian model; `groundedness_prompt.txt` is its prompt, `groundedness_rendered_by_ollama.txt` the rendering its test compares with |
| `crate/src/internal.rs` | claims about the brain itself, never extracted or web-verified (words in `internal-claims.yaml`) |
| `crate/src/said_here.rs` | attributions the recording proves by itself, skipped by the verifier (words in `said-here-claims.yaml`) |
| `crate/src/relation_skill.rs` | `extract_relations` |
| `crate/src/relations.rs` | typed relationships between entities, from the closed vocabulary in `relations.yaml`, as proposals the person reviews |
| `crate/src/routes.rs` | the HTTP routes under `/api/plugins/epistemics/` (relationships) |
| `crate/src/db.rs` | the graph as the plugin uses it |
| `internal-claims.yaml`, `said-here-claims.yaml`, `relations.yaml` | the words and vocabulary those modules read |
| `sources/platforms.yaml`, `state-media.yaml` | source lists seeded once, then graph-owned: user-content platforms (citable, never counted as independent) and government-controlled outlets (labelled, never excluded) |
| `views/claims.yaml`, `claim_evidence.yaml` | the `lookup` views of claims, and of claims with their evidence |
| `workflows/claim-verification.yaml` | every 6 h: the unverified backlog, in batches, against independent evidence |
| `workflows/claim-refutation-review.yaml` | weekly: contested claims as one Feed item the owner answers |
| `workflows/relation-extraction.yaml` | every 30 min: relationships proposed from claims naming two or more entities |
| `schemas/tools/` | `claim` and `extract_relations` input schemas |
| `schemas/seeds/` | the generated shapes of `internal-claims.yaml` and `said-here-claims.yaml` |
| `schemas/lists/claim-statuses.json` | the closed list of claim statuses, written from `ClaimStatus` |

Claims are notes (`note_type: 'claim'`), so the core keeps reading them with the plugin off; nothing new is claimed or verified then. Graph access is in `db.rs` (with the queries in each module).

## Enable and check

On by default (`PLUGINS_ENABLED` in `docker-compose.yml`). Then
`cargo test -p agent-brain-plugin-epistemics --lib`, `scripts/check_schemas.sh`
and `scripts/check_plugin_deps.sh`, and rebuild the image.
