# Provider Routing: Local, Free and Paid Tiers Behind One Router

**Status: design, implementation starting 2026-09-13.** Supersedes the free-tier-only draft
(`FREE_TIER_ROUTING_PLAN.md`, never committed).

## Goal

Route every LLM call through **one policy-driven router** that treats "local", "free tier" and
"paid API" as properties of a provider, not as special cases in code. Today the brain runs on
local models plus free tiers. Tomorrow a paid key, or better hardware, should be a **config
change**, with no redesign and ideally no Rust change.

Four non-negotiables:

1. **Privacy before budget.** Private work never reaches a provider that may log or train on
   prompts, not even as the last fallback.
2. **Budgets act before the call.** Rate limits, daily quotas and **spend caps** are enforced
   proactively; a 429 is information, not the detection mechanism.
3. **Adding an OpenAI-compatible API is YAML-only.** Mistral, OpenRouter, DeepSeek, Together,
   a self-hosted vLLM, etc. share one generic provider kind.
4. **Paid spend is off until a cap is set.** A paid provider with no cap is unusable, never
   unlimited.

## Where the code is today (2026-09-13)

| concern | where | the free-only / hardcoded assumption |
|---|---|---|
| provider kinds | `services/llm.rs` `LlmProviderType` | 5 variants; each new API is an enum variant, a `Display` arm and a dispatch arm |
| client construction | `LlmClient::with_config` (`llm.rs` ~L428) | per-variant match |
| endpoint + key | `model_router::config_for_catalog_entry` | per-provider env var names hardcoded in Rust |
| tier policy | `model_router::candidate_allowed`, `CLOUD_TIER` 0/1/2 | 4 providers hardcoded; "free" means `cost == 0.0` |
| chat loop dispatch | `clients/chat.rs` `run_one_attempt` | `OllamaCloud \| LlamaCpp` → OpenAI tool loop; anything unlisted → degraded text loop |
| chat provider env | `config.rs` `CHAT_LLM_PROVIDER` parsing | per-name match |
| sub-agent policy | `agent_runner.rs` clamp (~L242) | allow-list of provider *types* (Ollama/OllamaCloud/LlamaCpp) |
| fallback chain | `shared_llm::build_fallback_chain` | fixed "selected → active → local" |
| concurrency | `queue.rs` 3 semaphores (ollama/anthropic/gemini) | unknown `provider_hint` falls into the ollama pool; chat and sub-agents unmetered |
| usage ledger | DuckDB `model_usage` (4 call sites: 3 in `chat.rs`, 1 in `shared_llm.rs`) | no `provider` column; cost is computed but never capped |
| catalog | `models.yaml` → `model_registry` | cost/rank/capabilities only; consumers: `model_router`, `model_config`, `/api/models`, `self_model`, `skills/model.rs` |

**The OpenAI-compatible client already exists** (`llm_providers/openai_compat.rs`) and chat's
`run_ollama_cloud_loop` already drives OpenAI-style tool calls. What's missing is the ability to
point them at an arbitrary endpoint and key from config, and to see response headers (rate-limit
headroom is currently invisible).

## Measured load (Phase 0, 2026-09-13, last 7 days of `model_usage`)

| | calls | tokens in | tokens out |
|---|---|---|---|
| **total** | ~2,640 (43–846/day, ≈378/day avg) | ~11.1 M (≤4.9 M/day) | ~0.7 M |
| gemma4-e4b (local llama.cpp) | 962 | 3.43 M | 200 K |
| gpt-oss:120b-cloud | 776 | 5.99 M | 297 K |
| granite4 (local Ollama) | 730 | 0.84 M | 161 K |
| gemma4:31b-cloud | 117 | 0.45 M | 32 K |
| gemini-2.5-flash | 28 | 59 K | 4 K |
| claude-sonnet-4-6 (**paid**) | 4 | 23 K | 1 K |

- **By tool:** chat 766, `subagent:knowledge-worker` 451, `ingest_media` 450, `reason` 393, `claim` 332.
- **Burstiness:** peak **26 calls in one minute**; 5.3 per active minute on average.
- **Spend:** **$0.08** over the week, all Anthropic. Cost tracking works; there are no caps and no
  per-provider view.

**What this sizes:**
- **OpenRouter `:free` (20 rpm, 50 rpd) is a fallback rung only.** One peak minute exceeds its
  rpm cap; one busy hour exceeds its daily cap.
