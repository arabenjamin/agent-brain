// Phase 2b (CORE_AND_PLUGINS_PLAN.md §6): :ScheduledTask and :SchedulerChain
// become :Workflow. Run with the brain STOPPED, after backing up the old nodes,
// then deploy: the seeder finds every yaml workflow already present (same id,
// same timers), so nothing fires early.
//
//   docker compose stop agent-brain
//   cat scripts/phase2b_migrate.cypher | docker compose exec -T neo4j cypher-shell -u neo4j -p <pw>
//   docker compose up -d --build agent-brain

// 1. Every ScheduledTask becomes a Workflow with its id, timers and ownership.
//    A name not in this map (created after the map was written) fails loudly below.
UNWIND [
  {old: "AI Dev Board Job Search", new: "ai-dev-board-job-search"},
  {old: "Bi-weekly SLM benchmark watch: track frontier-equivalent small models in the 1B-7B range", new: "slm-benchmark-watch"},
  {old: "Brain API self-sync: regenerate the brain's own endpoint graph from its Axum routes", new: "brain-api-sync"},
  {old: "Brain exercise: pick a topic from the knowledge graph and create a self-directed research task", new: "brain-exercise"},
  {old: "Brain health monitor: review scheduler state, queue metrics, and failure patterns", new: "health-monitor"},
  {old: "Capability mining: identify agent/tool candidates from todos and repeated patterns", new: "capability-mining"},
  {old: "Claim verification: gather evidence for unverified claims and record what it says", new: "claim-verification"},
  {old: "Daily news aggregation and briefing: aggregate headlines from world, tech, and business, then write and store a daily briefing", new: "daily-news"},
  {old: "Daily news analysis: reason over recent news briefs, extract lasting insights, and queue research threads", new: "news-analysis"},
  {old: "Daily todo review: check outstanding todos and open a chat session to discuss progress, blockers, and next steps with the user", new: "todo-review-chat"},
  {old: "Deep dive: narrow a broad theme into one specific question and answer it", new: "deep-dive-pick"},
  {old: "Discover media sources: propose new channels and podcasts to watch", new: "discover-media-sources"},
  {old: "Entity linking: propose Wikipedia reference links for entities", new: "entity-linking"},
  {old: "Item dispatch: run the user's items assigned to an agent when they are due", new: "item-dispatch"},
  {old: "Media watch: poll subscribed channels for new videos to learn from", new: "media-watch"},
  {old: "Monthly tech dependency synthesis: correlate hardware scarcity against small-model adoption", new: "tech-dependency-synthesis"},
  {old: "Off-Grid Networking Monitor", new: "off-grid-networking-monitor"},
  {old: "Overdue TODO review: check for overdue todos and offer agent assistance", new: "overdue-todo-review"},
  {old: "Relation extraction: propose typed relationships stated in claims", new: "relation-extraction"},
  {old: "SELF-TEST: broken task with dead tool names", new: "self-test-dead-tools"},
  {old: "Todo snapshot: summarise active todos into a reviewable knowledge note", new: "todo-snapshot"},
  {old: "Trump Benefit Daily Monitor", new: "trump-benefit-daily-monitor"},
  {old: "Weekly API scout: discover free public REST APIs for our interests, ingest the keyless ones, flag key-gated ones to the human", new: "api-scout"},
  {old: "Weekly hardware tripwire: scan the AI accelerator supply chain for disruption signals", new: "hardware-tripwire"},
  {old: "Weekly news briefing and analysis: synthesize major world, tech, business, and international stories from the past week", new: "weekly-news"},
  {old: "Zero Trust Architecture Research", new: "zero-trust-architecture-research"},
  {old: "intelligence-agency-claims-monitor", new: "intelligence-agency-claims-monitor"},
  {old: "weekly-sodium-battery-research", new: "weekly-sodium-battery-research"}
] AS m
MATCH (s:ScheduledTask {name: m.old})
CREATE (w:Workflow {
  id: s.id, name: m.new, description: s.description, enabled: coalesce(s.enabled, true),
  interval_seconds: s.interval_seconds, steps: s.steps, inputs: '[]', checks: [],
  is_default: false, next_run_at: s.next_run_at, last_run_at: s.last_run_at,
  created_at: s.created_at, updated_at: datetime(), managed_by: coalesce(s.managed_by, 'runtime'),
  paused_reason: s.paused_reason, topic: s.topic, kind: s.kind
})
WITH s, w
OPTIONAL MATCH (t:Todo)-[:BECAME]->(s)
FOREACH (_ IN CASE WHEN t IS NULL THEN [] ELSE [1] END | CREATE (t)-[:BECAME]->(w))
RETURN count(DISTINCT w) AS workflows_from_schedules;

