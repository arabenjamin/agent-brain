# Fine-Tuning: Teaching the Brain's Models to Behave

**Status: design only, nothing implemented.** Written 2026-09-12.

The long goal is a brain that can build itself: a small local model that writes
Rust well enough to change this codebase, and a small Gemma that *is* the
orchestrator and the voice instead of being prompted into acting like one. The
near goal is experience. Run a fine-tune end to end on something that isn't
brain-specific, and record every result (including the failures) in the graph.

---

> **Before fine-tuning anything, read [`FINE_TUNING_PLAYBOOK.md`](FINE_TUNING_PLAYBOOK.md)**: the
> process and the silent failures learned from Experiment 1.
>
> **Experiment 1 CONCLUDED 2026-09-13, negative:** [`RUST_FINETUNE_EXPERIMENT.md`](RUST_FINETUNE_EXPERIMENT.md).
> No adapter improved gemma4:e4b on Rust. Teacher distillation harmed it through style drift;
> self-only guarded training avoided harm but added nothing. Revisit the tracks below with
> that result in mind: behaviour with a verifier is still the right target, but "a few
> hundred easy examples" is not enough to teach skill to a 4B-effective model.
>
> **First experiment runs outside the brain:** [`RUST_FINETUNE_EXPERIMENT.md`](RUST_FINETUNE_EXPERIMENT.md).
> A QLoRA fine-tune for Rust, trained on compiler-filtered solutions and
> benchmarked on Aider's polyglot Rust exercises before and after, using plain
> Python in `workspace/finetune/`. It builds the `rust-sandbox` the rest of this
> plan needs, and should surface what this plan hasn't considered before any
> of the brain-integrated machinery below gets built.

## The thesis: weights for behaviour, graph for knowledge, prompt for state

| layer | holds | changes how often | how it changes |
|---|---|---|---|
| **weights** (base + LoRA adapter) | *how* to behave: routing policy, tool-call habits, Rust idiom, the brain's voice | per training run (weeks) | train, evaluate, promote |
| **graph** | *what* is known: notes, claims, APIs, media, tasks | continuously | tools write it |
| **prompt** | *what is true right now*: roster, allowlist, date, `LIVE SELF-STATE` | every turn | built by `build_system_prompt` |

The model doesn't need to know everything; the graph does that. Fine-tuning
facts in is the wrong tool: facts go stale, can't be cited, and would get around
the epistemics layer (`label_claims`, claim status). **Do not train knowledge
into weights.**

The failure to design against is a **stale-state adapter**: an orchestrator
trained with the roster baked in keeps routing to agents that were renamed or
removed. Training examples must therefore *include* the dynamic context
(roster, allowlist) as input, so the model learns to read it rather than
memorise it.

### Why this is worth doing: two measured findings point here

1. **The front door has a prompt-size budget.** Same model, same tools, same
   question: at 2332 chars E4B delegates, at 6801 it refuses (CLAUDE.md
   § Context Profiles). The `general.yaml` rules that push it over are narrative
   policy. Context distillation moves them into weights, so the prompt shrinks
   back to state only.
2. **The local-model gap is result-directed iteration** (CLAUDE.md § Going
   local). E4B stops at the first obstacle or loops without progress; the cloud
   twin closes the loop. That is a *behaviour* over tool results, which is
   exactly what trajectory distillation targets.

---

## What exists today, and why it can't produce training data yet

`SleepService::digest_experiences` (`crates/app/src/services/sleep.rs:29`)
writes ChatML JSONL from DuckDB `interactions`. Three defects:

1. **It records tool I/O, not model I/O.** `log_interaction`
   (`crates/app/src/mcp/tools.rs:136`) stores `tool name + args → tool result
   text`. The model's actual prompt, tool calls and completion are never saved.
   `model_usage` has token counts but no text.
2. **`feedback_score` is never written.** Nothing in the codebase sets it, so
   `digest_experiences(min_score: Some(n))` always exports zero rows.
3. **"success" means `is_error.is_none()`.** The brain's worst misses all had
   `isError: false`: "no videos were watched" on an 11-video day, the fabricated
   GDELT probe, "10 tools" against a registry of 90. A dataset filtered on this
   flag would train in exactly the failures this codebase has spent months
   removing.

**The label is the hard part, and the brain already owns most of the graders:**

| grader | kind | exists as |
|---|---|---|
| step args match the tool's `input_schema` | deterministic | `services/validation.rs` |
| schedule is lint-clean | deterministic | `lint_chain_steps` |
| Cypher uses real labels/props | deterministic | `services/cypher_guard.rs` |
| claimed tools actually ran | deterministic | `tool_trace_receipt`, `AgentFindings.tool_trace` |
| an answer contradicts a count we hold | ground truth | `recall_recent`, `list_capabilities`, `list_apis` |
| Rust compiles / lints / passes tests | deterministic | **missing**: needs a Rust sandbox (Track A) |
| work met its success criteria | model-graded | evaluator `Score: N/5` |
| plan is robust | model-graded | adversarial critic |
| local vs cloud on the same task | preference pair | the 2026-09-11 twin head-to-head |
| retrieval quality | golden set | `services/retrieval_eval.rs` |

