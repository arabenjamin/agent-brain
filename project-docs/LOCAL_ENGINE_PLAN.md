# Local engine plan

One local engine, chosen at install, instead of two running side by side. Today
generation runs on a llama.cpp sidecar while embeddings and the classifiers run
on host Ollama, so anyone else who runs the brain has to install both. Planned
2026-11-01, measured the same day. Owner's decisions are in §7.

Goal: `LOCAL_ENGINE` picks the engine. llama.cpp is the default and the only
engine this box runs. Nothing outside the engine layer names an engine.

The Ollama Cloud deadline (§2) is a separate problem with an earlier date, and
it comes first. Removing host Ollama does not touch the `ollama-cloud` provider,
and vice versa.

## 1. What we measured (2026-11-01)

Throwaway CPU-only containers from our own `agent-brain-llamacpp` image (b10900,
GPU hidden), reading Ollama's model files read-only. GPU behaviour is **not
tested yet**; it needs the GPU lease and the brain resting (Phase 1).

| Question | Result |
|---|---|
| One `llama-server` serving every model? | **Yes.** Router mode (`--models-preset`) served Gemma E4B, bge-m3, Nimble and Guardian on one port, each loaded on first request, all resident together (`--models-max 4`) |
| bge-m3 vectors the same as Ollama's? | **Yes.** Cosine 0.9998 to 1.0000 on 5 texts (English, code, one word, long, mixed language). Same 1024 dimensions. llama.cpp normalizes (length 1.0, Ollama ~26); every index and query uses cosine, so that does not matter. **No re-embedding expected** |
| Nimble on llama.cpp? | **Yes.** It loads and answers. `/v1/systemone` is Ollama's own endpoint, but it only builds a prompt and normalizes first-token probabilities. Rebuilt from Ollama's source in a test script: 10 of 10 same answer, mean difference 0.005, worst 0.041, none crossing the 0.7 bar. 3-6 s a question on 6 CPU threads |
| Guardian on llama.cpp? | **Yes, with work.** Ollama supplies its prompt from a 14 KB Go template; the GGUF alone gives garbage. Given the prompt Ollama renders, 6 of 6 same verdict. 16 s a check on 6 CPU threads (documented 3-5 s on Ollama, not re-timed here) |
| Gemma E4B tools | Tool call came back correctly formed |
| Gemma E4B `json_schema` | Works with the prompts we send: 8 of 8 on the real picker prompt, and 16 of 16 live turns routed. Empty content only when the prompt does not ask for JSON; see §6 |

Ollama's model files are not all portable: `gemma4:e4b`'s blob fails to load
(`wrong number of tensors; expected 2131, got 720`). The unsloth GGUF the sidecar
already uses is fine.

## 2. Ollama Cloud (before 2026-11-09)

The owner reports the free tier ends on the 9th. ollama.com/pricing no longer
lists a Free plan, only $0 "Pay as you go: pay per token for every cloud model".
It gives no end date, and third-party trackers still say free, so **the date is
the owner's; the page only agrees with it.**

Last 7 days, from the ledger (`model_usage`):

| Provider | Calls | Tokens | Cost |
|---|---|---|---|
| llamacpp | 2,178 | 3.4M | $0 |
| mistral | 2,102 | 0.8M | $0 |
| ollama-cloud | 492 (10%) | 5.1M (55%) | $0 |

No generation went to host Ollama in those 7 days. Nimble, Guardian and
embeddings call Ollama directly and do not appear in the ledger.

The cloud calls are the hard ones: `subagent:ml-student` 155, `subagent:deep-diver`
74, `reason` 37, plus `link_entities` 166 small ones. Investigative loops need
cloud (a standing rule), so this load cannot simply fall to E4B.

**The risk is silent billing.** `models.yaml` declares `ollama-cloud` as
`billing: free` with no `spend:` cap. If the key keeps working on pay-per-token,
the ledger records $0 and no cap applies. Whether it would bill without a card on
file is unknown.

Also depends on it: `LLM_PROVIDER=ollama-cloud` (compose and `.env`), the first
four rungs of `chat_fallback_ladder`, 32 `provider_hint:
ollama-cloud` lines across workflows and profiles, and profiles pinned to its models (`deep-diver`, `rnd-scout`,
Harness² judges and workers).

### Phase 0: get off it before the 9th

1. **Declare it honestly.** `enabled: false`, `billing: paid`, `spend` 0. *Built.*
2. **Move the load to Mistral** (§7.1): `LLM_PROVIDER=mistral`, `OLLAMA_MODEL` and
   `CHAT_LLM_MODEL` = `ministral-14b-latest`, the 32 `provider_hint` lines and 7 pinned profiles,
   `ministral-14b-latest` to rank 96 so it also leads public work (Gemini-2.5 is
   98). The chat ladder is Ministral, Codestral, OpenRouter, E4B. *Live 2026-11-01.*
