# Sub-Agent Orchestration in Chat — Design Proposal

Status: **Phase 0 implemented behind `SUBAGENT_ENABLED` (default OFF); awaiting
measurement.** This document proposes an orchestrator/worker pattern for the
human-facing `/chat` path: a conversational model that stays in the dialogue and
delegates bounded, tool-heavy sub-tasks to worker sub-agents whose results return
*into the same turn*.

**What Phase 0 shipped (see `crates/app/src/clients/chat.rs`):** a `delegate`
pseudo-tool added to the orchestrator when `SUBAGENT_ENABLED` is set, intercepted
in `run_ollama_cloud_loop` (the current gpt-oss chat path) and run as a nested,
read-only worker via `ChatService::run_subagent`; a curated hardcoded roster
(`SPAWNABLE_PROFILES`); depth cap 1 (sub-agents get no `delegate`); progress
forwarded to the user, final prose captured and folded back as the tool result.
Guard tests: `general` is never spawnable; the tool contract is `agent`+`task`.

**Measured 2026-09-06 (two turns, "What have we learned today?"):**
- gpt-oss orchestrator (design B): **0 delegations** — it looped its own
  `search_notes` to the 10-round budget. Design B rejected.
- gemma orchestrator (design A, `contexts/orchestrator.yaml`): **delegated on
  its first call** (gemma×3 + gpt-oss×11 = 14 calls). Delegation works; the
  worker (gpt-oss) then looped `search_notes` to *its* budget and summarised
  unrelated notes — the sub-agent cap, not the plumbing, was the problem.

**Phase 1 progress:**
- ✅ **Sub-agent iteration cap** — `MAX_SUBAGENT_ITERATIONS` (3) is threaded
  through all four provider loops via a `max_iterations` param; `run_one_attempt`
  passes `MAX_TOOL_ITERATIONS`, `run_subagent` passes `MAX_SUBAGENT_ITERATIONS`.
  Validated 2026-09-07: the delegated worker hit `rounds:3` (vs 11 before), turn
  calls 14→7, latency 20s→11s. Guard: `subagent_budget_is_tighter_than_the_orchestrator`.
- ✅ **`search_notes` empty-query hardening** — an empty/whitespace `entity_name`
  is now treated as absent (mirrors the existing empty-`query` guard), so a
  worker looping `search_notes` with `entity_name: ""` gets a clear "supply a
  real query" error instead of noise it reads as success and loops on. Guards:
  `search_notes_empty_entity_name_is_not_searched`,
  `empty_entity_name_with_a_real_query_falls_through_to_query` (`skills/knowledge.rs`).

- ✅ **Structured findings** — `run_subagent` no longer returns bare prose. It
  collects the worker's successful tool-result previews, and `format_subagent_findings`
  wraps the answer with a relay instruction (cite sources, pass along stated
  limitations, add no facts) plus a `SOURCES:` list of URLs lifted straight from
  the worker's tool outputs via `extract_sources` (the worker's prose can drop
  links; its tool results can't). Guards: `extract_sources_*`, `findings_*`.
  Note: this delivers `answer` + `sources`; a first-class `gaps` array is still
  carried as prose in the answer, not a separate field.

- ✅ **Grounding-flag propagation** — `run_subagent` now also reports whether the
  worker ran ≥1 successful tool (`retrieved`); the dispatcher calls
  `TurnWriteGuard::mark_retrieval()` on a retrieving delegate, so a `semantic`
  note the orchestrator writes afterward is no longer wrongly downgraded to
  `unsourced_synthesis`. Guard: `a_delegate_retrieval_grounds_a_semantic_note`.
- ✅ **All-loop interception** — the delegate interception + grounding was
  extracted into one `ChatService::dispatch_tool_call` that **all four** provider
  loops route through (replacing their direct `execute_guarded` calls). Each
  loop gained an `allow_delegate` param; `run_one_attempt` passes `true`,
  `run_subagent` passes `false` (depth cap 1). Delegation now works whichever
  provider the orchestrator runs on, and the interception is defined once rather
  than copied per loop — closing the "handled in one path, not the other" risk.
