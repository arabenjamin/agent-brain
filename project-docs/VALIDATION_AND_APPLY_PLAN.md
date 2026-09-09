# Deterministic Validation Engine + AiDevBoard Apply Integration

Status: **design + phased build** (2026-09-09). Two related tracks from one session:
a schema/validation engine that pushes back on the LLM deterministically, and the
AiDevBoard agentic apply integration — which is itself the reference model for the
"the model cannot do the stupid thing" pattern the validation engine generalizes.

---

## Track 1 — Deterministic validation engine

### Problem

Objects the LLM (chat or a background loop) can write into the graph —
`ScheduledTask`, `Task`, `SchedulerChain`, agent (`ContextProfile`), and each
step's tool `arguments` — are only shape-checked today. `manage_scheduled_task`
upsert does `serde_json::from_str::<Vec<ChainStep>>` (envelope shape only) then
`lint_chain_steps` (semantic **warnings**, never rejects — it saves anyway and
tells the model "SAVED BUT LIKELY BROKEN"). `ChainStep.arguments` is a free
`serde_json::Value`, so wrong-typed tool args pass straight through.

The motivating failure: the **AI Dev Board Job Search** schedule was written with
`query_params: "[object Object]"` (a stringified JS object), `timeout_ms: "15000"`
(string), `pick` as a comma-string — the HBI editor serialized structured args to
strings. `http_request`'s strict `Input` struct rejected them at *runtime*, so the
step errored every run and the chain silently died. Nothing caught it at write time.

### Key lever: tools already ship their own JSON Schema

Every registered tool has an `input_schema` (`ToolDefinition.input_schema`, what
`tools/list` returns). `http_request`'s already declares `query_params: {type:
object}`, `timeout_ms: {type: integer}`, `pick: {type: array}`, `required:
[method, url]`. A JSON-Schema validator run against it **rejects the exact
corruption above**, and it generalizes to every tool with **zero schema drift** —
the tool definition is the authority, not a hand-maintained copy.

### Design — two layers

**Layer 1 · Structural (JSON Schema, HARD-REJECT).** New dep `jsonschema`. New
module `crates/app/src/services/validation.rs` exposing a `Validator` that:
- validates the **object envelope** against a schema in a new `schemas/` seed dir
  (`schemas/scheduled_task.json`, `task.json`, `scheduler_chain.json`,
  `context_profile.json`, …) — declarative, editable without a rebuild, consistent
  with `chains/` `schedules/` `contexts/` `sources/` (and the no-hardcoded-data
  rule). `SCHEMAS_DIR` compose env, non-fatal if missing (degrade to layer-2 only).
- validates **each step's `arguments`** against the live registry's
  `input_schema` for `step.tool_name`. This is the corruption-catcher.
- returns `Vec<Violation { severity: Hard | Soft, path, message }>`.

**Layer 2 · Semantic (rule lint, SOFT warnings — keep as is).** `lint_chain_steps`
rules (`reason` needs `context`; `provider_hint` top-level & present; persisting
step references `{{_prev}}`; `store_note` `source_context` + known `note_type`;
`semantic` in a web-searching chain; chain never persists) stay **warnings** —
they are correctness advice JSON Schema can't express. One rule graduates to HARD:
**unregistered tool** (a step naming no live tool fails every run — reject it).

**Enforcement points (write boundary):**
- `manage_scheduled_task` upsert  ← first, the proven pain point
- `manage_chain` define
- `create_task`
- `construct_agent`
- YAML seeders (`schedule_seeder`, `chain_seeder`, context loader) — a HARD
  violation logs loud + skips that node rather than aborting startup (a bad
  checked-in file shouldn't wedge the boot; a missing `schedules/` dir stays fatal).

Hard violation → `ToolCallResult::error(...)`, nothing written. Soft → existing
`step_warnings` / `needs_fix` response unchanged.

### Why this is the "classic automated pushback"

The gate is deterministic and runs before persistence, so the model cannot save a
structurally invalid object no matter how confidently it narrates success — the
same principle AiDevBoard enforces for applications (below).

### Phases
- **P1** dep + `validation.rs` + `schemas/scheduled_task.json` + per-step
  arg-vs-`input_schema` check + wire HARD-reject into `manage_scheduled_task`
  upsert; unit tests incl. a regression test reproducing the `query_params`
  string corruption. ← start here.
- **P2** `scheduler_chain.json` + `manage_chain`; `task.json` + `create_task`.
- **P3** `context_profile.json` (agents) + `construct_agent`; seeder enforcement.
- **P4** optional: tighten individual tool `input_schema`s
  (`additionalProperties`, string value types on `query_params`) so structural
  validation is even sharper.

---

## Track 2 — AiDevBoard agentic apply (full MCP-client loop)

### What exists (verified live 2026-09-09)

`AIDEVEBOARD_KEY` in `.env` is an **email-verified** free developer key (100/hr).
Spec `https://aidevboard.com/openapi.yaml`. A full **MCP server at
`https://aidevboard.com/mcp`** (JSON-RPC, `X-API-Key`) exposes the apply toolchain;
the REST `/candidate/resume-preview` is transient-only, so the **persisted/approved/
submitted** path is MCP-only → the brain must be an MCP **client** to AiDevBoard.