Prefer deterministic and ground-truth labels. Model-graded labels are for
filtering only, never for rewards, because of the self-grading trap below.

---

## Hardware budget

RTX 3060 Ti, 8 GB. **Serving already uses ~4.8 GB** (llama.cpp E4B + KV, bge-m3,
whisper). Training and serving can't share the card.

- **Local training runs in a maintenance window:** pause the scheduler, stop
  `llamacpp`/`whisper`, unload bge-m3 (`keep_alive: 0`), train, restart. The
  front door is down for the window. Schedule it overnight.
- **QLoRA is the only local option.** Full fine-tuning even a 1B model won't
  fit. Which sizes fit (1.5B / 3B / 4B / 7B at useful sequence lengths) is a
  **Phase 0 measurement**, not an assumption. Rust examples are long, and
  sequence length drives memory as much as parameter count does.
- **Rented GPU is an opt-in escape hatch**, not the default: an hourly
  community GPU for a run that won't fit. Everything in this plan must also
  work on the local card, even if more slowly or at a smaller size.

---

## Shared infrastructure (all tracks use it)

### 1. Capture: record what the model actually did

New DuckDB table `llm_transcripts`, one row per LLM call: `id`, `ts`,
`tool_name` (same attribution as `model_usage`: `CURRENT_TOOL` /
`USAGE_TOOL_LABEL` / `chat`), `model`, `provider`, `messages` (JSON, including
tool calls and tool results), `completion`, `session_id`, `task_id`, `job_id`,
`profile`, `turn_id`. Links to `model_usage` by call id.

- **Provider of origin is recorded on every row** and is load-bearing for
  licensing (below).
- Size-bounded: store over-long tool results as a hash + length rather than the
  full bytes; retention window configurable.
- Capture is **off by default** (`TRANSCRIPT_CAPTURE_ENABLED`) until the size
  cost is measured.

### 2. Label: attach outcomes after the fact

New table `transcript_labels(transcript_id | turn_id, grader, verdict, score,
detail, labelled_at)`. Many graders per row. Labels come from:

- the deterministic graders above, run offline over captured rows
- ground-truth reconciliation (e.g. the answer said zero while `recall_recent` returned 43 → `contradicts_ground_truth`)
- **human feedback: 👍/👎 on HBI chat messages.** This finally writes
  `feedback_score` and is the only source of truth for *voice*

### 3. Curate: the sleep cycle's real job

`digest_experiences` evolves into a dataset builder driven by a declarative
recipe (YAML in a `datasets/recipes/` seed dir, not Rust literals): which
source, which label filter, which chat format, a dedupe key, and a **held-out
split fixed by time** (train on older, evaluate on newer, never shuffled
together). Output: versioned JSONL + a manifest with counts and label
distribution.

### 4. Train: a sidecar, never the brain process

A `trainer` compose service (Python; Unsloth or TRL + PEFT), in the same
spirit as `sandbox`: no credentials, no brain volumes except a read-only
dataset mount and a write-only adapter output mount. The brain **requests** a
run; a human approves it; the trainer runs in the maintenance window. The
output is a LoRA adapter converted to GGUF for llama.cpp.

### 5. Evaluate: gate on the answer, not the loss

Generalise `retrieval_eval` into an eval runner: a suite (golden cases, or a
held-out split, plus graders) × a model (base, base + adapter, cloud twin).
**Every training run has a base-model baseline measured under identical
conditions; a run without a baseline is not a result.** Promotion requires
beating the currently promoted adapter on the held-out suite *and* not
regressing a small general-capability canary suite (forgetting check).

### 6. Serve: adapters on one base

llama.cpp `llama-server` supports LoRA adapters at launch (`--lora`) with
runtime scaling via its adapters endpoint. One E4B base could carry several
narrow adapters without extra VRAM per model. **Phase 0 verifies this works in
our CUDA-12.2 source build (b10900)** and whether a per-request adapter choice
is available or whether adapters are per-instance.

### 7. Record: the experiment ledger lives in the graph

```
(:Dataset {id, recipe, version, n_train, n_eval, created_at, manifest_path})
(:TrainingRun {id, base_model, method, hyperparams, started_at, finished_at,
               status, gpu, wall_secs, peak_vram_mb, notes})
(:Adapter {id, path, format, size_mb, promoted, promoted_at, retired_at})
(:EvalRun {id, suite, model_label, metrics, at})

(:TrainingRun)-[:TRAINED_ON]->(:Dataset)
(:TrainingRun)-[:PRODUCED]->(:Adapter)
(:EvalRun)-[:EVALUATED]->(:Adapter)          // or a base model label
(:Adapter)-[:SUPERSEDES]->(:Adapter)
```