- **A GPU lease moves ~1,700 local calls and ~4.3 M tokens a week onto cloud providers.** On free
  tiers alone that is the load that will hit limits, which is exactly where a paid provider
  should slot in once one exists.
- **Paid spend is already non-zero** with no cap. The spend guard isn't only for tomorrow.

---

## Design

### 1. Provider registry (`providers:` in `models.yaml`)

Every model's `provider:` names an entry here. Endpoints and **env var names** live in YAML; keys
never do.

```yaml
providers:
  ollama:          { kind: ollama,       billing: local, data_policy: no_log,
                     base_url_env: OLLAMA_LOCAL_URL, base_url_default: "http://localhost:11434",
                     limits: { concurrency: 2 } }
  llamacpp:        { kind: llamacpp,     billing: local, data_policy: no_log,
                     base_url_env: LLAMACPP_URL, base_url_default: "http://localhost:8080",
                     limits: { concurrency: 2 } }
  ollama-cloud:    { kind: openai_compat, billing: free, data_policy: no_log,
                     base_url_env: OLLAMA_URL, base_url_default: "https://ollama.com",
                     api_key_env: OLLAMA_API_KEY,
                     limits: { concurrency: 1 },
                     terms: "never logged or trained on (pricing page, checked 2026-09-12)" }
  mistral:         { kind: openai_compat, billing: free, data_policy: may_train,
                     base_url_default: "https://api.mistral.ai", api_key_env: MISTRAL_API_KEY,
                     limits: { concurrency: 1 },
                     terms: "Experiment plan: inputs/outputs may train Mistral models" }
  openrouter:      { kind: openai_compat, billing: free, data_policy: may_train,
                     base_url_default: "https://openrouter.ai/api", api_key_env: OPENROUTER_API_KEY,
                     limits: { rpm: 20, rpd: 50, concurrency: 1 } }
  anthropic:       { kind: anthropic, billing: paid, data_policy: no_log, api_key_env: ANTHROPIC_API_KEY,
                     spend: { daily_usd: 1.00, monthly_usd: 10.00 } }
  gemini:          { kind: gemini, billing: free, data_policy: may_train, api_key_env: GEMINI_API_KEY,
                     limits: { rpm: 5 } }
```

- **`kind`** selects the client code: `ollama | llamacpp | openai_compat | anthropic | gemini`.
  `openai_compat` is the generic kind: any OpenAI-wire API is a new YAML entry.
- **`billing`**: `local` (own hardware), `free` (a free tier: rate-limited, $0), `paid` (metered money).
- **`data_policy`**: `no_log | may_log | may_train`. **Unknown ⇒ `may_train`.**
- **`private_ok_for`** (added 2026-09-26): purposes (`chat | subagent | background`) whose
  *private* work may use this provider despite its `data_policy`. It is the owner's override,
  deliberately separate from `data_policy` so the provider's terms stay recorded as written.
  It lifts only the privacy filter; billing, spend caps and the sub-agent rpm/rpd floor still
  apply. `gemini` sets `[chat]`.
- **`limits`**: `rpm`, `rpd` (requests/day), `tpd` (tokens/day), `concurrency`. All optional;
  absent = unenforced.
- **`spend`**: `daily_usd`, `monthly_usd`. **A `paid` provider with no `spend` block is not
  routable.**
- **`enabled`** (default true) and **key presence**: a provider whose `api_key_env` is unset is
  skipped, never an error.
- **Per-model overrides** (`data_policy`, `billing`, `limits`) handle mixed providers. Example:
  an OpenRouter `:free` model vs a paid model on the same account, or Gemini's free tier vs
  billed usage.
- **Anthropic is `no_log` in this table only as the API default** (it does not train on API
  data). It remains subject to the separate rule that its outputs are never used as training
  data (fine-tuning plan).

**Backward compatibility:** a model whose `provider:` isn't in `providers:` resolves through a
built-in default table equal to today's behaviour, so nothing breaks before the YAML is filled in.

### 2. Routing policy (`ROUTING_BILLING`, per-purpose)

Replaces the numeric `CLOUD_TIER`.

