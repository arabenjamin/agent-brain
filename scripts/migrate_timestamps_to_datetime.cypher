// Convert every string-stored timestamp in the graph to a native ZONED DATETIME.
//
// WHY: the graph stored timestamps two ways — `Note.created_at` was a datetime
// while `Task.created_at`, `AgentJob.*_at` and others were ISO strings. Cypher
// compares a temporal value to a string as null, so a date filter written for
// one representation silently matched NOTHING against the other: no error, no
// warning, zero rows. That is indistinguishable from "the data isn't there".
// It cost a chat answer ("my memory is clear of any new learnings") on a day
// with 504 new notes, and it had silently killed task dedup —
// `find_similar_tasks` compared a string property to `datetime() - duration`
// and returned 0 instead of 815 on every single call.
//
// SAFETY: every statement is idempotent and guarded by `valueType(...) STARTS
// WITH 'STRING'`, so re-running converts only what is still a string and a
// partial run can simply be repeated. Values are converted, never cleared:
// `datetime(<string>)` throws on an unparseable input rather than writing null,
// which would fail the statement loudly instead of destroying a timestamp.
// A `WHERE <> ''` guard excludes empty strings, which `datetime()` rejects.
//
// Date-only values (`Media.published_at`, `Todo.due_at` are stored as
// `2026-07-24`) parse to midnight UTC, which is the correct reading of a bare
// date and preserves ordering.
//
// Run with:
//   docker exec -i agent-brain-neo4j-1 cypher-shell -u "$NEO4J_USER" \
//     -p "$NEO4J_PASSWORD" -f /path/to/this/file
// The brain MUST be stopped first — the old binary writes strings, so a
// concurrent write would reintroduce exactly what this removes.

// --- :Note -----------------------------------------------------------------
MATCH (n:Note) WHERE valueType(n.asserted_at) STARTS WITH 'STRING' AND n.asserted_at <> ''
SET n.asserted_at = datetime(n.asserted_at);

MATCH (n:Note) WHERE valueType(n.verified_at) STARTS WITH 'STRING' AND n.verified_at <> ''
SET n.verified_at = datetime(n.verified_at);

MATCH (n:Note) WHERE valueType(n.event_at) STARTS WITH 'STRING' AND n.event_at <> ''
SET n.event_at = datetime(n.event_at);

// --- :Task -----------------------------------------------------------------
MATCH (t:Task) WHERE valueType(t.created_at) STARTS WITH 'STRING' AND t.created_at <> ''
SET t.created_at = datetime(t.created_at);

MATCH (t:Task) WHERE valueType(t.updated_at) STARTS WITH 'STRING' AND t.updated_at <> ''
SET t.updated_at = datetime(t.updated_at);

// --- :AgentJob -------------------------------------------------------------
MATCH (j:AgentJob) WHERE valueType(j.created_at) STARTS WITH 'STRING' AND j.created_at <> ''
SET j.created_at = datetime(j.created_at);

MATCH (j:AgentJob) WHERE valueType(j.updated_at) STARTS WITH 'STRING' AND j.updated_at <> ''
SET j.updated_at = datetime(j.updated_at);

MATCH (j:AgentJob) WHERE valueType(j.started_at) STARTS WITH 'STRING' AND j.started_at <> ''
SET j.started_at = datetime(j.started_at);

MATCH (j:AgentJob) WHERE valueType(j.completed_at) STARTS WITH 'STRING' AND j.completed_at <> ''
SET j.completed_at = datetime(j.completed_at);

MATCH (j:AgentJob) WHERE valueType(j.dead_lettered_at) STARTS WITH 'STRING' AND j.dead_lettered_at <> ''
SET j.dead_lettered_at = datetime(j.dead_lettered_at);

MATCH (j:AgentJob) WHERE valueType(j.progress_updated_at) STARTS WITH 'STRING' AND j.progress_updated_at <> ''
SET j.progress_updated_at = datetime(j.progress_updated_at);

// `expires_at` is written as '' when a job has no TTL. Null it rather than
// converting, so `j.expires_at IS NOT NULL` in expire_jobs() means what it says.
MATCH (j:AgentJob) WHERE valueType(j.expires_at) STARTS WITH 'STRING' AND j.expires_at = ''
SET j.expires_at = null;

MATCH (j:AgentJob) WHERE valueType(j.expires_at) STARTS WITH 'STRING' AND j.expires_at <> ''
SET j.expires_at = datetime(j.expires_at);

// --- :ScheduledTask --------------------------------------------------------
MATCH (s:ScheduledTask) WHERE valueType(s.created_at) STARTS WITH 'STRING' AND s.created_at <> ''
SET s.created_at = datetime(s.created_at);

MATCH (s:ScheduledTask) WHERE valueType(s.updated_at) STARTS WITH 'STRING' AND s.updated_at <> ''
SET s.updated_at = datetime(s.updated_at);

MATCH (s:ScheduledTask) WHERE valueType(s.last_run_at) STARTS WITH 'STRING' AND s.last_run_at <> ''
SET s.last_run_at = datetime(s.last_run_at);