// Any ScheduledTask the map missed — must be zero before step 4.
MATCH (s:ScheduledTask) WHERE NOT EXISTS { MATCH (w:Workflow {id: s.id}) }
RETURN count(s) AS unmapped_schedules, collect(s.name) AS names;

// 2. Past and pending schedule runs name their workflow (the HBI's run history).
MATCH (t:Task) WHERE t.context STARTS WITH 'Run of ScheduledTask ' AND t.workflow IS NULL
WITH t, split(split(t.context, '(id: ')[1], ')')[0] AS sid
MATCH (w:Workflow {id: sid})
SET t.workflow = w.name
RETURN count(t) AS schedule_runs_backfilled;

// 3. One-off tasks the routing prefixes sent to a pipeline: the same
//    workflow, its input taken from the goal after the prefix (what
//    {{goal_topic}} gave it). JSON-escaped by hand: no APOC here.
MATCH (t:Task) WHERE t.workflow IS NULL AND toLower(t.goal) STARTS WITH 'fill knowledge gap:'
WITH t, trim(substring(t.goal, 19)) AS v
WITH t, replace(replace(replace(replace(v, '\\', '\\\\'), '"', '\\"'), '\n', '\\n'), '\r', '') AS esc
SET t.workflow = 'fill-knowledge-gap', t.inputs_json = '{"topic": "' + esc + '"}'
RETURN count(t) AS fill_knowledge_gap_backfilled;

MATCH (t:Task) WHERE t.workflow IS NULL AND toLower(t.goal) STARTS WITH 'watch video:'
WITH t, trim(substring(t.goal, 12)) AS v
WITH t, replace(replace(replace(replace(v, '\\', '\\\\'), '"', '\\"'), '\n', '\\n'), '\r', '') AS esc
SET t.workflow = 'video-learning', t.inputs_json = '{"url": "' + esc + '"}'
RETURN count(t) AS video_learning_backfilled;

MATCH (t:Task) WHERE t.workflow IS NULL AND toLower(t.goal) STARTS WITH 'read article:'
WITH t, trim(substring(t.goal, 13)) AS v
WITH t, replace(replace(replace(replace(v, '\\', '\\\\'), '"', '\\"'), '\n', '\\n'), '\r', '') AS esc
SET t.workflow = 'article-learning', t.inputs_json = '{"url": "' + esc + '"}'
RETURN count(t) AS article_learning_backfilled;

MATCH (t:Task) WHERE t.workflow IS NULL AND toLower(t.goal) STARTS WITH 'deep dive (hard):'
WITH t, trim(substring(t.goal, 17)) AS v
WITH t, replace(replace(replace(replace(v, '\\', '\\\\'), '"', '\\"'), '\n', '\\n'), '\r', '') AS esc
SET t.workflow = 'deep-dive-hard', t.inputs_json = '{"question": "' + esc + '"}'
RETURN count(t) AS deep_dive_hard_backfilled;

MATCH (t:Task) WHERE t.workflow IS NULL AND toLower(t.goal) STARTS WITH 'deep dive:'
WITH t, trim(substring(t.goal, 10)) AS v
WITH t, replace(replace(replace(replace(v, '\\', '\\\\'), '"', '\\"'), '\n', '\\n'), '\r', '') AS esc
SET t.workflow = 'deep-dive', t.inputs_json = '{"question": "' + esc + '"}'
RETURN count(t) AS deep_dive_backfilled;

// 4. The old nodes and their constraints.
MATCH (s:ScheduledTask) DETACH DELETE s RETURN count(*) AS schedules_deleted;
MATCH (c:SchedulerChain) DETACH DELETE c RETURN count(*) AS chains_deleted;
DROP CONSTRAINT scheduled_task_id IF EXISTS;
DROP CONSTRAINT scheduled_task_name IF EXISTS;
DROP CONSTRAINT scheduler_chain_id IF EXISTS;
