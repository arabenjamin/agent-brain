// Phase 0 (CORE_AND_PLUGINS_PLAN.md §4): remove graph nodes for things deleted
// from the code and seeds. Seeders MERGE but never prune, so a deleted YAML file
// leaves its node running. Run once after the Phase 0 image is up:
//
//   docker compose exec -T neo4j cypher-shell -u neo4j -p "$NEO4J_PASSWORD" \
//     < scripts/phase0_cleanup.cypher
//
// Idempotent. Every statement returns what it removed.
// Not touched: :RemovedTool tombstones (self_model.rs writes them at startup),
// :AgentSpec nodes (history; inert without the constructor), telemetry/datasets.duckdb.

// Schedules whose YAML was deleted (GDELT + ReliefWeb ETL and their review).
MATCH (s:ScheduledTask)
WHERE s.name IN [
  'GDELT sync: load new 15-minute event files',
  'ReliefWeb sync: load new humanitarian reports',
  'Dataset review: freshness, failures and source mix of GDELT and ReliefWeb'
]
WITH s, s.name AS name
DETACH DELETE s
RETURN 'schedule removed' AS what, name;

// The git-history chain (its diff step's tool is gone; it never ran past step 1).
MATCH (c:SchedulerChain {name: 'git-history'})
DETACH DELETE c
RETURN 'chain removed' AS what, 'git-history' AS name;

// Built-in dynamic tools retired from brain_core.rs seed_built_ins.
MATCH (d:DynamicTool)
WHERE d.name IN ['list_sessions', 'get_job_result', 'search_procedures', 'list_notes']
OPTIONAL MATCH (d)-[:USES]->(p:Procedure)
WITH d, p, d.name AS name
DETACH DELETE d, p
RETURN 'dynamic tool removed' AS what, name;