MATCH (s:ScheduledTask) WHERE valueType(s.next_run_at) STARTS WITH 'STRING' AND s.next_run_at <> ''
SET s.next_run_at = datetime(s.next_run_at);

// --- :Todo -----------------------------------------------------------------
MATCH (t:Todo) WHERE valueType(t.created_at) STARTS WITH 'STRING' AND t.created_at <> ''
SET t.created_at = datetime(t.created_at);

MATCH (t:Todo) WHERE valueType(t.updated_at) STARTS WITH 'STRING' AND t.updated_at <> ''
SET t.updated_at = datetime(t.updated_at);

MATCH (t:Todo) WHERE valueType(t.due_at) STARTS WITH 'STRING' AND t.due_at = ''
SET t.due_at = null;

MATCH (t:Todo) WHERE valueType(t.due_at) STARTS WITH 'STRING' AND t.due_at <> ''
SET t.due_at = datetime(t.due_at);

// --- :AgentNotification ----------------------------------------------------
MATCH (n:AgentNotification) WHERE valueType(n.created_at) STARTS WITH 'STRING' AND n.created_at <> ''
SET n.created_at = datetime(n.created_at);

MATCH (n:AgentNotification) WHERE valueType(n.read_at) STARTS WITH 'STRING' AND n.read_at <> ''
SET n.read_at = datetime(n.read_at);

// --- :Media / :MediaSource -------------------------------------------------
MATCH (m:Media) WHERE valueType(m.ingested_at) STARTS WITH 'STRING' AND m.ingested_at <> ''
SET m.ingested_at = datetime(m.ingested_at);

// Date-only ('2026-07-24') → midnight UTC.
MATCH (m:Media) WHERE valueType(m.published_at) STARTS WITH 'STRING' AND m.published_at = ''
SET m.published_at = null;

MATCH (m:Media) WHERE valueType(m.published_at) STARTS WITH 'STRING' AND m.published_at <> ''
SET m.published_at = datetime(m.published_at);

MATCH (s:MediaSource) WHERE valueType(s.created_at) STARTS WITH 'STRING' AND s.created_at <> ''
SET s.created_at = datetime(s.created_at);

MATCH (s:MediaSource) WHERE valueType(s.updated_at) STARTS WITH 'STRING' AND s.updated_at <> ''
SET s.updated_at = datetime(s.updated_at);

// --- :SourceList -----------------------------------------------------------
MATCH (s:SourceList) WHERE valueType(s.created_at) STARTS WITH 'STRING' AND s.created_at <> ''
SET s.created_at = datetime(s.created_at);

MATCH (s:SourceList) WHERE valueType(s.updated_at) STARTS WITH 'STRING' AND s.updated_at <> ''
SET s.updated_at = datetime(s.updated_at);

// --- self-model meta-graph (:ToolDef / :ContextProfile / :ModelDef) ---------
MATCH (d:ToolDef) WHERE valueType(d.synced_at) STARTS WITH 'STRING' AND d.synced_at <> ''
SET d.synced_at = datetime(d.synced_at);

MATCH (c:ContextProfile) WHERE valueType(c.synced_at) STARTS WITH 'STRING' AND c.synced_at <> ''
SET c.synced_at = datetime(c.synced_at);

MATCH (d:ModelDef) WHERE valueType(d.synced_at) STARTS WITH 'STRING' AND d.synced_at <> ''
SET d.synced_at = datetime(d.synced_at);

// --- :AgentSpec / :BrainVersion / :ApiCredential ----------------------------
MATCH (a:AgentSpec) WHERE valueType(a.created_at) STARTS WITH 'STRING' AND a.created_at <> ''
SET a.created_at = datetime(a.created_at);

MATCH (v:BrainVersion) WHERE valueType(v.seen_at) STARTS WITH 'STRING' AND v.seen_at <> ''
SET v.seen_at = datetime(v.seen_at);

MATCH (v:BrainVersion) WHERE valueType(v.deployed_at) STARTS WITH 'STRING' AND v.deployed_at <> ''
SET v.deployed_at = datetime(v.deployed_at);

MATCH (c:ApiCredential) WHERE valueType(c.created_at) STARTS WITH 'STRING' AND c.created_at <> ''
SET c.created_at = datetime(c.created_at);

MATCH (c:ApiCredential) WHERE valueType(c.updated_at) STARTS WITH 'STRING' AND c.updated_at <> ''
SET c.updated_at = datetime(c.updated_at);

// --- relationship properties -----------------------------------------------
// (:AgentSpec)-[:PERFORMED {at}]->(:Task) — the constructor's grading history.
MATCH ()-[r:PERFORMED]->() WHERE valueType(r.at) STARTS WITH 'STRING' AND r.at <> ''
SET r.at = datetime(r.at);

// (:Note)-[:CORROBORATED_BY|CONTRADICTED_BY]->() carries `found_at`.
MATCH ()-[r:CORROBORATED_BY]->() WHERE valueType(r.found_at) STARTS WITH 'STRING' AND r.found_at <> ''
SET r.found_at = datetime(r.found_at);

MATCH ()-[r:CONTRADICTED_BY]->() WHERE valueType(r.found_at) STARTS WITH 'STRING' AND r.found_at <> ''
SET r.found_at = datetime(r.found_at);