- ✅ **Sub-agent cap 3→6 + `note_type` hardening** — a 2026-09-07 orchestrator
  turn ("the latest on Our Niche Intelligence Agency") wrongly answered "no
  information" where `general` succeeded. Root cause was a THIRD empty-arg-as-
  filter bug: the worker passed `note_type: ""`, which `search_notes` treated as
  *filter for type ""* → zero results → false negative. Fixed systematically:
  `serde_flex::empty_string_as_none` normalizes empty/whitespace optional filter
  args to `None` at the **deserialization boundary**, applied to every
  `search_notes` filter (`query`/`entity_name`/`entity_type`/`note_type`) and the
  `search_web` filters (`engine`/`source_list`) — so a new filter field is
  covered by adding the attribute, not by remembering a per-handler guard (this
  was the third reactive patch of the same bug class). Ancillary: the sub-agent
  cap was raised 3→6 (the old junk-loop reason
  was already hardened away, and 3 starved query refinement) and the
  `researcher`/`knowledge-worker` prompts nudged to strip possessives, prefer
  `query` over `entity_name`, and not conclude "not found" from one miss. Guard:
  `empty_note_type_is_not_treated_as_a_filter`.

**Phases 1 and 2 are complete.** The gemma-orchestrator design is validated, the
primitive is hardened (bounded, grounded, structured, provider-agnostic), and the
worker roster is data-driven (`spawnable`/`specialty` profile fields — no
hardcoded list). What remains is forward work: Phase 3 (streaming/episodic-note
polish) and Phase 4 (re-express the worker/voice split on `delegate`).

It also folds in the broader question the work raised — "how agents are put
together and get used" — by formalising **agent = context profile** and giving
the orchestrator a way to see and call the roster of agents that exist.

Read `project-docs/architecture_context.md` and the chat sections of `CLAUDE.md`
(worker/voice split, write guard, fallback ladder, Context Profiles) before
implementing; this design is constrained by hard-won behaviour in those.

---

## 1. Motivation

Different models have complementary, non-overlapping strengths in this fleet:

- **gpt-oss:120b-cloud** — fast, a willing and reliable tool caller, never
  returns empty completions. But it fills gaps with fluent invention and loops
  until its tool budget is gone.
- **gemma4:31b-cloud** — nicer to converse with, and says "we did not find
  that" instead of inventing. Slower, ~1-in-4 empty-completion rate on a
  tool-calling prompt (survivable for a single composition call, not for a
  tool loop).
- **Gemini free tier** — good, but 5 requests/minute makes it unfit for
  agentic chat (one tool-using turn is many model calls; measured 2026-09-06 as
  8 Gemini calls in 17s → 429 mid-turn). Fine for background reasoning
  (1 call/step).

The current **worker/voice split** (`CHAT_SYNTHESIS_PROVIDER`) already exploits
the first two: gpt-oss runs the whole tool loop, gemma composes the reply. It
works but is **rigid** — exactly one worker pass, then exactly one voice pass,
no interleaving. The user cannot ask a follow-up that the voice model answers
from conversation *and* have it go fetch one new fact without re-running the
entire worker loop.

The goal: let the **conversational** model lead the turn and **delegate** a
scoped lookup to a **tool-capable** worker on demand, get the result back, and
keep talking — as many times as the conversation needs.

---

## 2. What already exists (and why it isn't enough)

### 2.1 The chat loop (`crates/app/src/clients/chat.rs`)
- `ChatService::run` orchestrates a turn: resolve a context profile
  (`auto_assign` / explicit), build the tool allowlist + system prompt +
  `pre_load_query` notes, apply the profile's `model_preference`
  (`resolve_profile_model`), then run the active model followed by the
  `chat_fallback_ladder` until one **delivers**.
- `run_one_attempt` runs one model on the turn and classifies the result
  `Delivered` vs `Unanswered` (nothing reached the client → the ladder may try
  another model).
- Four provider loops (`run_anthropic_loop`, `run_ollama_tool_loop`,
  `run_ollama_cloud_loop`, `run_text_loop`) each iterate up to
  `MAX_TOOL_ITERATIONS`, dispatching tool calls through `execute_guarded`.
- `execute_guarded` + `TurnWriteGuard` enforce grounding (downgrade an
  ungrounded `semantic` note to `unsourced_synthesis`) and idempotence
  (suppress a byte-identical write called twice in a turn).
- `run_synthesis` (the worker/voice split): after the worker loop gathers
  results, a separate model composes the final reply from what was gathered
  (passed as an argument, not scraped from history).

