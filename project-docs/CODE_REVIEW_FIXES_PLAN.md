# Code Review Fixes — Implementation Plan

Source: full-codebase review on 2026-08-03 (queue, scheduler, LLM client, knowledge
service, repository layer, schema, HTTP auth). Each phase is independently
shippable and ordered by value-for-effort. Phases 1–3 are the priority batch.

**Conventions (do not skip):**
- Update docs (CLAUDE.md / project-docs) in the same commit as the behavior change.
- No LLM attribution in commit messages (see CLAUDE.md Branch Strategy note).
- Work on `dev`. `cargo fmt && cargo clippy && cargo test --lib` must pass per phase.
- Integration tests (`cargo test --test '*'`) require Neo4j running (`docker compose up -d`).

---

## Phase 1 — Chain template substitution breaks on quotes (bug)

**Files:** `crates/app/src/services/scheduler.rs` (~line 1265 `try_load_chain_from_neo4j`,
~line 499 `dispatch_one_scheduled_task`)

**Problem:** `{{goal}}` / `{{task_id}}` / `{{date}}` / `{{file_slug}}` are substituted
into the chain's **serialized JSON string** before `serde_json::from_str`. A goal
containing `"`, `\`, or a newline corrupts the JSON → parse fails → task silently
falls back to `build_diagnosis_chain`. Real occurrence in the graph: task
`fill knowledge gap: The "Priority Research Topic" section is not present…` (status
blocked). LLM-generated goals contain quotes routinely.

**Fix:**
1. Parse `steps_json` into `Vec<ChainStep>` (or `serde_json::Value`) FIRST.
2. Substitute template vars at the Value level — walk string values only, exactly
   like `substitute_prev()` in `crates/app/src/services/queue.rs:1522`. Extract a
   shared helper, e.g. `substitute_template_vars(val: &Value, vars: &[(&str, &str)]) -> Value`,
   put it in `queue.rs` next to `substitute_prev` (or a small `services/template.rs`),
   and reuse it for both `substitute_prev` and chain substitution.
3. Apply the same fix in `dispatch_one_scheduled_task` (same pattern, `st.steps`).

**Tests:** unit test: a chain step template `{"arguments":{"question":"{{goal}}"}}`
with goal `say "hello" \ world` round-trips to valid steps with the literal quote
preserved. Existing chain-routing tests must still pass.

**Docs:** CLAUDE.md "Chain YAML schema" section — note that template substitution is
value-level and quote-safe.

---

## Phase 2 — Missing constraints/indexes on hot labels (perf + integrity)

**File:** `crates/repository/src/client.rs` (`init_schema`, lines ~36–72)

**Problem:** No unique constraint on `Note.id` or `Task.id`. Every by-id MATCH
(`get_note`, access-stat updates after each search hit, `SUMMARIZED_BY` linking,
`update_task_status` every scheduler tick) is a full label scan (~3k notes). Also
missing: index on `Task.status` (scanned every tick via `list_tasks(Some("created"))`)
and `AgentJob.parent_job_id` (used by `unpark_children` / `cancel_parked_children`).

**Fix:** add to `init_schema`:
```cypher
CREATE CONSTRAINT note_id IF NOT EXISTS FOR (n:Note) REQUIRE n.id IS UNIQUE
CREATE CONSTRAINT task_id IF NOT EXISTS FOR (t:Task) REQUIRE t.id IS UNIQUE
CREATE INDEX task_status IF NOT EXISTS FOR (t:Task) ON (t.status)
CREATE INDEX agent_job_parent IF NOT EXISTS FOR (j:AgentJob) ON (j.parent_job_id)
```
`init_schema` runs on `init-db`; confirm whether it also runs at server startup —
if not, note in the rollout section of the commit message that `cargo run -- init-db`
(or the dockerized equivalent) must be run once after deploy.

**Pre-check:** a unique constraint fails to create if duplicates exist.
✅ Already verified on the live DB (2026-08-03): zero duplicate `Note.id` and zero
duplicate `Task.id` — the constraints will create cleanly. Make
`init_schema` log-and-continue on individual statement failure if it doesn't already,
so one failed constraint doesn't abort the rest.

**Docs:** `project-docs/schema.md` — add the new constraints/indexes.

---

## Phase 3 — Evaluator score parse failure = spurious task failure (bug)

**File:** `crates/app/src/services/queue.rs` (`parse_evaluator_score`, ~line 1398;
call site ~line 999)

**Problem:** When evaluator output has no `Score: N/5` line and no verdict keyword,
`parse_evaluator_score` returns 3.0 — **below** the default `min_score` 3.5 — so
format drift from the local model fails the task and burns up to 3 retry chains.
Note the inconsistency: the adversarial gate's fallback (3.0 vs threshold 2.5) passes.

**Fix:** make "unparseable" explicit instead of a fake mid-scale score:
1. Change signature to `parse_evaluator_score(text: &str) -> Option<f32>`; return
   `None` when no score line AND no verdict keyword matches.
2. At the call site: on `None`, log a warning (`"Evaluator output unparseable — treating as pass"`),
   skip the requeue gate (treat as pass), and still record nothing onto the AgentSpec
   PERFORMED edge (don't grade with invented numbers).
3. Keep the existing keyword fallbacks (FULLY/PARTIALLY/NOT MET) as real parses.

**Tests:** unit tests for: explicit score, each verdict keyword, garbage text → `None`.
Verify the requeue path is NOT taken for `None`.

**Docs:** CLAUDE.md "Evaluator Loop" section — document the unparseable-output behavior.

---

## Phase 4 — Cancel race: finalizers overwrite `cancelled` (bug)

**Files:** `crates/repository/src/agent_job.rs` (`set_job_started` ~174,
`set_job_completed` ~189, `set_job_failed` ~226, `set_job_dead` ~242,
`requeue_for_retry` ~211); `crates/app/src/services/queue.rs` (`execute_job` ~903)

**Problem:** `cancel()` sets status `cancelled` in Neo4j, but a job already executing
finishes and `set_job_completed` unconditionally overwrites the status — the
"cancelled" job completes and its chain children unpark.

**Fix (guard at the Cypher level, races resolve in the DB):**
1. `set_job_started`: add `WHERE j.status = 'queued'` and return whether a row was
   updated (e.g. `RETURN count(j) AS n`). In `execute_job`, if no row updated, log
   and return — the job was cancelled (or already picked up) between pop and start.
2. `set_job_completed` / `set_job_failed` / `set_job_dead` / `requeue_for_retry`:
   add `WHERE j.status = 'running'`. Return updated-count; when 0, skip the
   follow-on effects in `execute_job` (unpark children, evaluator gate, events).
3. Keep the tombstone `cancelled_ids` heap mechanism as-is (it handles the
   not-yet-popped case; the Cypher guards handle the already-running case).

**Tests:** repository integration test: create job → set running → cancel →
`set_job_completed` → status must still be `cancelled`. Unit-level: `execute_job`
skips unpark when finalize reports 0 rows (may need a small refactor for testability;
keep it minimal).

---

## Phase 5 — Coordinator head-of-line blocking + slow permit wakeup

**File:** `crates/app/src/services/queue.rs` (`run_coordinator` ~771–833,
`execute_job` end)

**Problem (two parts):**
- Drain loop pops a job; if that provider's semaphore is full it pushes the job back
  and `break`s — blocking dispatch for OTHER providers with free permits.
- Nothing notifies the coordinator when a permit frees: after a completed job, the
  next queued job waits up to `poll_interval_secs` (30 s).

**Fix:**
1. End of `execute_job` (all paths — completed, failed, dead): `self.notify.notify_one()`.
   (The permit drops when the spawned task ends; notify right before so the coordinator
   re-drains promptly.)
2. In the drain loop, replace push-back-and-`break` with skip-and-continue: keep a
   local `Vec<PrioritizedJob> blocked`; on `try_acquire` failure push the job there and
   `continue`; after the loop, push all `blocked` back onto the heap. Break only when
   the heap is empty or every provider seen this pass is saturated (i.e. all remaining
   pops land in `blocked` — a simple guard: stop after N consecutive blocked pops
   where N = heap size at loop entry, to avoid spinning).

**Tests:** hard to unit test end-to-end; at minimum add a test for the skip logic if
the drain loop is factored into a testable function. Otherwise verify via
`cargo test --lib` regressions plus a manual smoke: enqueue 3 ollama jobs with
max_concurrent_ollama=1 and observe they run back-to-back without 30 s gaps
(job `updated_at` timestamps in Neo4j).

---

## Phase 6 — Lucene query sanitization in BM25 search

**File:** `crates/app/src/services/knowledge.rs` (`search_notes_inner` ~827–849,
same pattern in `search_notes_with_ids` ~1420)

**Problem:** raw query text goes to `db.index.fulltext.queryNodes` (Lucene syntax).
Special chars (`:` `/` `[` `]` `(` `)` `~` `^` `"` `AND/OR/NOT`, unbalanced quotes —
note content is full of URLs) throw; the `if let Ok` swallows the error and search
silently degrades to the CONTAINS fallback scan.