| env | default | meaning |
|---|---|---|
| `ROUTING_BILLING` | `local,free` | billing classes the router may use at all |
| `ROUTING_PAID_FOR` | *(empty: nothing may spend)* | purposes allowed to spend money: any of `chat,subagent,background`. Empty by decision on 2026-09-15, until a paid key is in use |

- `CLOUD_TIER` stays readable for compatibility: `0 → local`, `1 → local,free`,
  `2 → local,free,paid` (paid still needs `ROUTING_PAID_FOR` + a spend cap).
- **"Paid when needed"** is expressed by `selection_rank` + cost: paid models rank behind free
  ones, so they're reached only when free rungs are unavailable (budget exhausted, cooling down,
  or lacking the capability). Raising a paid model's rank makes it preferred.

### 3. Work carries a data class

`data_class: private | public` on context profiles, chains, schedules and individual chain steps.
**Absent ⇒ private.**

> **Routing rule:** private work may only use a provider/model whose effective
> `data_policy` is `no_log`. With no eligible rung, a background job **defers** (retried later
> by the queue) and interactive chat **fails loudly** with the reason. Neither ever falls back
> to a `may_log`/`may_train` provider.

Proposed defaults (the `researcher` classification is an open decision):

| unit | class |
|---|---|
| chat front door, `knowledge-worker`, `scribe`, `job-applicant`, `code-analyst`, `toolsmith`, `self-analyst` | private |
| news brief, `api-scout`, media summaries of public videos, fine-tuning teacher sampling | public |
| `researcher` | **open** (public topics, revealing question set) |

### 4. One router

```text
route(RouteRequest { purpose: chat|subagent|background,
                     data_class, required_capabilities, preferred_model?, preferred_provider? })
  -> RoutePlan { candidates: [Candidate{model, provider, config, why}], rejected: [(model, reason)] }
```

**Filters, in order.** Each rejected model records a reason, so "why didn't it use X?" is
answerable:

1. provider enabled and key present
2. billing allowed (`ROUTING_BILLING`; paid also needs `ROUTING_PAID_FOR` ∋ purpose and a spend cap)
3. **data policy permits the data class**
4. capabilities satisfied
5. budget available: not cooling down, rpm/rpd/tpd not exhausted, spend cap not reached
6. observed availability: no recent `subscription_required`, as today