3. **The deploy is the fire drill.** With the provider off, nothing reaches Ollama
   Cloud, and the key stays in `.env` so going back is one line. For a few days read
   which workflows fail or fall to E4B, and compare `ml-student`, `deep-diver` and
   `intake`. *Not started.*
4. **Judges re-measured 2026-11-02: Ministral fails.** Two known-answer runs (47066863,
   90b7a48d) on the original inputs. Neither reached a checked fix: in the first the
   reference probe made no tool calls and no candidate probe ran; in the second one
   editor made no calls and the reflector says the final run never executed. The
   composer still chose an edit both times, and both reflectors, with 0 tool calls,
   named a report file they did not write. Both runs read `completed`. Judges moved to free Nemotron Ultra, below.

Probes 2026-11-02, 3 each, one task per agent, side effects stubbed (no todo, note or
task left behind). Small sample; behaviour only, not whether the conclusions are right.

| Agent | Ministral | Codestral | E4B |
|---|---|---|---|
| intake | 3 of 3 used tools, 2 started the source | 0 tool calls, 3 of 3 | 0 calls in 2 of 3, none started the source |
| ml-student | 3 of 3 ran the code | 2 of 3 | 1 of 3 |
| deep-diver | 3-5 calls, read the code | 2 searches, no file read | 0 calls, 3 of 3 |

Each model had a run claiming work it did not do (Ministral `intake`: "started reading";
Codestral and E4B `ml-student`: `ran: yes`). `deep-diver` averaged 13 calls on gpt-oss in
real runs, on other tasks with no 10-round cap, so that gap is a warning, not a measurement.
Codestral was dropped on 2026-11-02 (owner).

OpenRouter, 2026-11-02 ($10 of credit: 1000 free requests/day). Judges moved to free
`nemotron-3-ultra-550b-a55b`: 2 of 2 known-answer runs found the `manage_todo` fix, wrote
their reports and ran the candidates, though both reflectors cited steps of a reference run
that had made no tool calls, so read them. As a worker it is not dependable: 10 of 58 calls
were 5xx, each benching it, and pinned probes then ran on E4B.

Priced models, `openrouter-paid` (`provider: {zdr: true, ignore: [Groq]}` on every request,
seen in the logs before any private task; Qwen left out by the owner). 3 probes per cell,
one task per agent, real spend $0.14:

| Agent | GLM 5.3 Flash | DeepSeek V4 Flash | gpt-oss 120B |
|---|---|---|---|
| intake: started the source | 0 of 3 | 2 of 3 | 0 of 3 |
| ml-student: ran the code | 3 of 3 | 3 of 3 | 0 of 3, all claiming `ran: yes` |
| deep-diver: usable answer | 3 of 3, 18-31 calls | 1 of 3; 2 ended in raw tool-call markup | Yes, but one claims a compile error in code that compiles |

Citations checked by hand on a sample: GLM mostly right with some wrong lines (`YtDlpGate` at
service.rs:1754, not 1778); DeepSeek's `on_job_dead` line is wrong. All found the same thing,
that a job retries at once during the yt-dlp backoff and dies; not yet checked as a real bug.
Result: GLM for `deep-diver` and `ml-student`, Ministral stays on `intake` (§7.7).

Mistral's headers on 2026-11-01: 30 requests and 937,500 tokens a minute, no monthly
figure, so the free plan's monthly ceiling is unknown. Everything now shares
`concurrency: 1`.

Left alone: `zero-trust-architecture-research` still names the provider; it is
runtime-owned and disabled.

Incident 2026-11-02: queued `reason` steps on the active config all failed with a 404 from
host Ollama, 5 tasks dead, because startup built that config before loading `models.yaml`.
Fixed the same day.

Done when: several days after deploy show no failing workflow that was not decided
on, the ledger has no `ollama-cloud` rows, and the judges have a measured answer.

## 3. What is tied to Ollama

| Tie | Where | Answer |
|---|---|---|
| Embeddings | `llm_providers/ollama.rs` `/api/embeddings`; `build_local_llm_config` pins it | `/v1/embeddings` |
| Decision calls | `/v1/systemone`: `retrieve/api_tags.rs`, `chat/route.rs` (checker), `alias-judge/judge.rs` | One Rust `decision` module (Phase 2) |
| Guardian | `/api/chat` with `think`, `document` role: `epistemics/grounding.rs` | Render the prompt ourselves (Phase 3) |
| GPU lease | `gpu_lease.rs` `/api/ps`, `keep_alive: 0` | Engine's unload call; llama.cpp router has one (to verify) |
| Chat wire | `chat/ollama_loop.rs`, `route.rs` Ollama branch | OpenAI wire only |
| The word itself | 270 `provider_hint: ollama` steps; `OLLAMA_LOCAL_*`; `keep_alive`, `num_ctx` | `local`, roles (§4) |
| Local catalog | 5 `provider: ollama` models; `granite4:latest` is the chat ladder's last rung and `OLLAMA_LOCAL_MODEL` | One local catalog per install; last rung becomes E4B |
| Vision | `gemma4:latest` is the only local `vision` model; the sidecar runs `--no-mmproj` | §7.4 |