**Fix:**
1. Add `fn sanitize_lucene_query(q: &str) -> String` — escape Lucene special
   characters with `\` (list: `+ - && || ! ( ) { } [ ] ^ " ~ * ? : \ /`), collapse
   to terms. Simplest robust approach: escape everything, join terms with spaces
   (Lucene ORs terms by default).
2. Apply in both call sites.
3. Replace the silent `if let Ok` on BOTH vector and fulltext calls with
   `match … Err(e) => warn!(…)` so infra failures are visible in logs (behavior
   unchanged: continue with empty hits).

**Tests:** unit test sanitizer (URL input, colons, quotes, `AND`). Integration test
(Neo4j): `search_notes("https://example.com/path: [test]")` returns Ok.

---

## Phase 7 — Meta-learning fires on quota/transient errors

**File:** `crates/app/src/services/queue.rs` (dead-job branch of `execute_job`
~1146–1219, `should_meta_learn` ~1462)

**Problem:** a dead `search_web` job from `429 "run out of searches"` triggers a full
meta-learning chain — the brain has repeatedly hypothesized about a billing limit
(11 `dead_job:search_web` notes). Wasted LLM cycles and graph noise.

**Fix:**
1. Add `fn is_transient_infra_error(error_text: &str) -> bool` — case-insensitive
   match on: `429`, `too many requests`, `rate limit`, `quota`, `run out of searches`,
   `timeout`, `timed out`, `connection refused`, `503`, `502`. When true, store the
   reflection note (keep forensics) but skip the meta-learning chain.