### 2.2 Agents are already profiles (`contexts/*.yaml`)
A `ContextProfile` (`services/context_builder.rs`) is already an agent
definition: `tools` allowlist, `system_prompt`, `pre_load_query`, and — as of
the 2026-09-06 work — a `model_preference`/`provider_hint` that is **honored on
the chat path** (`resolve_profile_model`). So "a research sub-agent" needs no
new concept; it is a profile (e.g. `researcher` = a tool-capable model +
`search_web`/`search_notes`).

### 2.3 The async agent infrastructure — the reason we need something new
The brain already spawns agents, but **asynchronously**:
- `AgentJob` priority queue + coordinator (`services/queue.rs`).
- `construct_agent` (`skills/constructor.rs`) plans an `(:AgentSpec)` chain and
  optionally dispatches it as a Task.
- Scheduler chains (`chains/*.yaml`).

None of this can serve an inline chat lookup: **their output never returns to
the live turn.** The chat system prompt deliberately forbids `enqueue_jobs` in
chat for exactly this reason ("background jobs run asynchronously and their
output will NEVER appear here"). The missing capability is a **synchronous**
sub-agent whose result is available before the turn ends.

---

## 3. Core proposal

### 3.1 Agent = context profile (formalise)
Keep one definition of "agent": a `ContextProfile`. Add two optional fields:

- `spawnable: bool` (default `false`) — may this profile be invoked as a
  sub-agent by an orchestrator? Keeps `general` (the human entry point) from
  being spawned recursively, and lets us curate a deliberate worker roster.
- `specialty: String` (optional) — a one-line description of what this agent is
  for, injected into the orchestrator's prompt so it knows *when* to delegate to
  it. (Distinct from `description`, which is human-facing UI text.)

No new node type; this rides on the existing `(:ContextProfile)` sync in
`services/self_model.rs`.

### 3.2 The delegation primitive
A single new capability the orchestrator can invoke:

```
delegate(agent: string, task: string) -> { answer, sources[], gaps[] }
```

- `agent` — a `spawnable` profile name.
- `task` — a self-contained instruction (the orchestrator must include the
  context the sub-agent needs; the sub-agent does **not** see the chat history
  by default — see §7).
- Returns a **structured** result, not free prose (see §6).

### 3.3 Intercept in the chat loop — do NOT make it a normal MCP tool
`delegate` must run a *nested LLM tool-loop*, which the MCP `ToolHandler` layer
cannot do (it dispatches stateless tool calls; it has no LLM). Two options:

- **(A, recommended) Intercept in the provider loops.** When a loop sees a tool
  call named `delegate`, it does not hand it to `execute_guarded`/`ToolHandler`;
  it calls `ChatService::run_subagent(...)`, which runs a scoped attempt under
  the target profile and returns the structured result as the tool result the
  orchestrator reads next round. Keeps all loop logic (depth, streaming, guards)
  inside `ChatService`, where the fallback ladder and write guard already live.
- **(B) A self-referential Skill** holding an `Arc<ChatService>`. Cleaner
  surface, but introduces a circular Arc (`ChatService` → skill → `ChatService`)
  and scatters loop concerns across two places. Rejected unless (A) proves
  awkward.

`delegate` is surfaced to the orchestrator like any tool (name + schema) so the
model calls it naturally, but the dispatch is special-cased. This mirrors how
`execute_guarded` already special-cases writes before dispatch.

### 3.4 `run_subagent` — the nested attempt
A new `ChatService::run_subagent(profile, task, depth, tx)`:
1. Build the sub-agent's tool allowlist + system prompt from its profile
   (reuse `build_bundle`), and its model from `resolve_profile_model`.
2. Run a **bounded** provider loop (reuse the existing loops) with a fresh
   `TurnWriteGuard` and a tight `MAX_SUBAGENT_ITERATIONS` (smaller than the
   top-level cap).
3. Collect gathered tool results (the same `gathered` structure `run_synthesis`
   already builds) and the sub-agent's final text.
4. Return `{ answer, sources, gaps }` to the caller.

The nested loop reuses the fallback ladder and all guards for free.

---

## 4. Model roles — this generalises the worker/voice split

| Role | Model (proposed) | Job |
|------|------------------|-----|
| Orchestrator / voice | gemma4:31b-cloud (conversational) | Lead the dialogue; decide when to `delegate`; compose the reply from returned findings |
| Worker sub-agent | gpt-oss:120b-cloud (tool caller) | Run the scoped tool loop for one lookup |

The **current worker/voice split is the degenerate case** of this: one implicit
sub-agent (gpt-oss) that runs once at the start, then the voice model speaks.
The generalisation lets the voice model call the worker *zero or many* times,
*when* it needs to, instead of always-once-upfront.

Sub-agents run on **gpt-oss (unmetered)**, never Gemini — a sub-agent is many
model calls, the exact shape that trips Gemini's 5 RPM. (Phase 4 can re-express
`run_synthesis` on top of `run_subagent` so there is one mechanism, not two.)

