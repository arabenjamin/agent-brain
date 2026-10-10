// Phase 1 (CORE_AND_PLUGINS_PLAN.md §5): graph nodes for things removed from
// the seeds. Run once after the Phase 1 image is up:
//
//   docker compose exec -T neo4j cypher-shell -u neo4j -p "$NEO4J_PASSWORD" \
//     < scripts/phase1_cleanup.cypher
//
// Idempotent. Returns what it removed.

// doc-update wrote a git-log digest into project-docs/STATUS.md every 12h;
// STATUS.md is now dev notes in the graph and `git log` is the changelog.
MATCH (s:ScheduledTask {name: 'Doc update: review recent git changes and sync project-docs'})
WITH s, s.name AS name
DETACH DELETE s
RETURN 'schedule removed' AS what, name;
