# Unified Agent Execution — Design Proposal

Status (2026-09-07): **Phases 0–3 COMPLETE and live-smoke-tested; Phases 4–5
REMAINING (both were always optional/deferred).** The core is delivered — one
shared `AgentRunner` primitive reachable from all three goal entrypoints (chat,
tasks, schedules) with choose-or-build selection. What remains is the durable
iterative-project loop (Phase 4, tentative — §8 decision 3) and automatic
proven-agent preference (Phase 5, optional). Per-phase detail is in §9.

- ✅ **Phase 0** — extract `AgentRunner` (behaviour-neutral).
- ✅ **Phase 1** — `run_agent` background tool.
- ✅ **Phase 2** — choose-or-build (`select_agent`, `agent:"auto"`).
- ✅ **Phase 3** — three front doors (`chains/run-agent.yaml`, `schedule-author`).
- ⬜ **Phase 4** — iterative `:Project` build↔review loop (tentative; decide vs a
  lighter evaluator-loop tuning).
- ⬜ **Phase 5** — automatic proven-agent preference (optional).

Gated behind `SUBAGENT_ENABLED` (default OFF). Smoke-tested functionally
(routing + a live end-to-end `run_agent`); **not** yet load-, cost-, or
answer-quality-tested.

Read first: `project-docs/SUBAGENT_ORCHESTRATION_PLAN.md` (the chat-side
`delegate`/`run_subagent` primitive this generalises), the Externalized Agent
Chains / ScheduledTask ownership / Evaluator Loop / Adversarial Critic sections of
`CLAUDE.md`, and `crates/app/src/skills/constructor.rs`. This design is entirely
constrained by hard-won behaviour in those.

---

## 1. The problem: one goal, three execution machines

The brain has **three front doors** that all mean "accomplish a goal," but each
runs through a different machine, and only one of them has a real *agent*:

| Entrypoint | How a "step" runs today | Agent identity? | Evaluator / critic? |
|---|---|---|---|
| **Chat** (`clients/chat.rs`) | orchestrator → `delegate` → `run_subagent`: a **profile-driven bounded LLM tool loop** | **Yes** — the sub-agent *is* a `ContextProfile` (model + toolset + system prompt) | worker/voice split, write guard, grounding |
| **Task / todo** (`scheduler.rs::goal_to_steps` → chain) | each step is **one fixed tool call**; `provider_hint`/`required_capabilities` pick a **model** | **No** | yes (generator-evaluator + adversarial critic) |
| **Scheduled task** (`schedules/*.yaml`) | steps enqueued **verbatim** | **No** | **No** — no routing, no evaluator, no retrieval |

The load-bearing fact (documented in `scheduler.rs:344`):

> for background jobs [the context profile] is **ANNOTATION ONLY — the
> coordinator never reads `job.context_profile` at execution** … applies no
> profile tool-filter, system_prompt, or pre_load_query. Profiles only take
> effect on /chat turns.

So on the background path there is **no agent** — a step is a single stateless
tool invocation, and the only per-step intelligence is *which model runs it*. The
chat path got the good machinery (`run_subagent`: a profile that decides its own
steps, calls tools, returns structured findings). Tasks and schedules never did.

Every capability the user asked for is a consequence of closing that one gap:

- *"apply an actual agent to each step, or one agent to oversee the task"* → a
  step (or a whole task) that runs a **profile-driven agent loop**, not a fixed tool.
- *"choose an agent, or build one from the available models and tools"* → the
  roster (`spawnable` profiles) for *choose*, `construct_agent` for *build*.
- *"create a scheduled task from chat, but it came out malformed"* → let the
  orchestrator delegate schedule-authoring to a specialist agent whose job is a
  lint-clean chain (same pathway, different worker).
- *"pass a project back and forth until it meets the definition of done"* → the
  evaluator loop, generalised with **artifact continuity** and a durable node.
- *"the pathways from orchestrator to worker should be the same coming from
  todos, chat, or schedules"* → **one primitive, three front doors.**

---

## 2. What already exists (build on it; do not rebuild)

- **`run_subagent` (chat).** `ChatService::run_subagent(agent, task)` builds a
  sub-agent's toolset + system prompt + model from a `ContextProfile`, runs a
  **bounded** provider loop (`MAX_SUBAGENT_ITERATIONS`), and returns structured
  findings (`answer` + `sources`). Read-only in chat; depth cap 1; grounded; all
  four provider loops route delegate interception through `dispatch_tool_call`.
  **This is the agent loop we want everywhere — it just lives inside `ChatService`
  and streams to a chat channel.**
