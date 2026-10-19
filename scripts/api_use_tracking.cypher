// API_USE_PLAN Phase 4 (2026-10-19): start tracking API use. Before today
// nothing recorded when an API was called, so each existing context's clock
// starts now (use_tracked_from), not at a guessed last use. From the Phase 4
// build on, http_request stamps last_used_at and registration sets created_at.
// The brain's own API is generated from the codebase (brain-api-sync) and is
// never offered for retirement. Idempotent.
//   docker compose exec -T neo4j cypher-shell -u $NEO4J_USER -p $NEO4J_PASSWORD < scripts/api_use_tracking.cypher
MATCH (c:ApiContext) WHERE c.use_tracked_from IS NULL SET c.use_tracked_from = datetime();
MATCH (c:ApiContext {name: 'agent-brain'}) SET c.retire_exempt = true;
MATCH (c:ApiContext) RETURN c.name AS api, toString(c.created_at) AS created, toString(c.last_used_at) AS last_used, toString(c.use_tracked_from) AS tracked_from, c.retire_exempt AS exempt ORDER BY api;