**Ordering:** the preferred model (profile pin) first if it survives, then
`cost ASC, selection_rank ASC, context_window DESC` (today's ordering).

**The plan is the fallback chain.** `SharedLlm` walks `candidates` on an unavailable-class
failure instead of today's fixed selected→active→local chain, and the chat ladder is expressed
as preferred models filtered through the same router. The sub-agent clamp becomes policy:
`purpose: subagent` with default `ROUTING_PAID_FOR=chat` excludes paid automatically, and
data-class filtering excludes training providers for private agents. No provider-type allow-list.

### 5. Budget ledger

- **`model_usage.provider TEXT`** (new column) and cost recorded per call, as today.
- **In-memory `ProviderBudget`** per provider (and per model where limits are per-model):
  - sliding 60 s request window (rpm)
  - today's requests, tokens and spend, seeded at startup from `model_usage` since the provider's
    reset boundary (UTC midnight default)
  - this month's spend
  - `cooldown_until`
  - last seen `x-ratelimit-limit-*` / `x-ratelimit-remaining-*` / `retry-after` headers
- **Before a call:** `route()` excludes exhausted providers. **After a call:** record it and
  update the window. **After a 429:** cooldown until `Retry-After` or reset, else 1 m → 5 m → 1 h.
  A reported limit of 0 marks the model unavailable until re-probed (Mistral Medium on the free
  plan).
- **Header capture:** `OpenAiCompatProvider` returns rate-limit headers alongside the response
  (and in the error on failure), so Mistral/OpenRouter headroom feeds the budget.

### 6. Concurrency gate

A process-wide `ProviderGate` holds one semaphore per provider, sized from `limits.concurrency`.
It's acquired by `SharedLlm`, the chat loops and `AgentRunner` around each call, and by the queue
when dispatching. Ollama Cloud's free plan = 1 means chat, sub-agents and background jobs share
one slot. Waiting callers queue, with interactive chat served first.

### 7. GPU lease

Unchanged from the earlier draft:
- `gpu_lease(acquire|release|status, holder, reason)` via tool + REST, persisted as `(:GpuLease)`
- while held, `billing: local` generation providers are unavailable to the router
- private work routes to `no_log` cloud (Ollama Cloud) or defers
- `store_note` writes `embedding_pending` (bge-m3 lives on the GPU) and a backfill runs on release

### 8. Observability

- **`get_llm_budget`** tool + **`/api/llm/budget`**: per provider, billing, data policy, today's
  requests/tokens/spend vs limits and caps, monthly spend, cooldowns, concurrency in use, and the
  lease state.
- **WARN** when private work defers for lack of a `no_log` rung, and when a spend cap blocks a
  paid provider. These are the signals that more capacity is needed.
- **`route()` logs the chosen model and the rejection reasons** of the models it skipped.

---

## Failure modes this must not introduce

| risk | guard |
|---|---|
| private data sent to a training provider via fallback | data-policy filter inside `route()`; unknown policy = `may_train`; test that private + only-`may_train` candidates yields an empty plan |
| surprise spend | paid requires a spend cap; `ROUTING_PAID_FOR` defaults to `chat`; spend checked before every paid call |
| a new provider silently changes routing | rejection reasons logged; catalog tests pin current routing by name (existing `model_config` guards extended) |
| free-tier limit relearned per call | cooldowns + `limit = 0` → unavailable until re-probed |
| Ollama Cloud's single slot starves chat | process-wide gate with chat priority |
| breaking existing behaviour before YAML is filled in | built-in default provider table mirrors today's hardcoded values; `CLOUD_TIER` still honoured |

## Phases

- [x] **Phase 0: Measure.** Done above.
- [x] **Phase 1: Registry + generic OpenAI-compatible providers.** Done 2026-09-13.
  - `services/provider_registry.rs`: schema, a built-in table equal to the old hardcoded endpoints, and a process-wide slot installed from `models.yaml` at startup and by `reload_models`. Unknown YAML fields are rejected; omitted `billing`/`data_policy` default to `paid`/`may_train`.
  - `LlmProviderType::OpenAiCompat` plus `LlmConfig.provider_name` / `provider_label()`; `with_provider` clears a stale name.
  - `config_for_catalog_entry` reads the endpoint and key env var from the registry. Ollama Cloud keeps its own provider type, so `CLOUD_TIER` and the sub-agent clamp are unchanged.
  - Chat dispatch sends `OpenAiCompat` to the OpenAI tool loop. `model_usage.provider` is recorded on every ledger row.
  - `providers:` section plus 3 Mistral and 2 OpenRouter `:free` models at ranks 350–372, behind every other model.
  - `/api/models` returns the registry: key env var names and `key_configured`, never values. Startup warns on provider issues; guard tests assert the checked-in declarations have none and that the training free tiers stay `may_train`.
  - **Not routable yet, by design:** `candidate_allowed` rejects `mistral`/`openrouter` and the sub-agent clamp drops `OpenAiCompat`. Only an explicit `/chat` profile pin reaches them until Phase 2.
- [x] **Phase 2: Policy router.** Done 2026-09-15.
  - `services/routing.rs`:
    - `RoutingPolicy` is built from `ROUTING_BILLING`, falling back to the `CLOUD_TIER` mapping, plus `ROUTING_PAID_FOR`.
    - `check_entry` runs the §4 filters in order: registered, enabled, key, billing, paid purpose and spend cap, data policy, then loop limits for sub-agents.
    - `admit(config, ctx)` judges one ready config. `route(rows, ctx, …)` filters the capability-matched catalog, keeping its order, with a reason for every rejection.
  - **Route context.** A `ROUTE_CONTEXT` task-local (`Purpose` + `DataClass`) is scoped by the chat turn (per ladder attempt), `AgentRunner::run` and the queue coordinator. Anything unscoped reads as background/private.
  - **Where the policy now applies:**
    - capability routing: `resolve_model_config` takes the context and logs the winner plus every better-ranked model it skipped, with the reason
    - `SharedLlm`: every rung, primary included, must be admitted; a refused primary walks the chain, and with nothing admitted the call fails with all reasons
    - chat: the profile pin, each ladder rung and synthesis
    - sub-agents: the pin and ladder, with the class the stricter of caller and profile
    - `construct_agent`'s planner, which inherits the caller's class
  - **The provider-type allow-lists are gone:** `candidate_allowed` and the sub-agent clamp.
  - **Where `data_class` is set:**
    - `ContextProfile.data_class`; a save that omits it keeps the stored value, so a UI edit can't flip a profile private
    - `ChainStep.data_class`, carried as `__data_class` in job args
    - a chain or schedule file's top-level `data_class`, stamped onto steps by the seeders; the three envelope schemas declare it
  - **Observability:** `/api/models` returns `routing_policy`.
  - **Guards:**
    - policy unit tests, including private work with only training providers getting an empty plan
    - real-catalog tests: private reasoning goes to `gpt-oss:120b-cloud`, public reasoning to `gemini-2.5-flash`, private work never reaches a non-`no_log` provider for any capability, and paid stays unroutable until a purpose may spend
    - chat tests: a fully refused turn fails loudly and sends nothing; a refused primary is skipped
  - **Decisions taken 2026-09-15:**
    1. The front door (`GeneralDefault-Orchrestator`) is **public** and keeps `gemini-2.5-flash`.
    2. `researcher` is **private**.
    3. The four monitors (hardware-tripwire, slm-benchmark-watch, off-grid-networking-monitor, tech-dependency-synthesis) are **public**, and `learn` is **private**.
    4. **Nothing may spend yet**, so the `ROUTING_PAID_FOR` default is empty, not `chat`.
    5. `api-scout` is public, per the proposed table.
  - **Deviations from the design:**
    - `SharedLlm` keeps the selected → active → local order, with each rung admitted, rather than walking the full candidate list. A longer walk matters once cooldowns exist (Phase 3).
    - A preferred model is expressed as "admit the pin, else the default", not as an ordering input to `route()`.
    - No defer mechanism is needed yet: private background work always has a `no_log` rung (Ollama Cloud or local), so deferral arrives with the GPU lease (Phase 5).
    - Per-model policy overrides are not implemented; no provider mixes policies today.
    - The sub-agent loop thresholds (`rpm ≥ 20`, `rpd ≥ 500`) are constants until Phase 3 budgets replace them.
- [x] **Phase 3: Budget ledger.** Done 2026-09-15.
  - **`services/budget.rs`** holds a `BudgetBook` behind a process-wide mutex. Every method takes `now`, so the rules are unit-tested.
  - **Per provider:** requests in a sliding 60 s window, plus requests, tokens and spend for the UTC day, and spend for the month. `verdict` checks them against the declared `limits` (`rpm`, `rpd`, `tpd`) and, for `billing: paid`, the `spend` caps, with a WARN when a cap blocks.
  - **Per (provider, model):** cooldowns and the last rate-limit headers seen. Limits and 429s were measured per model, so a cooldown never benches a whole provider.
  - **Per-model limits (2026-09-26):** a `models.yaml` model entry may declare `limits:` (rpm/rpd/tpd). The budget counts that model alone (seeded from `model_usage` at startup like the provider counters) and checks it before the provider's limits, and the sub-agent loop floor applies to it too. The `gemini` provider no longer declares `rpm: 5`; each Gemini model carries its own, because Google's free-tier quotas are per model, per project. Before this, chat, the voice and background reasoning drew from one shared 5 rpm bucket across every Gemini model. Guard: `every_gemini_model_declares_its_own_rate_limit`.
  - **One write path:** `record_call` writes the DuckDB usage ledger and the budget together. `record_model_usage` now returns the computed cost. All four ledger sites use it: `SharedLlm::run_and_record`, chat's `record_llm_call`, and the Ollama Cloud and Anthropic error paths.
  - **Cooldowns, most trusted first:**
    - `Retry-After` on a 429 or 503, or a `…remaining…req…: 0` header with its reset time (RFC 3339, unit duration like `6m0s`, epoch ms/s, or seconds)
    - a retry hint in the error body (Gemini's `retryDelay`, "retry in 12.5s")
    - otherwise backoff of 60 s → 5 m → 1 h, reset by a success
    - a reported limit of `0` benches the model for an hour, then one call re-probes
    - cooldowns only ever lengthen
  - **Header capture:** `budget::observe_headers` runs wherever a response arrives, before the body is read. That covers the OpenAI-compatible, Anthropic and Gemini providers, via a new `ProviderConfig.provider_name`, and chat's Ollama Cloud loop. Only `*ratelimit*` and `retry-after` headers are kept.
  - **Enforcement:** `RoutingPolicy::admit` consults `budget::verdict` after the policy checks. `route()` takes a `budget` closure and records the reason for each skip. So a cooling or spent model is skipped by capability routing, the `SharedLlm` fallback walk and both chat ladders before any call is made.
  - **Seeding:** at startup, after the catalog sync, from `model_usage` via `TelemetryClient::budget_usage_since`. Rows written before `provider` existed take their provider from the catalog, so a restart does not hand back spent quota.
  - **Observability:** the `get_llm_budget` tool (in `ModelSkill`, allowlisted for `general` and `self-analyst`) and `/api/llm/budget`. Both show each provider's declaration, today's usage, month spend, last-minute requests, headroom against declared limits, and per-model cooldowns with reasons and headers.
  - **Deviations from the design:**
    - Day boundaries are UTC midnight for every provider; Gemini's free tier actually resets at Pacific midnight.
    - Declared limits count per provider; provider-reported limits act per model through cooldowns.
    - The sub-agent loop thresholds (`rpm ≥ 20`, `rpd ≥ 500`) stay as a loop-shape gate in `routing.rs`, separate from quota.
    - `SharedLlm` keeps the selected → active → local chain rather than walking the full candidate list; with cooldowns now skipping benched rungs, a longer walk is a possible follow-up.
- [x] **Phase 4: Concurrency gate.** Done 2026-09-23.
  - **`services/provider_gate.rs`**: one gate per provider, sized from `limits.concurrency`. Providers with no declared concurrency are ungated. Configured by `provider_registry::install`, so startup and `reload_models` both resize it live: a raised limit wakes waiters, a removed one lets them proceed ungated, and a declared 0 is read as 1.
  - **Verified limits.** Ollama Cloud free is "1 concurrent request" (Pro 3, Max/Team 10; ollama.com/pricing, checked 2026-09-23). llama.cpp is its `-np 2` slots.
  - **Measured before sizing (30 days of `model_usage`).** No provider ever had more than 2 calls in flight.
    - Ollama Cloud: only 16 of 1,520 calls started while another was running, so a gate of 1 costs ~1% of calls a wait. Its six 429s (all on 2026-08-26) each had nothing else in flight: quota, not concurrency.
    - llama.cpp: 622 of 2,847 calls overlapped. This is where the gate earns its keep, by giving chat priority over background jobs.
  - **The permit covers one request, never a job or a turn.** It is acquired in `LlmClient::generate_with_system` / `chat` (covering `SharedLlm`, the distiller, the context builder, synthesis and the text loop) and around the send in chat's three raw loops (Anthropic, native Ollama, OpenAI-compatible). In those loops it is dropped once the response is read, before tools run, because a turn delegating to a sub-agent on the same one-slot provider would otherwise wait on itself. It is taken before the call clock starts, so waits are not billed as call time.
  - **Priority by `ROUTE_CONTEXT` purpose**: chat, then sub-agent, then background; FIFO within a class. A waiter that gives up (an abandoned turn) is skipped without leaking a slot. Waits are bounded by the holder's HTTP timeout, not a timer of their own, and logged at INFO from 1 s and WARN from 30 s.
  - **Not gated: embeddings** (short, local; gating them behind generation would stall retrieval).
  - **Observability**: `/api/llm/budget` and `get_llm_budget` show each gated provider's capacity, in-use slots, waiters per class, totals and max wait.
  - **Deviation from the design: the queue's three job semaphores stay.** They cap how many jobs run, not how many requests a provider serves (a job also searches and writes the graph, and its `provider_hint` is not always the provider it calls). Making them the provider gate would have a job take the slot and then its model call take it again, deadlocking a one-slot provider. The queue docs now say so.
- [x] **Phase 5: GPU lease.** Done 2026-09-23.
  - **`services/gpu_lease.rs`**: `LeaseBook` holds the rules. A different holder is refused; the same holder renews (keeping the start time); the TTL defaults to 12 h and is clamped to 1–72 h; only the holder or `force` releases a live lease; releasing when nothing is held is a no-op; an expired lease is simply not held. The lease persists as `(:GpuLease {id:'current'})` with native datetimes and is reloaded at startup, so a restart mid-training does not reclaim the GPU.
  - **Driven by** the `gpu_lease` tool (`ModelSkill`), `GET`/`POST /api/gpu-lease` (409 with the reason when refused), and `workspace/finetune/scripts/gpu_lease.sh acquire|release|status`. `/api/llm/budget` and `/api/models` (`routing_policy.gpu_lease`) show the state.
  - **While held:**
    - `RoutingPolicy` refuses every `billing: local` provider with the lease as the reason. That covers capability routing, chat pins, ladders, sub-agents and `SharedLlm` fallback rungs.
    - `LlmClient` refuses local generation and embeddings, a backstop for the distiller and context builder. bge-m3 lives on the GPU, and one search would reload it.
    - The queue defers jobs with `provider_hint: ollama` and no capability routing: they keep their heap position and attempt count, and are counted in `deferred_for_gpu_lease` in the queue status.
    - `store_note_raw` marks vector-less notes `embedding_pending`, and entity extraction is skipped.
  - **On acquire** the brain unloads host Ollama via `/api/generate` with `keep_alive: 0`, which unloads embedding models too (verified on bge-m3). The llama.cpp and whisper sidecars are stopped from the host by the script, once the gates show no local request in flight; on release it restarts them and waits for their ready log lines (`model loaded`, `Application startup complete`) before returning the lease.
  - **Release hook** (run on release, on expiry at the first check after it, and at startup with no lease): `run_embedding_backfill` embeds pending notes oldest first, links similar notes, and extracts entities for non-chunk notes with no `MENTIONS`.
  - **Decision 4 resolved:** private background work *defers*, bounded by the lease's own expiry, rather than failing or moving to Ollama Cloud, whose one free slot and monthly credit could not carry ~240 local calls a day.
  - **Verified live 2026-09-23** (acquire → release): bge-m3 unloaded on acquire; a second holder got 409; the policy and graph carried the lease; search answered from BM25 without reloading bge-m3; a stored note came back `embedding_pending` with no vector; on release the backfill embedded it (1024 dims) within ~4 s, bge-m3 was loaded again, and the lease node was removed. Not exercised live: queue deferral (nothing was queued, so it is covered by a unit test only), and a restart mid-lease.
  - **Known limits:** `reason` and other tools that run on `local_llm` fail with the lease reason during a lease rather than moving to cloud. Jobs with a `ttl_secs` can expire while deferred. Notes that lost their vector before this phase are not marked pending, so the backfill does not reach them.

### Probe findings for Phase 3 (2026-09-13)

I sent one trivial public prompt to each provider, at the exact registry URL (`base_url_default` + `/v1/chat/completions`):

- **Mistral limits are per model, not per account.** `mistral-small-latest` returned 429 `rate_limited` (code 1300) while `codestral-latest` answered in 0.4s seconds later. The success carried `x-ratelimit-limit-req-minute: 125`, `x-ratelimit-limit-tokens-minute: 625000`, their `remaining-*` twins and `x-ratelimit-tokens-query-cost`. Header capture can seed per-model budgets directly.
- **OpenRouter's `:free` 429 can be upstream.** `google/gemma-4-31b-it:free` returned "temporarily rate-limited upstream" (`provider_name: Google AI Studio`) while `nvidia/nemotron-3-super-120b-a12b:free` answered. **Cooldowns therefore key on (provider, model).** The success carried **no rate-limit headers**, so OpenRouter's 20 rpm / 50 rpd budget must be counted locally from the ledger.
- **Nemotron via OpenRouter is a reasoning model.** 15 of 16 completion tokens were `reasoning_tokens` and the visible content leaked the reasoning. Give it a generous `max_tokens`, and don't route strict-JSON steps to it without a test.

## Open decisions

1. `researcher` data class.
2. Whether background work may ever use paid (`ROUTING_PAID_FOR`).
3. Starting spend caps once a paid key is added.
4. Whether private background work should defer indefinitely or fail after N hours during a long GPU lease. **Resolved (Phase 5): defer, bounded by the lease expiry.**

## Sources

- [Ollama pricing](https://ollama.com/pricing)
- [Mistral commercial terms](https://legal.mistral.ai/terms/commercial-terms-of-service)
- [OpenRouter free tier limits](https://klymentiev.com/blog/openrouter-free-tier)
- [OpenRouter free models: limits and catches](https://ask-coreai.com/blog/openrouter-free-models-2026-limits-catches)
- Mistral per-model limits measured via `GET /v1/models` and a probe call, 2026-09-12 (`RUST_FINETUNE_EXPERIMENT.md`).
