// Cleanup: remove the (:DynamicTool) and (:Procedure) nodes (CORE_AND_PLUGINS_PLAN Phase 3d).
//
// WHY: Phase 3d replaced DynamicSkill, manage_dynamic_tool and the procedure
// executor with declared tools (a bundle's tools/*.yaml, skills/bundle_tools.rs).
// Measured 2026-10-10: one DynamicTool (list_tasks, now
// plugins/core/tools/list_tasks.yaml, same query and output) and eight
// Procedures: list_tasks's own plus seven orphans that nothing ran (API Research
// Loop, Deep-learn a specification document, Brain self-improvement research
// loop, return_arm_to_home, knowledge_snapshot, search_procedures_by_name,
// list_dynamic_tools). No code reads either label after 3d.
//
// Idempotent. Back up first (the output is the only copy once deleted):
//   docker compose exec -T neo4j cypher-shell -u neo4j -p "$NEO4J_PASSWORD" --format plain \
//     "MATCH (n) WHERE n:DynamicTool OR n:Procedure RETURN labels(n) AS labels, properties(n) AS props" \
//     > backups/phase3d_dynamic_tools_procedures.txt
// then run this file:
//   docker compose exec -T neo4j cypher-shell -u neo4j -p "$NEO4J_PASSWORD" < scripts/phase3d_cleanup.cypher

// --- Preview ---
MATCH (n) WHERE n:DynamicTool OR n:Procedure
RETURN labels(n)[0] AS label, count(*) AS nodes;

// --- Delete (DETACH removes the DynamicTool-[:USES]->Procedure links) ---
MATCH (n) WHERE n:DynamicTool OR n:Procedure
DETACH DELETE n
RETURN count(*) AS deleted;

// --- Drop the schema the labels had (init_schema no longer creates it) ---
DROP CONSTRAINT dynamic_tool_name IF EXISTS;
DROP CONSTRAINT procedure_id IF EXISTS;
DROP INDEX dynamic_tool_idx IF EXISTS;