---

## 5. Orchestrator awareness (the roster)
For the orchestrator to delegate, it must know which agents exist and what each
is for. Inject a **roster** into the orchestrator's system prompt: for every
`spawnable` profile, one line `name — specialty (tools: n)`. Sourced from the
same `ContextBuilderService` the chat loop already reads; it is live, so a new
profile becomes delegable without a code change — the same property that makes
profiles the right unit for this.

---

## 6. Result shape & epistemics
A sub-agent returns **structured findings, not prose the voice model relays
verbatim**:

```json
{ "answer": "...", "sources": [{"title","url"}], "gaps": ["..."] }
```

This is required to keep the epistemics rules intact:
- The voice model must cite `sources` (the CITATION RULE already expects links).
- `gaps` are the sub-agent's declared limits; the voice model must relay them,
  not paper over them (the TOOL-RESULT RULE and the `reason` limits marker exist
  for exactly this).
- A `delegate` result is a **retrieval event**: it should flip the caller's
  `TurnWriteGuard.retrieval_ran` so a subsequent `semantic` note the voice model
  writes is legitimately grounded. Without this, delegating and then writing up
  the answer would get the note downgraded to `unsourced_synthesis` even though
  the turn *did* consult the world.

Whether the sub-agent's own writes (e.g. `store_note`) are allowed, or whether a
sub-agent is read-only and only the orchestrator persists, is an open decision
(§9).

---

## 7. Guards & invariants to preserve
The chat loop's existing behaviour is load-bearing; the nested loop must respect
all of it:

- **Depth guard.** A sub-agent may not `delegate` (depth cap 1 to start;
  never unbounded). Enforced by not surfacing `delegate` in a sub-agent's tool
  set.
- **Iteration budget.** `MAX_SUBAGENT_ITERATIONS` bounds each sub-agent; the
  top-level `MAX_TOOL_ITERATIONS` still bounds the orchestrator. A turn's total
  model-call budget is the product — bound both, and surface it.
- **Streaming, not silence.** A sub-agent can take seconds. Emit nested events
  (`ToolCall`/`ToolResult`/a "🔎 <agent> working…" `Thinking`) so the user sees
  progress rather than a stall — the dropped-turn detector
  (`forward_chat_events`) must still see activity.
- **Write guard.** Each sub-agent gets its own `TurnWriteGuard`; the
  orchestrator keeps its own. Duplicate-suppression is per-loop.
- **Fallback ladder.** A sub-agent whose model produces nothing falls down the
  ladder like any attempt (nested `run_one_attempt`). A sub-agent that fails
  entirely returns an error result the orchestrator can react to — it does not
  fail the whole turn.
- **Context isolation.** A sub-agent sees its `task` + its profile's
  `pre_load_query`, **not** the full chat history (keeps prompts small and
  bounds cost). The orchestrator is responsible for passing needed context in
  `task`.
- **Working memory / episodic notes.** Decide whether a sub-agent turn is
  recorded (probably: fold its findings into the orchestrator's single episodic
  note, not one note per sub-agent — avoid the note-spam failure mode).

---

## 8. Contrast — when to delegate vs. when to enqueue
| | `delegate` (this proposal) | `create_task` / `construct_agent` / chains |
|-|-|-|
| Timing | Synchronous, in-turn | Asynchronous, background |
| Result | Returns to the turn | Never returns to the turn |
| Use for | A lookup the user is waiting on | Ongoing/recurring work, long jobs |
| Cost | Bounded, interactive | Scheduled, may be large |