- **Agents are profiles.** `ContextProfile` already carries `tools`,
  `system_prompt`, `pre_load_query`, `model_preference`/`provider_hint`, and
  (from the sub-agent work) `spawnable`/`specialty`. `spawnable_profiles()` +
  `format_roster()` already produce a live roster. No new "agent" concept needed.
- **`construct_agent` (build).** Plans a validated chain from the live tool
  inventory, persists an `(:AgentSpec)`, optionally dispatches it as a Task with a
  mandatory adversarial pre-flight + evaluator. **Reuse-before-construct** already
  prefers a proven spec (avg `PERFORMED` score ≥ 3.5, goal Jaccard ≥ 0.5). This is
  the *build* half of choose-or-build — currently underused.
- **Evaluator + adversarial loops.** A Task with `success_criteria` already gets a
  prepended adversarial review and an appended `reflect_on_work` evaluator; a
  sub-threshold score marks the task failed and **creates a new Task carrying the
  critique** (retry cap 3). This is build→review→retry — but it **re-plans from
  scratch each requeue** and has no artifact.
- **Workspace.** `write_workspace_file` + `list_workspace_files` exist.
  **`read_workspace_file` does not** — a gap the project loop must fill (a reviewer
  can't grade an artifact it can't read).
- **`ChainStep.context_profile`** field already exists on every step — today
  written for annotation and never read at execution. **The plan gives it meaning.**

---

## 3. Core proposal — one primitive: `AgentRunner`

Extract the sub-agent loop out of `ChatService` into a standalone service both
paths can call:

```
services/agent_runner.rs
  AgentRunner {
    llm, tool_handler, context_builder, telemetry
  }
  async fn run(profile: &str, task: &str, budget: AgentBudget,
               sink: Option<EventSink>) -> AgentFindings
```

- **`profile`** — a `spawnable` `ContextProfile` (the agent).
- **`task`** — a self-contained sub-goal (caller supplies needed context;
  the agent does not see chat history / other steps).
- **`budget`** — iteration cap, write-allowed flag, provider/tier policy, depth.
- **`sink`** — `Some(stream)` on the chat path (nested events forwarded to the
  user); `None` on the background path (findings collected, not streamed).
- **Returns** `AgentFindings { answer, sources, gaps, retrieved, artifacts }` —
  the same structured shape `run_subagent` already produces.

It **reuses** the existing loop machinery (fallback ladder, `execute_guarded` +
`TurnWriteGuard`, depth guard by stripping the agent-run tool from the inner
toolset). This is the one mechanism the whole codebase's history says to reuse
rather than re-implement per path.

`ChatService::run_subagent` becomes a **thin wrapper** over `AgentRunner::run`
with a streaming sink. This is the *correct* version of the deferred
SUBAGENT_ORCHESTRATION Phase 4: not "swap which model leads the default chat"
(rejected — a behaviour change dressed as a refactor), but "extract the shared
agent-loop core so background can reach it too" (pure structure, chat behaviour
unchanged).

### 3.1 Two ways to reach it

1. **`run_agent(agent, task)` — a real tool** (new `AgentRunnerSkill`). Because a
   background chain step already flows through `ToolHandler` and its result
   becomes `{{_prev}}` for the next step, `run_agent` slots in as a step
   `tool_name` with **no coordinator interception** — unlike chat's `delegate`,
   which must be intercepted because its result re-enters the same live turn. A
   step `{tool_name: "run_agent", arguments: {agent, task}}` *is* "an agent for
   this step"; a one-step chain of it *is* "one agent overseeing the task."
2. **`delegate` (chat)** stays intercepted in the provider loops (its result
   must fold into the ongoing turn), but now calls `AgentRunner::run` under the
   hood instead of a private `ChatService` method.

**Depth / cost guard:** the inner agent's toolset has `run_agent` (and `delegate`)
stripped — depth cap 1, same as chat. A turn/chain's worst-case model-call budget
stays a bounded product (outer iterations × inner `AgentBudget`), and is surfaced.

### 3.2 Writes: background agents write, chat sub-agents don't