Negative results are first-class: a run that failed to beat baseline is
recorded with its metrics and a `semantic` note saying what was tried and why
it probably didn't help. This is the "gives all of us the experience" half: a
queryable history of what worked on this hardware.

---

## Track 0: a practice run that isn't brain-specific

**Purpose:** exercise capture → curate → train → eval → record end to end on a
task with a public dataset and an automatic metric, before any of it touches
the brain's own behaviour. Pick something small enough to train in under an
hour locally.

| candidate | metric | why | concern |
|---|---|---|---|
| **Text-to-SQL on DuckDB** (recommended) | execution accuracy: run both queries, compare result sets | DuckDB is already in the stack; a checker is a few lines; generally useful; public datasets exist | check each dataset's license before use |
| Structured extraction to a JSON Schema | schema-valid + field exact-match | reuses the `jsonschema` validator; directly relevant to `generate_json` | examples must be generated, so a teacher model is needed |
| Rust compile-error repair | fixed code passes `cargo check` | a warm-up for Track A | needs the Rust sandbox first, so it can't be the first run |

Deliverable: one `(:TrainingRun)` with a baseline, an adapter, an `(:EvalRun)`
on each, and a written note, whether or not it helped.

---

## Track A: a Rust coder

**Goal:** a small code-pretrained model that writes Rust *in this codebase's
idiom*, verified by the compiler, usable by `code-analyst` and eventually a
code-writing agent.

### Start by measuring, not training

CLAUDE.md's standing lesson: **when a local model underperforms, check its
tools before concluding the model is too small.** Before any Rust fine-tune,
Phase 0 measures candidate bases (the `qwen2.5-coder` family already in
`models.yaml`, plus any current small code model that fits) on the repo-commit
eval below, with a real compile loop. The baseline may already be usable with
a compiler in the loop, and the gap may turn out to be tool access, not
weights.

### The verifier: a Rust sandbox

The existing `sandbox` is Python-only and deliberately has no egress. A second
sidecar, `rust-sandbox`, with the same isolation contract (`internal: true`
network, no credentials, `cap_drop: ALL`):

