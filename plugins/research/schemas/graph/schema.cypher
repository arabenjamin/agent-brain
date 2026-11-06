// The research plugin's Neo4j constraint and index for deep-research runs
// (runs.rs), applied by its `start`. Statements end with `;` and must be
// IF NOT EXISTS.

CREATE CONSTRAINT research_run_id IF NOT EXISTS FOR (r:ResearchRun) REQUIRE r.run_id IS UNIQUE;
CREATE INDEX research_run_created IF NOT EXISTS FOR (r:ResearchRun) ON (r.created_at);