In chat a sub-agent is read-only (one writer per turn). In the background, writing
*is* the job (`store_note`, `write_workspace_file`). So `AgentBudget.allow_writes`
is `true` on the background path — but the inner agent runs its own
`TurnWriteGuard` so grounding/duplicate rules still hold.

---

## 4. Choose-or-build agent selection

Two existing mechanisms, unified into one decision the orchestrator (or a routing
step) makes:

- **Choose** — pick a `spawnable` profile from the live roster whose `specialty`
  fits the sub-goal, and `run_agent` it. Cheap, curated, inspectable.
- **Build** — when no profile fits, `construct_agent` plans a bespoke validated
  chain (and reuse-before-construct means a proven bespoke agent is preferred
  automatically next time).

Prompt policy: **prefer the roster; construct only when nothing fits.** Optionally
a small `select_agent(goal) -> profile | none` helper (specialty match, same
`format_roster` source) so a chain step can route without an LLM call. The two
notions of "agent" stay distinct and both keep their place:

| | **profile-agent** (`run_agent`) | **constructed-agent** (`construct_agent`) |
|-|-|-|
| Is | model + toolset + prompt, decides its own steps | a fixed validated chain (a plan) |
| Runs | bounded loop, in-line, returns findings | through the queue, async, graded (`PERFORMED`) |
| Use for | an open-ended sub-goal | a repeatable procedure worth persisting + scoring |

---

## 5. One pathway, three front doors

With `AgentRunner` + `run_agent` in place, all three entrypoints reach the same
agent loop:

- **Chat** — `delegate` → `AgentRunner::run` (streaming). *(already works; now
  shares the core.)*
- **Task / todo** — `goal_to_steps()` may emit a `run_agent` step, or route the
  whole task to a single overseeing `run_agent` (the "one agent per task" grain),
  or `construct_agent` builds a chain. The default grain is **task-level**: the
  step is cheap deterministic tools, and `run_agent` is reached for the genuinely
  open-ended part — not every step becomes an autonomous loop (that would multiply
  cost on fan-outs; see §7).
- **Scheduled task** — a schedule step can be `run_agent`, so schedules finally
  get real agents instead of verbatim fixed tools. `lint_chain_steps` gains a rule:
  a `run_agent` step must name a `spawnable` agent.

The orchestrator's ability to *create a well-formed schedule* becomes: delegate to
a schedule-authoring agent (a profile whose specialty is emitting a lint-clean
chain), then `manage_scheduled_task(upsert)`.

---

## 6. The iterative-project loop (the widget example)

A durable primitive layered on `run_agent`, generalising the evaluator loop with
the two things it lacks: **artifact continuity** and a **project-owned iteration
budget** decoupled from the evaluator's retry cap of 3.

- **`(:Project {id, spec, definition_of_done, workspace_path, status, iteration,
  max_iterations, created_at})`** — the durable record.
