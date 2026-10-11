# Plans index

Active plans only, one line each. A finished plan becomes `decision` dev notes
and its file is deleted; a designed-but-unbuilt one becomes `plan` dev notes.
Find either with `scripts/dev_docs.sh search "<topic>"`; a note's `subsystem`
is the plan file's old name (e.g. `HBI_REDESIGN_PLAN`).

- `CORE_AND_PLUGINS_PLAN.md`: carve the codebase to a lean core plus plugins. Phases 0 (dead-code sweep), 1 (docs into the graph), 2a (named step outputs) and 2b (workflows), 2c part 1 (rule notes rewritten) 3a (bundle loader), 3b (profiles in the graph) and 3c (plugin-api crate; ACLED is the `osint-data` plugin) done 2026-10-10; 3d (declared Cypher tools in bundles, `manage_plugin` drafting runtime bundles) done 2026-10-11; 2c part 2 (a week of runs compared) from 2026-10-17; Phase 4: packs 1 (`osint-data`), 2 (`media`), 3 (`reference` + `alias-judge`, plugin routes) and 4 (`developer`, architecture view split) done 2026-10-11, and 5 (`personal`, in the private repo `../agent-brain-personal`, mounted; topics now merge) done 2026-10-11; pack 6 (`epistemics`) next.
- `SOURCE_QUALITY_PLAN.md`: trustworthy intake from free sources. Phase 0 done 2026-10-06; Phase 1 (tag channels by source type, owner reviews) next.

Kept as a file on purpose: `FINE_TUNING_PLAYBOOK.md`, the reusable process for fine-tuning a local model.
