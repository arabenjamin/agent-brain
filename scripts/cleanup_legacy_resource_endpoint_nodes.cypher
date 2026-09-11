// Cleanup: remove the legacy (:Resource)-[:HAS_ENDPOINT]->(:Endpoint) structure.
//
// WHY: the current REST-ingestion model is (:ApiContext)-[:EXPOSES]->(:ApiEndpoint)
// (see `ingest_api` in skills/http.rs). An older, never-ported structure from the
// original agent-api — (:Resource)-[:HAS_ENDPOINT]->(:Endpoint) — was left behind
// in the graph: measured 2026-09-11 at 18 Resource + 40 Endpoint nodes (demo/test
// data, names like "Chuck Norris", "Funders", "Works", "Text Classification"),
// with ZERO links to any ApiContext and ZERO references in the current code
// (`grep -rn HAS_ENDPOINT crates` = 0).
//
// It is not merely dead — it is actively misleading. `describe_graph`
// (services/graph_schema.rs) introspects live labels/relationships, so it
// surfaced BOTH endpoint structures and BOTH relationship types. A chat asked to
// "list the endpoints for HomeAssistant" then hand-wrote Cypher that
// non-deterministically picked the legacy (:Resource)-[:HAS_ENDPOINT] path (or a
// mis-cased api_name), got zero rows, and reported the endpoints as "not stored /
// schema needs examining" — while the 20 real endpoints sat under EXPOSES. The
// deterministic `list_endpoints(api)` tool now bakes the correct query, and this
// migration removes the ambiguity at the source.
//
// Idempotent: re-running after the nodes are gone deletes nothing. This was run
// live on the primary graph 2026-09-11; the script exists so a restore-from-
// snapshot or a second deployment can reproduce it (data mutations do not travel
// with the code commit).
//
// Run the preview first, then the delete:
//   export $(grep NEO4J_PASSWORD .env)
//   docker exec -i agent-brain-neo4j-1 cypher-shell -u neo4j -p "$NEO4J_PASSWORD" \
//     < scripts/cleanup_legacy_resource_endpoint_nodes.cypher

// --- Preview: how many legacy nodes, and confirm none touch an ApiContext ---
MATCH (n) WHERE n:Resource OR n:Endpoint
OPTIONAL MATCH (n)-[:HAS_ENDPOINT]-()
WITH count(DISTINCT n) AS legacy_nodes, count(*) AS with_rel_rows
OPTIONAL MATCH (:ApiContext)-->(x) WHERE x:Resource OR x:Endpoint
RETURN legacy_nodes, with_rel_rows, count(x) AS linked_to_apicontext;

// --- Delete: remove the legacy nodes and their HAS_ENDPOINT relationships ---
// (DETACH removes the relationships; only single-label Resource/Endpoint nodes
// match, verified before the live run.)
MATCH (n) WHERE n:Resource OR n:Endpoint
DETACH DELETE n
RETURN count(*) AS deleted;