`delegate` does **not** replace the async infrastructure; it fills the one gap
it structurally cannot (in-turn results). The chat prompt's ban on `enqueue_jobs`
stays; `delegate` is the sanctioned in-turn alternative.

---

## 9. Open decisions (need a call before building)
1. **Trigger** — explicit only (the orchestrator calls `delegate`) vs. an
   auto-delegate heuristic (a research-shaped message auto-runs a worker). Start
   explicit; it is inspectable and testable.
2. **Result shape** — structured `{answer, sources, gaps}` (recommended) vs.
   prose. Structured protects the epistemics rules.
3. **Sub-agent writes** — may a sub-agent persist (`store_note`, `create_task`),
   or is it read-only with only the orchestrator writing? Read-only is safer to
   start (one writer per turn).
4. **Roster scope** — which profiles are `spawnable`. Start with a curated few
   (`researcher`, `code-analyst`, `knowledge-worker`), not "all".
5. **Parallelism** — one sub-agent at a time (simpler, ordered) vs. concurrent
   fan-out (faster, more quota/VRAM pressure). Start serial.
6. **Model assignment** — orchestrator = gemma, workers = gpt-oss (per §4);
   confirm, and confirm sub-agents never route to Gemini.

---

## 10. Phased plan
- **Phase 0 — spike. [IMPLEMENTED, behind `SUBAGENT_ENABLED`, default OFF]**
  `run_subagent` + `delegate` interception in `run_ollama_cloud_loop`, a curated
  hardcoded roster, depth 1, read-only sub-agents. **Still to do in this phase:
  turn it on in a test deployment and measure added latency and total model
  calls per turn on real questions** (the numbers that decide whether the
  orchestrator should be gemma or gpt-oss, and how tight the iteration caps must
  be).
- **Phase 1 — primitive. [COMPLETE]** `delegate` tool + `run_subagent` (§3.4);
  ✅ tight sub-agent iteration cap (`MAX_SUBAGENT_ITERATIONS`); ✅ structured
  result (§6, `answer`+`sources`); ✅ retrieval-flag propagation into the
  caller's write guard; ✅ interception in all four provider loops via the shared
  `dispatch_tool_call`. Validated end-to-end 2026-09-07 under the `orchestrator`
  profile.
- **Phase 2 — roster & profiles. [COMPLETE]** `spawnable` (default false) +
  `specialty` fields on `ContextProfile`; `ContextBuilderService::spawnable_profiles()`
  + `format_roster` build the `delegate` roster live from them; `run_subagent`
  validates against `spawnable` (not a hardcoded list), and the tool is only
  offered when ≥1 worker is spawnable. `researcher`/`knowledge-worker`/`code-analyst`
  carry `spawnable: true` + a specialty; `general`/`orchestrator` stay false
  (the safety default). Synced to the `:ContextProfile` meta-graph. A new worker
  now joins the roster by editing YAML, no code change. Guards:
  `profiles_are_not_spawnable_by_default`, `format_roster_uses_specialty_then_falls_back_to_description`.
- **Phase 3 — streaming & polish.** Nested event surfacing, episodic-note
  folding, error-result handling, `MCP_TOOL_PROFILE`-style bound on the
  orchestrator's own tool surface.
- **Phase 4 — unify.** Re-express `run_synthesis` (worker/voice split) as a
  `delegate`-backed special case so there is one delegation mechanism, and
  retire the parallel code path.

---

## 11. Risks
- **Latency.** A delegated turn is orchestrator calls + a full sub-agent loop.
  Serial + bounded iterations + streaming keep it tolerable; measure in Phase 0.
- **Recursion / runaway.** Depth cap and no `delegate` in sub-agent toolsets are
  the hard guards; a turn's worst-case model calls must be a bounded product,
  and surfaced.
- **Context bloat.** Passing history into every sub-agent would balloon prompts
  and cost; the isolation rule (§7) is deliberate.
- **Guard interactions.** The nested loop must reuse `execute_guarded`, the
  fallback ladder, and dropped-turn detection — re-implementing any of them in
  the sub-agent path is how this codebase has historically grown a
  "labelled-in-one-path-not-the-other" bug. One mechanism, reused.
- **Quota.** Sub-agents on gpt-oss (unmetered) are safe; a sub-agent on a
  metered/rate-limited model re-creates the Gemini 5-RPM failure. Enforce model
  roles.