Whisper has its own container and is out of scope.

## 4. Design

**The contract is the OpenAI wire.** `/v1/chat/completions` (tools, `json_schema`,
`logprobs`), `/v1/embeddings`, `/v1/completions` and `/v1/models`. llama.cpp, vLLM
and Ollama all speak it, so core keeps one local client.

**An engine is a YAML entry**, following the no-hardcoded-data rule: endpoint,
which capabilities it has, how to unload. What differs is declared, not branched
on in Rust.

```yaml
engines:
  llamacpp: { base_url_default: "http://llamacpp:8080", unload: router,
              capabilities: [chat, tools, json_schema, embeddings, logprobs, completions] }
  openai:   { base_url_env: LOCAL_ENGINE_URL, capabilities: [chat] }   # bring your own; declare what it has
```

**A missing capability turns one feature off, visibly.** No `logprobs` means no
Nimble calls; `/health` and `lookup` say which feature is off and why. Embeddings
are the exception: with no embeddings there is no retrieval, so startup refuses.

**Models by role, not by name:** `LOCAL_MODEL`, `LOCAL_EMBED_MODEL`,
`LOCAL_DECISION_MODEL`, `LOCAL_GROUNDING_MODEL`, each `off`-able where it is
optional. A role's model-specific needs (Nimble's system prompt, today inside
Ollama's manifest; Guardian's template) move into its catalog entry.

**`provider_hint: local`**, with `ollama` still accepted and logged as deprecated,
because stored and runtime-owned workflows carry it. Migrate the YAML; migrate
stored workflows once, with a count. Old ledger rows stay as they are.

**Install.** `scripts/install.sh --engine llamacpp --gpu cuda|cpu` writes `.env`,
picks the compose profile and fetches models from a manifest (repo, file, sha256).
The llama.cpp Dockerfile's CUDA 12.2 / `sm_86` become build args. A sized preset
for 8 GB, others for CPU-only and larger cards, each a file.

Compose deliberately writes deployment values as literals, because a stale `.env`
silently overrode one once. `LOCAL_ENGINE` is the one exception: it has to be a
variable, so `/health` reports the engine and models actually in use, and startup
logs any difference from the installer's record.

## 5. Phases

Host Ollama stays installed and untouched until Phase 5, so every phase can go
back by flipping an env var.

**Phase 1: router with bge-m3, on the GPU.** Replace the sidecar's command with a
preset. Measure under the GPU lease, with the brain resting: E4B + bge-m3 VRAM
(3,684 + 645 MiB through separate engines today), E4B's 16,384 tokens a request,
and whether Nimble and Guardian belong on CPU or part-GPU. Nimble on CPU would end
the eviction churn with bge-m3 that keeps `CHAT_ROUTER_CHECKER` off, but the ML
runner takes 12 of 16 cores from 23:00 to 07:00, so measure contention. Copy the
bge-m3 and Nimble GGUFs into the model volume. Fix §6 first if it is the router's
own problem.
*Done when:* `eval-retrieval` scores recall@10 and MRR unchanged against the 0.92 /
0.74 baseline (re-measure first) with embeddings on llama.cpp, new and existing
vectors compare ≥ 0.999 on a large sample of real notes (the 5 above were
hand-written; long notes near the ~5.8K-character limit matter most), and an
over-long text fails the way it does now.

*Phase 1 measured and deployed 2026-11-02:*

| Check | Result |
|---|---|
| VRAM | E4B 3,656 + bge-m3 930 MiB (8192 batch: 1,142; through Ollama: 648-667) |
| E4B | `n_ctx_slot` 16384, 2 slots, 80 tok/s; 15.8k tokens fits, 17k returns 400 |
| bge-m3 speed | 32 ms a text over 546 notes; Ollama 120 ms |
| Same text, llama.cpp against Ollama | 546 real notes: lowest 0.99889, median 0.99981, 2 under 0.999, both notes over 3,000 characters |
| llama.cpp against stored vectors | Notes up to 3,000 characters: 338, lowest 0.99919. Over 3,000: 56 of 207 under 0.999, lowest 0.9318. Ollama scores the same on those (0.9318), so the stored vectors came from other text, not another engine |
| Retrieval | Before, on Ollama: recall@10 0.840, MRR 0.687. After: 0.880, 0.691. The old 0.92 / 0.74 no longer describes this graph. New notes were stored between the two runs |
| Over-long text | 5,000 tokens returns 500 "too large", the shape the shrinking retry expects. 4,000 dense characters is 4,003 tokens and fits |
| Nimble | Partial GPU does not fit: 15 layers alone wanted 4 GB. CPU, 4 threads: 2.4 s for 600 tokens, unchanged with 12 busy threads |
| Guardian | CPU, 4 threads: 1.8 s for 600 tokens, unchanged with 12 busy threads. This is not its real prompt; Phase 3 measures that |
| CPU models on the GPU | Each still held about 300 MiB of VRAM while loaded, not explained yet. Check in Phases 2 and 3 |