- **Loop (one iteration):**
  1. **Builder** `run_agent` (a `developer`/builder profile) — input: spec +
     current artifact (read from `workspace_path`) + prior reviewer critique;
     edits the artifact in place. *Edits, does not rebuild.*
  2. **Reviewer** `run_agent` (a distinct reviewer profile) — reads the artifact,
     grades it against `definition_of_done`, emits `Score: N/5` + a critique
     (reuses the evaluator's parse).
  3. **Gate** — pass (≥ threshold) → mark project `done`; fail → increment
     `iteration`, requeue the builder **with the critique and the artifact carried
     forward**, until `max_iterations`.
- **New tool** `manage_project(action: create|status|advance|cancel)` +
  **`read_workspace_file`** (the missing workspace read).

Differences from today's evaluator loop that make this a real primitive, not just
tuning: builder and reviewer are **different agents** (separation of concerns,
like the adversarial critic); the artifact is **carried forward** (not re-derived
each round); the loop is owned by a `:Project` with its **own iteration counter**
and DoD, not the 3-retry evaluator cap.

---

## 7. Guards, invariants, and the honest tensions

- **Not everything should become an agent.** A step like "store this note with
  `source_context: X`" must stay a cheap deterministic tool. `run_agent` is the
  escape hatch for the open-ended step (research, build, review), not the default
  wrapper for every step. The default grain is one agent per *task*, not per *step*.
- **Cost is a multiplication on fan-outs.** Background is local-pinned for a
  reason. A `run_agent` inner loop that touches cloud (tier 2) multiplies once per
  invocation — putting it on `poll_media_sources`' ~30-video fan-out without a
  burst cap re-creates the quota-blowout the codebase already warns about. Rules:
  `run_agent` is task-level not fan-out-item-level; inner budget bounded; inner
  agent respects `provider_hint`/`CLOUD_TIER`; background default stays local
  unless the sub-goal opts into a cloud capability. Bound burst and daily total
  separately (the existing ScheduledTask cost rule).
- **Depth cap 1.** Inner agents cannot `run_agent`/`delegate` (strip from inner
  toolset). Worst-case model calls stay a bounded, surfaced product.
- **Reuse one mechanism.** `AgentRunner` reuses the fallback ladder, write guard,
  and dropped-event detection. Re-implementing any of them in a second path is
  exactly how this codebase grows "labelled-in-one-path-not-the-other" bugs.
- **Provider policy (decided): local-first, Ollama Cloud allowed, Gemini never.**
  A background `run_agent` defaults to the **local** model (`OLLAMA_LOCAL_MODEL`) —
  unmetered, private, and matching the existing background pinning. It may step up
  to **Ollama Cloud** (`gpt-oss:120b-cloud` — also unmetered) when the sub-goal
  needs stronger tool-driving, opted in per profile/step, never as the silent
  default. It **never** routes to Gemini or any rate-limited/metered model — an
  agent is many model calls, the exact shape that trips the 5-RPM ceiling.
  **Caveat to measure in Phase 1:** chat sub-agents run on gpt-oss *because* the
  local model is a weaker tool-caller (and gemma empty-completes ~1-in-4). So
  "get away with local" is a hypothesis, not a given — Phase 1 must measure
  whether the local model can actually drive a bounded agent loop (tool-call
  reliability, `OLLAMA_LOCAL_TIMEOUT_SECS` burst behaviour) before local-default
  is locked in. If it can't, the fallback is Ollama Cloud, still unmetered.
- **Feature-flagged, default OFF.** Ship behind a flag (`AGENT_RUNNER_ENABLED` /
  reuse `SUBAGENT_ENABLED`) like the chat primitive; measure before defaulting on.

---

## 8. Decisions

**Settled (2026-09-07):**
1. **Default grain — task-level.** One agent oversees a *task*; per-step
   `run_agent` is the exception, not the wrapper for every step (§7).
2. **Provider policy — local-first, Ollama Cloud allowed, Gemini never** (§7).
   Background agents default to the local model; may opt into Ollama Cloud
   (unmetered) for harder tool-driving; never a rate-limited/metered model.
   *Phase 1 measures whether local can actually drive an agent loop before
   local-default is locked in.*

**Still open:**
3. **`:Project` vs "a Task that never gives up"** — a real new node (§6) vs tuning
   the evaluator loop with artifact continuity. The node is the honest primitive
   for iterative builds; the tuning gets ~80% for the "keep trying" case.
   **Leaning: defer.** Sequenced as Phase 4 (tentative) either way, only after the
   shared core (Phases 0–3) exists — decide when we get there.
4. **Who may write** — background `run_agent` writes (§3.2); confirm the inner
   write guard is sufficient vs a one-writer-per-chain rule.
5. **Selection** — LLM-chosen agent (orchestrator prompt) vs a deterministic
   `select_agent(goal)` specialty-match helper vs both. Start with the prompt;
   add the helper if routing needs to be LLM-free.
6. **Naming** — `run_agent` (proposed) vs reusing `delegate` for both paths. Keep
   them distinct: `delegate` is intercepted + streams into a live turn; `run_agent`
   is a normal tool whose result flows via `{{_prev}}`.

---

## 9. Phased plan

- **Phase 0 — Extract `AgentRunner` (no behaviour change). ✅ DONE (2026-09-07).**
  Lifted the sub-agent loop out of `ChatService::run_subagent` into
  `services/agent_runner.rs` (`AgentRunner::run(agent, task, budget, sink)`,
  returning structured `AgentFindings`); `run_subagent` is now a thin streaming
  wrapper over it. The four provider loops and catalog-aware model resolution stay
  on `ChatService` — the delicate, incident-laden machinery — and `AgentRunner`
  reaches them through the `SubagentLoopRunner` trait (`ChatService` implements it
  by dispatching to its existing loops unchanged). The ownership cycle
  (`ChatService` owns `Arc<AgentRunner>`; `AgentRunner` holds
  `Weak<dyn SubagentLoopRunner>`) is broken with a `Weak`, wired via
  `Arc::new_cyclic` in the shared `ChatService::build` constructor. `AgentBudget`
  carries `max_iterations` + `allow_writes` (chat passes `false`, read-only, one
  writer per turn; the background path will pass `true`). Behaviour proven
  unchanged: all 437 lib tests green, `cargo fmt`/`clippy` clean, sub-agent guard
  tests (`profiles_are_not_spawnable_by_default`,
  `subagent_budget_is_tighter_than_the_orchestrator`, roster/findings tests)
  still pass. This is the load-bearing refactor — everything else depends on it,
  and it is the *correct* form of the deferred SUBAGENT Phase 4 (share the core,
  don't swap the lead).
  - **Known follow-up (a later phase, not Phase 0):** `agent_runner.rs` imports
    `ChatEvent`/`ChatRequest` from `clients::chat` — a `services`→`clients` bend
    (legal, one crate). When the background path lands, rename/relocate these to
    a neutral `AgentEvent`/`AgentTask` so the engine's I/O types don't read as
    "chat". Left as-is here to keep Phase 0 a pure, reviewable refactor.
- **Phase 1 — `run_agent` background tool. ✅ DONE (2026-09-07).**
  `skills/agent_runner.rs` (`AgentRunnerSkill`) exposes `run_agent(agent, task)`:
  it calls `AgentRunner::run` with **no sink** and `allow_writes: true` (writing
  is the background job; the inner agent keeps its own `TurnWriteGuard`), a
  bounded loop (`MAX_BACKGROUND_AGENT_ITERATIONS = 8`), depth 1 (the inner
  toolset never contains `run_agent`/`delegate`). The tool result carries the
  findings under `answer` (so it flows through `{{_prev}}`) plus `ok`/`retrieved`/
  `agent`. Registered to **both** `tool_registry` and the `skills` vec, **gated on
  `SUBAGENT_ENABLED`** (default OFF, plan §7). `lint_chain_steps` gains a rule: a
  `run_agent` step must have non-empty `agent` and `task` (spawnable-membership is
  enforced live by `run_agent` itself, which returns the roster for a bad name —
  the scheduler lint has no roster).
  - **Wiring — where the engine comes from.** The loop engine is `ChatService`
    (Phase 0). So `AgentRunner` needs a live chat service. `BrainCore` gained a
    lazy `agent_runner: Arc<RwLock<Option<Arc<AgentRunner>>>>` slot; the
    `AgentRunnerSkill` (built before the chat service exists) holds that slot and
    reads it per-call. `McpServerCore::chat_service()` — the single factory point,
    called once at startup — publishes `chat.agent_runner()` into the slot. On a
    deployment with no chat service (stdio), the slot stays empty and `run_agent`
    reports itself unavailable cleanly. **This makes `run_agent` an HTTP-path
    feature for now** — the honest consequence of the engine living in a client
    adapter; a later phase (or the Phase 0 follow-up) can lift the engine to
    brain-core to make it transport-independent.
  - `AgentFindings` gained a `rejected` flag so a caller can tell a config/infra
    rejection (unknown/non-spawnable agent, empty task, engine gone → hard tool
    error) from a ran-but-empty result (non-fatal step result — same rationale as
    `execute_code` treating a failed run as a successful tool call, so it does not
    dead-letter the owning Task).
  - Guard tests: `run_agent` tool shape; empty agent/task → hard error;
    unavailable runner reports cleanly; lint flags a `run_agent` step missing
    `agent`/`task` and passes a well-formed one. 443 lib tests green,
    `fmt`/`clippy` clean.
- **Phase 2 — choose-or-build. ✅ DONE (2026-09-07).** The "choose" half is now
  deterministic and LLM-free (decision 5: shipped the helper, since background
  routing benefits from not spending a model call to pick an agent):
  - `AgentRunner::select_agent(goal) -> AgentSelection { agent, score, roster }`
    scores each spawnable profile by token overlap between the goal and its
    `name + specialty` (tiny stopword filter, min length 3), returns the best
    above `MIN_SELECTION_OVERLAP = 1`, else `agent: None` — the cue to build. The
    full roster is always returned.
  - `select_agent(goal)` **tool** exposes it (result's `answer` is the chosen name
    or `"none"`, so a `{{_prev}}` hands the routing decision to the next step).
  - `run_agent` accepts **`agent: "auto"`** — auto-selects, and on no match
    returns a rejection naming the roster (the "build" cue). One-call choose.
  - **Prompt policy** (prefer-roster-then-construct): the `run_agent` and
    `select_agent` tool descriptions state "prefer an existing agent; construct
    only when none fit." The *build* half is the existing `construct_agent`
    (already reuse-before-construct), unchanged.
  - Tests: `tokenize` filtering; overlap discriminates a research goal toward the
    researcher; the skill exposes both tools. 445 lib tests green, fmt/clippy
    clean. *Deferred:* wiring `select_agent`/`run_agent` into `goal_to_steps`
    routing chains is Phase 3 (three front doors).
- **Phase 3 — three front doors. ✅ DONE (2026-09-07).** All three entrypoints
  now reach the shared loop:
  - **Task / todo.** `chains/run-agent.yaml` — a Task whose goal is prefixed
    `run agent:` / `delegate to agent:` routes (priority 20) to a single
    `run_agent(agent: "auto", task: {{goal_topic}})` step, then persists the
    findings and completes the task. `no_evaluator`/`no_adversarial` (one agent
    owns the grain; the iterative critic loop is Phase 4). This is "one agent per
    task" via the externalized-chain mechanism — `goal_to_steps` emits it by
    pattern, no code change.
  - **Scheduled task.** Any schedule step can now be `run_agent` (the Phase-1
    tool + lint rule already support it). No live schedule ships using it (it
    would error while `SUBAGENT_ENABLED` is off); documented for the user to add
    once enabled.
  - **Chat.** Already reaches the loop via `delegate` (Phase 0). Unchanged.
  - **Schedule-authoring delegation flow** (the user's malformed-schedule pain):
    `contexts/schedule-author.yaml` — a **spawnable** drafting specialist whose
    system prompt encodes every `lint_chain_steps` rule and whose job is to
    return a lint-clean `{name, interval_seconds, steps}` draft. Because a
    sub-agent is read-only (one writer per turn), it **drafts**; the orchestrator
    **upserts**. `contexts/orchestrator.yaml` gained `manage_scheduled_task` (the
    one write it performs) and a RECURRING WORK rule: delegate to
    `schedule-author`, then `manage_scheduled_task(upsert)` the draft, and on
    `step_warnings` send them back to be fixed. This closes the gap where the
    gemma orchestrator could not create a schedule at all.
  - **Bug fixed along the way:** `chain_seeder.rs` parsed `no_evaluator` but not
    `no_adversarial`, so the documented YAML field was a silent no-op for every
    seeded chain (`goal_to_steps` reads `c.no_adversarial`; only `manage_chain`
    wrote it). Now seeded correctly.
  - Guard tests: every shipped chain + context profile parses (would have caught
    the missing required `pattern`); run-agent skips both critics; schedule-author
    is spawnable with a specialty. 449 lib tests green, fmt/clippy clean.
- **Phase 4 — ⬜ REMAINING (tentative — decision 3 open) — iterative Project
  loop.** `(:Project)` node, `read_workspace_file`, `manage_project`,
  builder↔reviewer `run_agent` loop with artifact continuity and a DoD gate. The
  widget example, fully realised. Gated on the §8.3 call — may instead be a
  lighter tuning of the existing evaluator loop. Phases 0–3 have landed, so this
  is now the one substantive decision left: build the node vs the lighter tuning
  vs leave it. Recommendation: decide from real usage of Phases 0–3 with the flag
  on, rather than building it speculatively.
- **Phase 5 — ⬜ REMAINING (optional) — automatic proven-agent preference.** Fold
  `construct_agent`'s `PERFORMED` grading into `run_agent` selection so a proven
  agent for a recurring sub-goal is chosen without asking.

---

## 10. Risks

- **Latency / cost multiplication** — bounded budgets, task-level grain, local
  default, measured before default-on (§7).
- **Recursion / runaway** — depth cap 1, agent-run tool stripped from inner sets.
- **Two "agent" notions confusing users** — keep the profile-agent vs
  constructed-agent split explicit (§4); one is a loop, one is a graded plan.
- **Guard divergence** — one `AgentRunner`, reused by both paths; no second
  implementation of the ladder / write guard.
- **Scope creep into the Project loop** — Phases 0–3 stand alone and deliver the
  unified pathway; Phase 4 is a distinct layer, sequenced last on purpose.
