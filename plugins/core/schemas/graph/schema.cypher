// Core's Neo4j constraints and indexes, applied in order on every start by
// Neo4jClient::init_schema (crates/repository/src/client.rs). Each statement
// ends with `;` and must be idempotent (IF NOT EXISTS). A plugin's own labels
// are in its bundle's schemas/graph/schema.cypher. Docs: project-docs/schema.md.

// ---------------------------------------------------------------- constraints

CREATE CONSTRAINT workflow_id IF NOT EXISTS FOR (w:Workflow) REQUIRE w.id IS UNIQUE;
CREATE CONSTRAINT workflow_name IF NOT EXISTS FOR (w:Workflow) REQUIRE w.name IS UNIQUE;
CREATE CONSTRAINT working_memory_id IF NOT EXISTS FOR (w:WorkingMemory) REQUIRE w.id IS UNIQUE;
CREATE CONSTRAINT entity_name IF NOT EXISTS FOR (e:Entity) REQUIRE e.name IS UNIQUE;
// Entity resolution: an alias name means one entity (services/entity_resolution.rs).
CREATE CONSTRAINT entity_alias_name IF NOT EXISTS FOR (a:EntityAlias) REQUIRE a.name IS UNIQUE;
// Topics (services/topics.rs): one node per topics/*.yaml file.
CREATE CONSTRAINT topic_name IF NOT EXISTS FOR (t:Topic) REQUIRE t.name IS UNIQUE;
// Programs (PROGRAMS_PLAN): one node per name; MERGE relies on it.
CREATE CONSTRAINT program_name IF NOT EXISTS FOR (p:Program) REQUIRE p.name IS UNIQUE;
// Revisions (OUTSIDE_REVIEW_PLAN Phase 2): one per object and version.
CREATE CONSTRAINT revision_uid IF NOT EXISTS FOR (r:Revision) REQUIRE r.uid IS UNIQUE;
CREATE CONSTRAINT agent_job_id IF NOT EXISTS FOR (j:AgentJob) REQUIRE j.id IS UNIQUE;
CREATE CONSTRAINT todo_id IF NOT EXISTS FOR (t:Todo) REQUIRE t.id IS UNIQUE;
// Note and Task are the hottest by-id lookups (get_note, per-search access-stat
// updates, SUMMARIZED_BY linking, update_task_status every scheduler tick).
// Without these, every `MATCH (n:Note {id: $id})` is a full label scan.
CREATE CONSTRAINT note_id IF NOT EXISTS FOR (n:Note) REQUIRE n.id IS UNIQUE;
CREATE CONSTRAINT task_id IF NOT EXISTS FOR (t:Task) REQUIRE t.id IS UNIQUE;

// -------------------------------------------------------------------- indexes

// Vector index for RAG (bge-m3 = 1024 dim).
CREATE VECTOR INDEX note_embeddings IF NOT EXISTS FOR (n:Note) ON (n.embedding)
  OPTIONS {indexConfig: {`vector.dimensions`: 1024, `vector.similarity_function`: 'cosine'}};
// Spaced repetition.
CREATE INDEX note_next_review IF NOT EXISTS FOR (n:Note) ON (n.next_review_at);
// Full-text index for BM25 hybrid search.
CREATE FULLTEXT INDEX note_content_fulltext IF NOT EXISTS FOR (n:Note) ON EACH [n.content];
CREATE INDEX working_memory_session IF NOT EXISTS FOR (w:WorkingMemory) ON (w.session_id);
CREATE INDEX entity_type_idx IF NOT EXISTS FOR (e:Entity) ON (e.entity_type);
// Entity resolution looks names up by normalised key, and the HBI by id.
CREATE INDEX entity_key_idx IF NOT EXISTS FOR (e:Entity) ON (e.key);
CREATE INDEX entity_id_idx IF NOT EXISTS FOR (e:Entity) ON (e.id);
CREATE INDEX entity_alias_key_idx IF NOT EXISTS FOR (a:EntityAlias) ON (a.key);
CREATE INDEX revision_kind_name IF NOT EXISTS FOR (r:Revision) ON (r.kind, r.name);
CREATE INDEX agent_job_status IF NOT EXISTS FOR (j:AgentJob) ON (j.status);
CREATE INDEX agent_job_priority IF NOT EXISTS FOR (j:AgentJob) ON (j.priority);
CREATE INDEX agent_job_created IF NOT EXISTS FOR (j:AgentJob) ON (j.created_at);
// parent_job_id drives unpark_children / cancel_parked_children on every chain step.
CREATE INDEX agent_job_parent IF NOT EXISTS FOR (j:AgentJob) ON (j.parent_job_id);
// Task.status is scanned every scheduler tick via list_tasks(status='created').
CREATE INDEX task_status IF NOT EXISTS FOR (t:Task) ON (t.status);
CREATE INDEX task_workflow IF NOT EXISTS FOR (t:Task) ON (t.workflow);
// Recency ordering (list_tasks, a workflow's latest run, the oldest due item).
// Live and documented before 2026-11-06, but no code created it, so a fresh
// install lacked it.
CREATE INDEX task_created IF NOT EXISTS FOR (t:Task) ON (t.created_at);
// What a run produced (`produced_by_task`): the workflow scorecard looks every
// run's notes up by it; unindexed it scanned all notes once per workflow (45 s
// for 41 workflows, 2026-10-17).
CREATE INDEX note_produced_by_task IF NOT EXISTS FOR (n:Note) ON (n.produced_by_task);
CREATE INDEX todo_status IF NOT EXISTS FOR (t:Todo) ON (t.status);
CREATE INDEX todo_priority IF NOT EXISTS FOR (t:Todo) ON (t.priority);
CREATE INDEX agent_notification_read IF NOT EXISTS FOR (n:AgentNotification) ON (n.read);
CREATE INDEX agent_notification_created IF NOT EXISTS FOR (n:AgentNotification) ON (n.created_at);
// ingest_api: "what endpoints does this API expose" lookups.
CREATE INDEX api_endpoint_api IF NOT EXISTS FOR (e:ApiEndpoint) ON (e.api_name);