Nimble and Guardian stay on Ollama until Phases 2 and 3, so Ollama's Guardian still shares the GPU:
7,040 MiB in use right after deploy. Rollback: `LOCAL_EMBED_BACKEND=ollama`. The 12 busy threads
are the ML runner's `cpus: 12` of this machine's 32, not 12 of 16 as written above.

**Phase 2: the decision module.** Port Ollama's prompt building and scoring
(`decision/systemone.go`, `prompts.go`; MIT, keep the notice) to one Rust module
over `logprobs`; `api_tags`, the router checker and the alias judge call it. A
candidate outside `top_logprobs` must be an error, not 0.
*Done when:* on `eval/api_tags_golden.yaml` and stored alias verdicts, compared
with Ollama's answers, no tag crosses 0.7 and no verdict changes that was not read
and accepted.

**Phase 3: Guardian.** Render the `groundedness` prompt in Rust (only that criterion
of the 14 KB template) and send it to `/v1/completions`. Time it; 16 s against 3-5 s
is a real cost at this call volume. Measure before accepting.
*Done when:* on real claims, verdicts match Ollama's, and `unchecked` does not rise.

**Phase 4: engines and roles.** §4: the entries, roles, capability gate, `local`,
one client, `ollama_loop` and the Ollama provider gone or reduced to its entry.
Run the same evals with `openai` pointed at Ollama's OpenAI endpoint, to prove the
flag is real.

**Phase 5: install, then remove Ollama.** The installer and manifest. Test on a
fresh clone with no private bundle and no host Ollama (joins OUTSIDE_REVIEW Phase
1's fresh-clone test). Find a public source for each GGUF: Gemma's is known;
bge-m3 and the merged Nimble GGUF are not yet confirmed (both Apache-2.0). Update
scripts and the fine-tuning harness, which call Ollama directly. After a clean
week, the owner stops the host service; deleting its models is a separate step.

## 6. Found on the way: E4B's `json_schema`

Checked 2026-11-01: **not a live problem.** A `json_schema` request whose prompt does
not ask for JSON sends E4B into its thought channel: empty `content`, the text in
`reasoning_content`, `finish_reason: length`, despite `--reasoning off`. A system
message prevented it in the one case tried; `enable_thinking: false` did not. The
real picker prompt ends "Answer with JSON", and got a valid route on 8 of 8 test
turns. All 16 live turns in the 14 days to 11-01 were routed, with no picker warning
in 7 days of logs. If it did happen the turn runs unrouted, with a warning. A new
caller of `constrained` must ask for JSON in its prompt.

## 7. Decisions for the owner

All decided by the owner, 2026-11-01 unless dated.

1. **Where the Ollama Cloud load goes: Mistral, for now.** That makes a `may_train`
   provider the default for private work, not the overflow it was on 10-10; the
   owner chose this knowingly. Other candidates, or a bigger GPU, are later.
2. **Guardian moves too: yes.**
3. **Ship with llama.cpp; vLLM later.** Ollama is not a shipped engine. The flag and
   the bring-your-own entry stay, and Ollama's OpenAI endpoint is the Phase 4 test
   that the flag is real, not something we support.
4. **No local vision.** No workflow declares `vision` (checked 2026-11-01); E4B keeps
   `--no-mmproj`.

7. **GLM on `deep-diver` and `ml-student` (2026-11-02): yes, for now, and the
   `openrouter-paid` monthly cap goes from $1 to $3.** `deep-dive`, `ml-study`,
   `squares-study` and `playground-study` are `paid: true`. The pin is on the profiles
   because a refused profile pin steps down to Ministral, and a refused step pin kills
   the job. `intake` stays on Ministral. The cap is the ledger's figure, which runs high
   (about $0.56 against $0.17 real during the test).
   One real `deep-dive` on GLM cost $0.053 by the ledger. Those four workflows are about
   85 runs a month, so the ledger could reach $3 before the month ends; past the cap the
   agents step down to Ministral. Watch it for the first month.

## 8. Order

Phase 0 went live 2026-11-01 with OUTSIDE_REVIEW's Phase 0, because the post-commit
hook builds the working tree; its checks finish before the 9th. Then 1 → 2 → 3 → 4 → 5.