- Rust toolchain baked into the image, **dependencies vendored at build time**
  (`cargo vendor` of this workspace's lockfile), since there's no network at
  run time
- a warm `target/` cache volume **owned by the sandbox**, not shared with the
  brain, because cold builds of a 62K-line workspace are slow
- `mem_limit` sized by measurement; the Python sandbox's 2 GB won't compile
  this workspace
- returns structured `cargo check` / `clippy` / `test` diagnostics, and a
  failing build is a successful tool call (same rule as `execute_code`)

### Data

1. **Eval set from this repo's own history (the most valuable asset).** 237
   commits, most touching `.rs`, each with a detailed message. Case = parent
   tree + commit message (+ the files it touched) → model produces a patch →
   grader applies it and runs `cargo check` plus the tests that commit added or
   changed. This has the shape of SWE-bench, on our own code. **Hold out the
   most recent commits by date.** It is too small to train on alone and exactly
   the right size to evaluate on.
2. **Training data: permissively-licensed public Rust**, license-filtered and
   recorded in the `(:Dataset)` manifest. Filtered by *compiles*, not just
   *parses*.
3. **Rejection sampling on the brain's own tasks.** Generate N patches per task,
   keep those that compile and pass, and train on the survivors. The compiler is
   the reward, with no model grading involved.

### "The brain builds itself" safety contract

Non-negotiable, and enforced structurally rather than by prompt:

- Model-written code lands **only in a git worktree on a `feature/*` branch**.
  Never `dev`, never a push.
- **A human reviews and signs every commit** (GPG, as today). The brain can
  propose a branch; it can't merge or deploy.
- The brain can't promote an adapter trained on data that includes its own
  promotion or eval code, and can't edit the eval suites or the graders it's
  scored by. Eval suites and grader code are human-owned, like
  `eval/retrieval_golden.yaml`.

---

## Track B: Gemma as orchestrator and voice (distilled, not prompted)

**Goal:** E4B that routes, delegates and speaks like the brain with a prompt
that carries only live state.

### B1: routing first (narrow, verifiable)

- **Task:** given the user message, roster and allowlist, emit the right
  `delegate(agent, task)` call, or answer directly.
- **Labels:** a turn is correctly routed when the delegated agent's run answered
  without contradicting ground truth. Known misses are already documented
  (meta-questions sent to `researcher`; "how many agents" matched on a word).
- **Data trick:** vary the roster in training examples (rename, drop and add
  agents) so the model learns to *read* the roster.
- Small, short examples. This is the best first real adapter.

### B2: the task string (preserve intent through delegation)

The front door rewrote *"I want to see all of what we learned"* into a search
query and lost the presentation intent (CLAUDE.md § bulk channel). Pairs:
(user message, roster) → a task string that keeps the imperative. Graded by
whether the worker's result satisfied the original request.

### B3: voice via context distillation

- **Teacher:** a strong model given the *full* policy prompt (the `general.yaml`
  ABSENCE / TOOL-RESULT / CITATION / TIME rules and the style guidance) plus
  the gathered tool results.
- **Student:** E4B given only live state and the same tool results, trained to
  produce the teacher's reply.
- **Human 👍/👎 is required here.** Voice has no deterministic checker.
- **Attribution must survive, and is checked mechanically.** The worker/voice
  split was turned off partly because the voice pass stripped "based on what
  the knowledge-worker found". Any reply that drops a citation or attribution
  present in the tool results is excluded from training.

### What gets lost, and the mitigation

Today a behaviour change is a YAML edit plus a restart. After distillation it is
a retrain. Mitigations: keep a short prompt override layer that always wins;
adapters are versioned and rollback is instant (`(:Adapter)-[:SUPERSEDES]->`);
distil only rules that have been stable for weeks.

---

## Constraints to design around

- **Licensing of teacher outputs.** Only distil from models whose terms permit
  training other models on their outputs. gpt-oss (Apache-2.0) and our own
  local models are fine. **Exclude Gemini and Anthropic outputs** from training
  datasets. Their terms restrict using outputs to build competing models. This
  is why `llm_transcripts` records the provider on every row, and why recipes
  filter on it by default. Verify Gemma's current terms for fine-tuning and
  redistribution of adapters. The Reticulum license issue in
  `BRAIN_TO_BRAIN_PLAN.md` hits this same dataset path.
- **The self-grading trap.** A model graded by itself (or by an adapter of
  itself) and trained on those grades compounds its errors. Rewards come from
  compilers, validators, ground-truth tools, humans, or a *different and
  stronger* model, never the model being trained.
- **Contamination between train and eval.** Split by time. Never generate
  training data from the eval suite's prompts.
- **GPU contention.** Training evicts serving (see Hardware). No run starts
  without a human-approved window.
- **Small data overfits.** Start narrow (B1 routing, Track 0), and check the
  canary suite for forgetting on every promotion.

---

## Phases

- [ ] **Phase 0: Measure (no code).** VRAM + wall time for QLoRA at 1.5B/3B/4B/7B at realistic sequence lengths with serving evicted; whether Gemma-4-E4B is supported by the chosen trainer; llama.cpp LoRA adapter loading in our b10900 build (launch vs per-request); baseline Rust ability of candidate code models on ~20 held-out repo commits with a manual compile loop; front-door routing-miss rate from the ledger.
- [ ] **Phase 1: Capture + ledger.** `llm_transcripts` + `transcript_labels` tables; HBI 👍/👎 writing `feedback_score`; `(:Dataset)/(:TrainingRun)/(:Adapter)/(:EvalRun)` schema; fix or retire `digest_experiences`'s `is_error` success filter.
- [ ] **Phase 2: Track 0 practice run.** `trainer` sidecar, dataset recipe, eval runner, one recorded run with a baseline, win or lose.
- [ ] **Phase 3: Rust sandbox + repo-commit eval suite.** `rust-sandbox` sidecar with a vendored toolchain; the SWE-bench-shaped suite from our history; baseline scores recorded.
- [ ] **Phase 4: Rust adapter.** Public-data SFT, then rejection sampling on brain tasks; promotion gated on the held-out commit suite.
- [ ] **Phase 5: Orchestrator adapters.** B1 routing → B2 task strings → B3 voice, each promoted separately; front-door prompt shrinks as each lands (measure the char count before and after).
- [ ] **Phase 6: The brain proposes its own runs.** A schedule notices a behaviour with enough labelled data and a measured gap, drafts a recipe and a run request, and `notify_user`s for approval. The human approves the run, the eval gate promotes, and the human merges any code.

## Open decisions

1. **Track 0 task:** text-to-SQL on DuckDB (recommended) vs structured extraction.
2. **Trainer stack:** Unsloth (lowest VRAM, model support lags) vs TRL + PEFT (broadest support, more memory). Decide after Phase 0 measures both on the target bases.
3. **Adapter granularity:** one adapter per narrow behaviour (clean rollback, needs per-request selection) vs one merged orchestrator adapter (simpler serving). Depends on what Phase 0 finds about llama-server.
4. **Rented GPU:** allowed at all, and with what monthly cap?
