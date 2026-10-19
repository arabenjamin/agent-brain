// API_USE_PLAN Phase 1: the owner's hand tags (eval/api_tags_golden.yaml,
// accepted 2026-10-19) onto the registered (:ApiContext)s. Run once after the
// Phase 1 build has seeded (:ApiKind). Idempotent: re-running sets the same tags.
//   docker compose exec -T neo4j cypher-shell -u $NEO4J_USER -p $NEO4J_PASSWORD < scripts/api_tags_owner.cypher

MATCH (c:ApiContext {name: "aidevboard"}) OPTIONAL MATCH (c)-[s:SERVES]->(:Topic) DELETE s;
MATCH (c:ApiContext {name: "aidevboard"}) MATCH (k:ApiKind {name: "job-postings"}) SET c.api_kind = k.name, c.tags_by = 'owner', c.tagged_at = datetime();
MATCH (c:ApiContext {name: "aidevboard"}) UNWIND ["my-job-search", "ai-ml"] AS tn MATCH (t:Topic {name: tn}) MERGE (c)-[:SERVES]->(t);
MATCH (c:ApiContext {name: "jobs-board-remoteok"}) OPTIONAL MATCH (c)-[s:SERVES]->(:Topic) DELETE s;
MATCH (c:ApiContext {name: "jobs-board-remoteok"}) MATCH (k:ApiKind {name: "job-postings"}) SET c.api_kind = k.name, c.tags_by = 'owner', c.tagged_at = datetime();
MATCH (c:ApiContext {name: "jobs-board-remoteok"}) UNWIND ["my-job-search"] AS tn MATCH (t:Topic {name: tn}) MERGE (c)-[:SERVES]->(t);
MATCH (c:ApiContext {name: "hacker_news"}) OPTIONAL MATCH (c)-[s:SERVES]->(:Topic) DELETE s;
MATCH (c:ApiContext {name: "hacker_news"}) MATCH (k:ApiKind {name: "news-feed"}) SET c.api_kind = k.name, c.tags_by = 'owner', c.tagged_at = datetime();
MATCH (c:ApiContext {name: "openviglet_model_catalog"}) OPTIONAL MATCH (c)-[s:SERVES]->(:Topic) DELETE s;
MATCH (c:ApiContext {name: "openviglet_model_catalog"}) MATCH (k:ApiKind {name: "reference-catalog"}) SET c.api_kind = k.name, c.tags_by = 'owner', c.tagged_at = datetime();
MATCH (c:ApiContext {name: "openviglet_model_catalog"}) UNWIND ["ai-ml"] AS tn MATCH (t:Topic {name: tn}) MERGE (c)-[:SERVES]->(t);
MATCH (c:ApiContext {name: "open_meteo"}) OPTIONAL MATCH (c)-[s:SERVES]->(:Topic) DELETE s;
MATCH (c:ApiContext {name: "open_meteo"}) MATCH (k:ApiKind {name: "forecast"}) SET c.api_kind = k.name, c.tags_by = 'owner', c.tagged_at = datetime();
MATCH (c:ApiContext {name: "home_assistant"}) OPTIONAL MATCH (c)-[s:SERVES]->(:Topic) DELETE s;
MATCH (c:ApiContext {name: "home_assistant"}) MATCH (k:ApiKind {name: "device-state"}) SET c.api_kind = k.name, c.tags_by = 'owner', c.tagged_at = datetime();
MATCH (c:ApiContext {name: "github"}) OPTIONAL MATCH (c)-[s:SERVES]->(:Topic) DELETE s;
MATCH (c:ApiContext {name: "github"}) MATCH (k:ApiKind {name: "code-hosting"}) SET c.api_kind = k.name, c.tags_by = 'owner', c.tagged_at = datetime();
MATCH (c:ApiContext {name: "brave"}) OPTIONAL MATCH (c)-[s:SERVES]->(:Topic) DELETE s;
MATCH (c:ApiContext {name: "brave"}) MATCH (k:ApiKind {name: "search"}) SET c.api_kind = k.name, c.tags_by = 'owner', c.tagged_at = datetime();
MATCH (c:ApiContext {name: "google_cse"}) OPTIONAL MATCH (c)-[s:SERVES]->(:Topic) DELETE s;
MATCH (c:ApiContext {name: "google_cse"}) MATCH (k:ApiKind {name: "search"}) SET c.api_kind = k.name, c.tags_by = 'owner', c.tagged_at = datetime();
MATCH (c:ApiContext {name: "serpapi"}) OPTIONAL MATCH (c)-[s:SERVES]->(:Topic) DELETE s;
MATCH (c:ApiContext {name: "serpapi"}) MATCH (k:ApiKind {name: "search"}) SET c.api_kind = k.name, c.tags_by = 'owner', c.tagged_at = datetime();
MATCH (c:ApiContext) OPTIONAL MATCH (c)-[:SERVES]->(t:Topic) RETURN c.name AS api, c.api_kind AS kind, collect(t.name) AS topics, c.tags_by AS by ORDER BY api;