6-stage gated flow (all $0 to candidate/agent):
1. `match_jobs {skills[],workplace,level,salary_*}` — server-side ranked matches
   (`match_score`, `matched_tags`, `match_reasons`). Recommended agentic search.
2. `get_job {id}` — full JD.
3. `analyze_application_readiness {job_id,candidate_skills,evidence_bullets}` —
   deterministic gap analysis: `matched_signals` vs `missing_signals`,
   `coverage_ratio`; NO acceptance probability; `improvement_loop.agent_rule` =
   "never invent/infer/embellish; recompile only with human-confirmed evidence."
4. `compile_job_specific_resume {job_id,evidence_bullets}` — with the verified key
   PERSISTS an immutable artifact: `artifact_id`, `artifact_hash` (sha256),
   `artifact_status: draft`, 7-day expiry, `artifact_markdown`. **Rejects hidden
   text / screening-model instructions / unsupported claims.** Returns
   `candidate_review.must_pause: true`.
5. `approve_compiled_application {artifact_id,artifact_hash,principal_authorized}`
   — single-use, short-lived authorization bound to exact hash + verified
   candidate + unchanged job.
6. `apply_to_job {job_id,principal_authorized,compiled_artifact_id,
   compiled_artifact_hash,compiled_authorization_id}` — submits ONLY the exact
   approved hash; consumes the authorization. Then
   `record_my_application_outcome` / `list_my_interview_outcomes`.

The apply path is content-addressed and human-gated at the protocol level: the
model cannot submit anything the human didn't approve by exact hash.

### Design
- **Outbound MCP client** (`services/mcp_client.rs` or reuse an HTTP JSON-RPC
  helper) to `aidevboard.com/mcp`; auth via a seeded `aidevboard` `ApiContext`
  (injects `X-API-Key`, env `AIDEVEBOARD_KEY`).
- **Skill** exposing the toolchain as brain tools (thin wrappers with the
  human-in-the-loop preserved): search/analyze/compile are autonomous-safe;
  compile → `notify_user` with `artifact_markdown` **verbatim** + the approval
  question + `create_task("Apply: <title> @ <company>")`, then STOP. approve +
  submit run **only** on Ara's explicit in-chat approval. Brain never auto-submits.
- **Evidence** comes from the résumé notes (`source_context: "resume"`); the
  brain SELECTS relevant bullets per job (grounded selection, never invention) and
  uses `analyze_application_readiness` `missing_signals` to decide which further
  résumé bullets to pull before recompiling.
- **Rewire** the daily **AI Dev Board Job Search** schedule from
  `http_request` + `execute_code` filter to `match_jobs` (server-side ranking with
  Ara's skill vector).
- **Rewrite** the JOB APPLICATION RULE in `contexts/general.yaml` around this real
  toolchain (it predates the discovery and assumes apply is not API-reachable).

### Guardrails / conventions
- Never claim an application was submitted unless `apply_to_job` returned success.
- Persisted compile is a benign draft (expires); approve/apply are the
  outward-facing, human-gated steps — treat like any hard-to-reverse action.
- One real walkthrough draft exists: `adb_art_S3knUSlXE1kFodZm` (Harvey AI Staff
  SWE Model Infra), expires 2026-09-16, unsubmitted.
