# Todos as Delayed Prompts

**Status: design only, nothing implemented.** Written 2026-09-11. Sequenced 2026-10-08 as Phases 12–13 of `HBI_REDESIGN_PLAN.md` §9, which adds the item record's `origin`, thread and review states and the Feed; the design and guardrails here still hold.

Turn `:Todo` from a passive list into a *delayed prompt*: a goal that carries who
should do it and when, dispatches itself to an agent when it comes due, and
keeps reminding the human when it is theirs.

---

## What exists today

`:Todo {id, title, description, status, priority, tags, due_at, created_at, updated_at}`

- **CRUD is REST-only** (`/todos`, `crates/app/src/clients/rest.rs` →
  `crates/repository/src/todo.rs`). There is **no MCP tool**, so the brain
  cannot read or write its own todos except through raw `neo4j_query`.
- Three schedules read them: `todo-snapshot.yaml` (daily), `todo-review-chat.yaml`
  (daily, opens a chat), `overdue-todo-review.yaml` (12h).
- The front door pre-loads open todos into every chat turn
  (`contexts/general-default-orchestrator.yaml` `pre_load_query`).

**The gap is the missing tool.** Without it nothing can write a result back, and
every schedule that touches todos hand-rolls Cypher — which is how
`overdue-todo-review.yaml` compared a `DATETIME` against `toString(date())` and
silently reported zero overdue todos for months (fixed 2026-09-11).

## Three things make a todo a delayed prompt

| need | field |
|---|---|
| a prompt | `prompt` (falls back to `title` + `description`) |
| a trigger | **reuse `due_at`** — a second timestamp would drift out of sync |
| an assignee | `assignee`: `"user"` \| `"agent:<profile>"` \| `"agent:auto"` \| null |

Plus dispatch bookkeeping: `dispatched_at`, `dispatch_count`, `result_note_id`,
and a `status` value `awaiting_approval`.

---

## Design

### 1. `manage_todo` — the missing primitive

Mirrors `manage_scheduled_task`: `list | get | upsert | assign | approve |
complete`. Everything below depends on it.

`assign` validates `assignee` against the **live spawnable roster** at write
time, not dispatch time — an unknown agent should fail when a human types it,
not silently at 3am.

### 2. Dispatch is a TOOL, not a schedule

**Load-bearing constraint: a ScheduledTask's steps are enqueued verbatim, so a
schedule cannot loop over N due todos.** The established pattern for fan-out in
this codebase is a tool that creates Tasks, driven by a thin schedule — exactly
how `poll_media_sources` works.

So: **`dispatch_due_todos`** + a small schedule that calls it.

The useful realisation: **an agent-assigned todo is just a Task with a delay.**
`create_task(goal: "run agent: <prompt>")` already routes through
`chains/run-agent.yaml` → `run_agent`, and inherits the queue, retries, and
chain-death attribution. The new work is the bridge and the writeback, not an
execution engine. The chain's last step calls `manage_todo(complete, …)`.

### 3. Side effects decide auto vs notify-first

**Auto-dispatch only for agents that write nothing outside the graph.**
Everything else notifies first and waits for a human.

Derived from each profile's tool allowlist (2026-09-11):

| auto-dispatch | notify-first | why |
|---|---|---|
| `knowledge-worker` | `job-applicant` | `mcp_call` — aidevboard `apply_to_job` submits a real application |
| `code-analyst` | `api-scout`, `api-integrator`, `toolsmith`, `researcher` | `http_request` (POST/PUT reach third parties), `manage_dynamic_tool`, `write_workspace_file` |
| `schedule-author` | | |

**Derivation alone is too coarse, and the failure case matters.** `researcher`
is classified notify-first purely because it holds `http_request`, which it uses
for GETs — yet "research X" is the most natural delayed-prompt there is (the live
todo *"Research Sodium batteries"* is exactly this). So:

```yaml
# contexts/researcher.yaml
side_effects: read_only   # http_request is used for GETs; reviewed <date>
```

Absent the key it is **derived from the allowlist and fails safe to
`notify_first`**. An override is a deliberate, greppable, reviewable act. The
honest long-term fix is a GET-only fetch capability, so the declaration becomes
enforced rather than a promise.

### 4. Notify-first flow

Uses machinery that already works:

1. `dispatch_due_todos` finds a due todo assigned to a writing agent.
2. Sets `status: awaiting_approval`; `notify_user` + `push_context` open a
   `todo-<date>` session naming the agent and showing the prompt.
3. The human replies; the front door calls `manage_todo(action: approve, id)` —
   it already owns the write role for exactly this reason (a sub-agent cannot
   write; one writer per turn).
4. That creates the Task → `chains/run-agent.yaml` → writeback.

Approval is **single-use and bound to that todo id**, mirroring the aidevboard
artifact-hash pattern: an approval cannot be replayed against a different todo.

### 5. User-assigned reminders

`daily-news.yaml` (the Daily Brief) already ends
`store_note → claim → notify_user → push_context`. Add a `neo4j_query` step
pulling open `assignee: "user"` todos past due and fold them into the
`notify_user` message. Fixed step count, so it respects the verbatim constraint.

---

## Guardrails

- **Cap `dispatch_count`.** On 2026-09-11 an `ingest_media` job reached attempt
  29 against a cap of 3 because crash recovery requeued it on every restart. An
  uncapped retry outlives restarts; assume anything that can loop, will.
- **Bound fan-out per poll** (like `MEDIA_WATCH_MAX_PER_SOURCE`) so ten due
  todos do not dispatch at once — a cloud agent's cost is a multiplication, not
  a rate.
- **Validate `assignee` against the live roster** at assign time.
- **No recurrence.** A recurring todo is a `ScheduledTask`. CLAUDE.md already
  warns that one-shot Tasks and recurring ScheduledTasks are not interchangeable
  and that cadence words in a goal string do nothing. Todos stay
  one-shot-with-a-delay; a todo that wants recurrence should *create a schedule*.
- **Dispatch must be idempotent.** Status transition + `dispatched_at` guard, so
  a poll that overlaps a slow dispatch cannot fire the same todo twice.

## Open questions

1. Should a completed agent-run todo store its result as a note and link it
   (`result_note_id`), or write the summary onto the todo itself? A note is
   consistent with the rest of the graph and searchable; the todo field is
   simpler for the UI.
2. Does the HBI frontend need an assignee/approval affordance, or is chat enough
   for v1? (Chat is enough to ship; the UI is where a human will actually notice.)
3. `agent:auto` uses `select_agent`, whose token-overlap matching is known to
   score 0 on ordinary phrasings ("what did we learn today"). For an unattended
   dispatch a miss is worse than for a chat turn — consider requiring an explicit
   agent for auto-dispatch and reserving `auto` for the notify-first path, where
   a human sees the choice before it runs.

## Phases

- [ ] **Phase 1 — `manage_todo` + schema fields.** Tool, REST passthrough, repo.
      Nothing dispatches yet, but the brain can finally see and edit its own
      todos. **Ships alone and unblocks everything else.**
- [ ] **Phase 2 — `side_effects` on context profiles.** Derived default,
      explicit override, roster validation.
- [ ] **Phase 3 — `dispatch_due_todos` + schedule + writeback.** Auto for
      read-only agents, `awaiting_approval` for the rest.
- [ ] **Phase 4 — Daily Brief reminders** for `assignee: "user"`.