2. Dedup: before enqueuing, query for an existing note with
   `source_context = 'dead_job:<tool>'` created in the last 24 h
   (`note_type = 'meta_learning_result'`); skip if found. One Cypher lookup, cheap.

**Tests:** unit test `is_transient_infra_error` against the SerpApi 429 string and a
genuine logic-error string.

---

## Phase 8 — Recursive chain cancellation

**File:** `crates/repository/src/agent_job.rs` (`cancel_parked_children` ~427)

**Problem:** cancellation reaches only direct children; grandchildren wait for the
5-minute orphan audit, one generation per pass (a 5-step chain takes ~15 min to
fully clear).

**Fix:** loop `cancel_parked_children` until it cancels 0 rows (each pass turns the
next generation's parents terminal), cap at e.g. 32 iterations. Do the loop in the
repository fn so all call sites (cancel, drain, dead-job path, evaluator gate)
benefit. Alternative single-query approach is awkward because `parent_job_id` is a
property, not an edge — the loop is fine at these sizes.

**Tests:** integration test: 4-step chain, kill step 1 (cancel), assert steps 2–4 are
all `cancelled` immediately.

---

## Phase 9 — Small cleanups (batch into one commit)

1. **Constant-time API key compare** — `crates/app/src/mcp/auth.rs:137`. Add `subtle`
   crate (`ConstantTimeEq` on bytes) or fold a manual XOR loop. One line + dependency.
2. **`enqueue` create-then-refetch round trip** — `crates/repository/src/agent_job.rs`
   `create_agent_job` / `create_agent_job_parked`: append `RETURN j` and map with the
   existing `node_to_agent_job`; return `AgentJob` instead of `String` id (or add a
   `_returning` variant). Update `QueueService::enqueue` / `enqueue_chain` / `retry`
   to drop the extra `get_agent_job` calls.
3. **`drain()` completeness** — `crates/app/src/services/queue.rs:489`: after draining
   the heap, also cancel Neo4j `queued` jobs not in the heap
   (`MATCH (j:AgentJob {status:'queued'}) SET j.status='cancelled'` + cancel their
   parked children). Otherwise the next 30 s poll resurrects them.
4. **Delete `goal_to_steps_legacy`** — `crates/app/src/services/scheduler.rs:1324`,
   ~650 lines of `#[allow(dead_code)]` heuristics. Chain seeding is mandatory now
   (missing `schedules/` aborts startup; chains seed on first tick). Also delete any
   helpers only it used (compiler will tell). Keep `build_diagnosis_chain` (live
   fallback).

---

## Suggested commit sequence

| # | Commit | Phases |
|---|--------|--------|
| 1 | `fix: value-level template substitution in chain loading` | 1 |
| 2 | `perf: unique constraints and indexes for Note/Task/AgentJob lookups` | 2 |
| 3 | `fix: treat unparseable evaluator output as explicit non-score` | 3 |
| 4 | `fix: guard job finalizers against cancel race` | 4 |
| 5 | `fix: per-provider skip + permit-release wakeup in coordinator` | 5 |
| 6 | `fix: sanitize Lucene queries; surface hybrid-search failures` | 6 |
| 7 | `fix: skip meta-learning on transient infra errors; daily dedup` | 7 |
| 8 | `fix: cascade chain cancellation to all descendants` | 8 |
| 9 | `chore: auth timing, enqueue round trips, drain completeness, dead code` | 9 |

## Rollout notes

- After Phase 2 merges: run `init-db` (or restart, if `init_schema` runs at startup)
  and verify constraints exist: `SHOW CONSTRAINTS`.
- The running container must be rebuilt (`docker compose up -d --build`) to pick up
  any of this; last rebuild was 2026-08-03.
- Watch after deploy: dead-letter queue size (Phase 7 should reduce it), scheduler
  logs for "diagnosis fallback" warnings (Phase 1 should eliminate the quote-goal
  cases), and job `updated_at` gaps (Phase 5 should remove 30 s stalls).
