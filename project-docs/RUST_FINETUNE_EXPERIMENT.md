# Experiment 1: Fine-Tune a Small Model to Write Better Rust

**Status: CONCLUDED 2026-09-13, negative result (decision "B").** Written 2026-09-12. Three
adapters were trained and fairly benchmarked; none improved the model. The recipe that avoids
harming it is documented. The brain was back online the same day. See **Conclusion** at the end.

> **Reusable lessons from this notebook** (process, silent-skew table,
> checklist for the next fine-tune) are distilled in
> [`FINE_TUNING_PLAYBOOK.md`](FINE_TUNING_PLAYBOOK.md). Keep both current.

The first concrete step of [`FINE_TUNING_PLAN.md`](FINE_TUNING_PLAN.md), run
**outside the brain**. No transcript capture, no graph ledger, no brain-driven
curation, just Python in `workspace/finetune/`, a Rust sandbox, a benchmark and
a before/after number. The point is to do one fine-tune end to end on this
hardware and surface what the big plan isn't considering yet.

The one brain component it builds is the **Rust sandbox**, because the brain
needs somewhere to compile and test Rust anyway.

---

## The question

> Does a QLoRA fine-tune on compiler-verified Rust solutions measurably improve
> a small local model's pass rate on held-out Rust exercises it has never seen?

A "no" is a valid, recorded result. A run without a base-model baseline under
identical conditions is not a result.

## Design at a glance

```
 Exercism Rust (106 practice exercises, MIT)
   ├── 30 in Aider polyglot  ──►  BENCHMARK (never trained on)
   └── 76 remaining          ──►  TRAINING POOL
                                      │
   teacher (Mistral free API) ─┐      ▼
   base model itself ──────────┼─► k samples/problem ─► rust-sandbox: cargo test
                               │                              │
                               └──────── keep only passing ◄──┘
                                              │
                                  QLoRA (trainer container, GPU)
                                              │
                         base vs base+adapter ─► same benchmark, same runtime, same quant
```

---

## Benchmark

**Primary: the 30 Rust exercises in [Aider's polyglot benchmark](https://github.com/Aider-AI/polyglot-benchmark)**
(`rust/exercises/practice/`). These are Exercism's hardest problems. Each has
`Cargo.toml`, a stub `src/lib.rs`, `tests/*.rs` and `.docs/` instructions.

- **Protocol (mirrors Aider):** the model gets the instructions + stub and
  writes the solution. If tests fail, it gets the test output once and tries
  again. Report **pass@1** (first attempt) and **pass@2** (after feedback).
  Run tests with `-- --include-ignored`, since Exercism marks all but the first
  test `#[ignore]`.
- **Decoding:** greedy (temperature 0) for the headline number; also run *n*
  sampled attempts per exercise to get a variance estimate.
- **Crates needed offline:** `time` (gigasecond), `anyhow` (grep), `rand`
  (simple-cipher), `pretty_assertions` (in the training pool).

**The weakness is size.** 30 problems means one problem is 3.3 points, so a
3–4 problem swing is within noise. If the result is marginal, add
**MultiPL-E HumanEval-rs** (161 problems, single-function, fast) for
statistical power before drawing a conclusion. Don't widen the benchmark
*after* seeing a result just to reach significance; decide the suites before
the tuned run.

## Harness: raw first

A standalone Python script (stdlib only, so nothing is installed on the host)
that:

1. calls the model directly: host Ollama (`/api/chat`), llama.cpp or Mistral
   (both OpenAI-compatible), selected by flag
2. executes through the **brain's** `execute_code(language: "rust")` tool over
   MCP at `localhost:3001`, so the benchmark also exercises the brain's real
   sandbox path
3. writes one JSON result per (model, exercise, attempt), with prompt, code,
   test output, pass/fail, tokens and latency, to `workspace/finetune/results/`

Running the brain's coding agent over the same benchmark is **Phase 6**, after
the raw A/B. Prompt, routing and tool noise would muddy a model comparison.

## Candidate base models

"Whichever is better at coding to begin with" is itself the first measurement.
The constraint is **QLoRA-trainable on 8 GB**, which realistically means ≤ ~4B
at useful sequence lengths (7–8B is a Phase 3 measurement, not an assumption).

| candidate | available | note |
|---|---|---|
| Gemma-4-E4B | Ollama `gemma4:e4b`; llama.cpp sidecar | already the brain's local model |
| Qwen3.5-4B | Ollama `qwen3.5:4b` | **thinking model**: emits into `thinking` and can return an empty `response` (CLAUDE.md), so disable thinking for the harness |
| Qwen3-8B | Ollama `qwen3:8b` | likely too big to train locally; baseline anyway as a reference point |
| a Qwen coder variant ≤ 7B | not pulled | pull if one fits; code-pretrained bases usually fine-tune better for code |

Pick the base on **pass@2 and trainability together**. A 7B that scores higher
but can't be trained here doesn't help.

## Training data: compiler-filtered sampling

1. For each of the **76** training-pool exercises, generate *k* solutions from
   the teacher and from the base model itself.
2. Run each through `rust-sandbox`. **Keep only solutions that pass every
   test** (including ignored ones).
3. Keep **repair traces** too: a failed attempt + its compiler/test output →
   a passing fix. This matches the benchmark's second attempt and is probably
   the more valuable example type.
4. Dedupe near-identical solutions per exercise.

The compiler is the grader. No model judges quality, which avoids the
self-grading trap. Expect a few hundred examples: enough for a LoRA to shift
behaviour, and small enough to overfit quickly. Watch the eval set, not the
training loss.

### Teacher: Mistral free Experiment plan

- ~1B tokens/month, rate-limited, phone verification. **Free-plan inputs and
  outputs may be used to train Mistral's models**, which the user has accepted
  for this experiment. **Put no brain data, keys or private code in teacher
  prompts**, only public Exercism problems.
- **Terms checked 2026-09-12:** the Commercial Terms of Service (§3.3) restrict
  using *image* outputs to train competing image products. The Additional
  Product Terms and Usage Policy contain no restriction on using text/code
  outputs for training. Third-party summaries claiming a blanket "no competing
  models" clause appear out of date. Re-check before any later experiment.
- Models: Mistral's current code models (Codestral / Devstral) via the
  OpenAI-compatible API.

### Contamination rules

- Exercise names in the benchmark set are excluded from every training prompt
  and every generated file, enforced by the data script rather than by hand.
- Base and teacher models may have memorised public Exercism solutions. That
  inflates absolute scores for everyone equally; the before/after delta is
  still meaningful.

## Training

- **Where:** a Python container with the NVIDIA runtime (already installed on
  the host). Nothing is installed on the host itself.
- **CUDA:** host driver 535 = CUDA 12.2. *Corrected 2026-09-12 by
  measurement:* PyTorch's `cu126` build (`pytorch/pytorch:2.9.1-cuda12.6-cudnn9-runtime`)
  runs on this driver via CUDA minor-version compatibility: CUDA available,
  sm_86 kernels, bf16, and a bitsandbytes 4-bit layer all work. The
  "≤ 12.2" rule belongs to the llama.cpp sidecar's upstream image, which
  bundles a forward-compat libcuda stub. It is not a general limit. The cu121
  builds stop at PyTorch 2.5.1, too old for the Qwen3.5/Gemma 4 architectures.
- **GPU window:** stop `llamacpp` and `whisper`, and unload Ollama models,
  before training. Serving uses ~4.8 GB of the 8.
- **Stack:** Unsloth vs TRL + PEFT is decided by what actually runs on cu121 on
  this card with the chosen base.
- **Output:** LoRA adapter → merged → GGUF at the **same quantization as the
  baseline** (Q4_K_M), served by the **same runtime**. Comparing a bf16 tuned
  model against a Q4 base would measure quantization, not training.
- **The real comparison is tuned vs *reconverted* base, not vs Ollama's
  model.** Ollama's published `qwen3.5:4b` was converted and quantized by
  Ollama. Our tuned model goes through llama.cpp `convert_hf_to_gguf` +
  `llama-quantize`. So the untuned base is exported through the *same*
  pipeline (`export` with no adapter → `…-base-reconverted`) and benchmarked
  too. Ollama's own model stays a sanity reference.
- **Prompt parity has two halves:**
  1. *Serving.* Ollama's qwen3.5 has no text template; it uses a built-in
     `RENDERER qwen3.5` / `PARSER qwen3.5` plus sampling `PARAMETER`s
     (`presence_penalty 1.5`) the harness doesn't override. An imported GGUF
     with a bare `FROM` would lose all of them, so `import_ollama.sh` copies
     every directive except FROM/LICENSE from the base.
  2. *Training.* Prompts are rendered with the Hugging Face template with
     `enable_thinking=false` (Qwen3.5 then emits an empty `<think></think>`).
     `check-template` checks this matches what Ollama sends with `think: false`,
     by comparing `/api/chat` against `/api/generate raw=true` on our rendering
     (same prompt token count and identical temperature-0 output).
- **Multimodal checkpoints:** Qwen3.5-4B (`Qwen3_5ForConditionalGeneration`)
  and Gemma-4-E4B (`Gemma4ForConditionalGeneration`) are vision/audio models.
  LoRA targets are discovered as the linear layers inside the language model's
  decoder layers, with vision/audio/projector/lm_head excluded, rather than a
  hardcoded q/k/v/o list that Qwen3.5's hybrid attention wouldn't match.
  llama.cpp `b10900`'s converter registers both architectures (and
  `Qwen3ForCausalLM`).

## Where things live

| what | where | versioned? |
|---|---|---|
| this doc | `project-docs/RUST_FINETUNE_EXPERIMENT.md` | yes |
| Rust sandbox | `rust-sandbox/` + `docker-compose.yml` + `ExecSkill` | yes |
| harness, data scripts, training scripts | `workspace/finetune/` | **no**: `workspace/` is gitignored. Decide whether to `git init` it |
| datasets, adapters, GGUFs, results | `workspace/finetune/{data,adapters,results}` | no (large); results summarised here |

---

## The Rust sandbox (`rust-sandbox` service)

Same isolation contract as the Python `sandbox`, with two measured differences.

**Isolation (unchanged from `sandbox`):** `sandbox-net` only (`internal: true`,
no egress), no `env_file`, no bind mounts, `read_only` root,
`cap_drop: ALL`, `no-new-privileges`. **No `ports:` mapping.**

**What differs, and why:**

- **The scratch tmpfs allows exec.** Rust compiles test binaries and build
  scripts and must run them. The Python sandbox's `noexec` was defence in
  depth; the boundary that matters is the network, which is unchanged.
- **Crates come from a read-only, pre-fetched cache.** `cargo fetch` runs at
  image build over a declarative manifest (`rust-sandbox/crates/Cargo.toml`);
  at run time cargo runs `--offline`. **Verified 2026-09-12:** a
  `rand` + `pretty_assertions` exercise compiled and passed from a read-only
  `CARGO_HOME` with `--network none` in ~2 s, **provided `TMPDIR` and `HOME`
  point into the writable scratch** (the linker otherwise fails with
  `Cannot create temporary file in ./: Read-only file system`).
- **Bigger ceilings:** `mem_limit: 4g` (tmpfs pages count against it),
  300 s max wall clock, no `RLIMIT_AS` (rustc/lld reserve large virtual address
  ranges, so the cgroup memory limit is the guard).
- **A dedicated non-host UID** (10001). `RLIMIT_NPROC` is counted per real UID
  across the whole kernel, so a container user sharing UID 1000 with the host
  user inherits the host user's process count. The cgroup `pids_limit` is the
  process guard instead.

**Adding a crate** = add it to `rust-sandbox/crates/Cargo.toml` and rebuild. A
submission that needs an uncached crate fails offline with cargo's own error,
and the server appends the list of crates that *are* available.

**Brain tool:** `execute_code` gains `language: "python" | "rust"`. Rust
accepts either `code` (a single `main.rs`, run with `cargo run`) or `files` (a
path → content map for a whole crate) plus `command`
(`test`/`check`/`clippy`/`run`/`build`) and `include_ignored`. Rust is only
offered when `RUST_SANDBOX_URL` is set. A failed build or failing test is a
**successful tool call** carrying the failure, the same rule as Python. The
result adds `tests_passed`, `tests_failed` and `compile_failed`.

---

## Phases

- [x] **Phase 1: Rust sandbox.** `rust-sandbox` service, `execute_code(language: "rust")`, verified from the brain over MCP against a real benchmark exercise. **Done 2026-09-12:**
  - **Directly against the sandbox:** `bowling` stub → builds, 0/31 pass; `bowling` reference solution → 31/31 in ~0.3 s; `simple-cipher` (needs `rand`) → 23/23 in 2.8 s offline; an uncached `tokio` dependency → cargo's error plus the available-crate list.
  - **Through the brain's `execute_code` over MCP:** Python default unchanged (`42`); Rust `files` + `include_ignored` → "Tests failed: 31 failed, 0 passed" / "Build succeeded; 31 tests passed."; Rust `code` → `55`. Every result was a successful tool call.
  - **Isolation:** no route to `1.1.1.1` or `index.crates.io` from inside; read-only root; no published ports.
- [x] **Phase 2: Harness + baselines.** Raw harness; benchmark every candidate base; pick the base.
  - **Harness built 2026-09-12** in `workspace/finetune/` (its own git repo; stdlib-only Python). Benchmark pinned to polyglot commit `7e0611e7`; training pool = exercism/rust `1d3a0f46`, 76 exercises after exclusion, overlap 0.
  - **Self-checks:** `stub` → 0/30 (7 compile failures, 23 test failures). `reference` → 24/30 pass@1, and the other **6 are dependency errors, not wrong answers**. Their Exercism reference solutions need crates the benchmark tells models not to use: alphametics `permutohedron`, decimal `num-bigint`, grep `thiserror` 1.x, pig-latin `regex` 0.2, poker `counter`, robot-name `rand` 0.3. They stay uncached. The first run labelled them `tests_failed`, because resolution fails before rustc so the sandbox's `compile_failed` is false. They're now their own `dependency_error` outcome, and the sandbox's available-crates hint covers the "failed to download … --offline" shape too.
  - **Every exercise lists `Cargo.toml` as a solution file**, so the model sees its manifest and may add dependencies (as in Aider).
  - **Smoke run (`qwen3.5:4b`, 3 exercises, not a baseline):**
    - **Pipeline:** works end to end. Files were extracted from every reply, `think: false` held (0 thinking chars), and no context was clipped (max 5267 tokens in, against `num_ctx` 16384).
    - **Feedback loop:** carries real signal. acronym went from 0/10 to 9/10 tests passing on the retry.
    - **Scores:** bowling improved only 2/31 → 3/31. macros failed to compile both times.
    - **Speed:** ~4.7 min for bowling alone on the shared GPU, so a full 30-exercise run per candidate is on the order of an hour. Schedule baselines, don't wait on them.
  - **Feedback cleanup changed before any baseline:** the smoke run's bowling feedback opened with an unused-variable *warning* while 28 failing tests sat behind it, and acronym's opened with an empty `running 0 tests … ok` section. Warning blocks and empty test sections are now stripped. This had to land before baselines, since changing feedback afterwards would invalidate the comparison.
  - **Second smoke run (bowling only) found three more skews, all fixed before baselines:**
    1. **Backtraces:** the sandbox set `RUST_BACKTRACE=1`, so each of 29 failing tests printed a stack trace. It's now `0` in `rust-sandbox/server.py`, and the harness strips any frames that do appear. Replayed feedback: 8188 → 4475 chars.
    2. **Per-test status lines and thread IDs:** `test x ... FAILED` × 31 duplicated the trailing `failures:` list, and `thread 'x' (5746)` differs every run. Both are stripped, so identical failures produce identical prompts.
    3. **`max_tokens` 4096 truncated a reply mid-way through its final code block.** Extraction then scored an earlier complete draft from the same reply, i.e. code the model had already revised. The default is now 8192, and any capped reply carries a warning.
  - **Takeaway so far:** the harness failures that matter are silent ones. Each of these produced a plausible score, not an error.
  - **Baselines started 2026-09-12 19:15 EDT** via `scripts/run_baselines.sh`, in order qwen3.5:4b → qwen3:8b → gemma4:e4b. Each is a full 30-exercise run with the harness defaults (greedy, seed 7, `num_ctx` 16384, `max_tokens` 8192, `think` off, 2 tries). **Brain rested for the window:** scheduler paused (`scheduler_control stop`), `llamacpp` and `whisper` stopped, and Ollama emptied before each model loads. The GPU was at 405 MiB before the first load, and qwen3.5:4b loaded fully resident (3.7 of 3.7 GB). The brain container and `rust-sandbox` stay up, since tests run through `execute_code`. (Measured afterwards: gemma4:e4b loaded fully resident at 3.3 GB; the 9.6 GB tag includes vision/audio weights not loaded for text.) **Restore afterwards:** `docker compose start llamacpp whisper`, then `scheduler_control start`.
- [x] **Phase 3: Trainability.** Trainer container; measure VRAM/time for QLoRA on the chosen base at realistic sequence length.
  - **Scripts written 2026-09-12** (`workspace/finetune/trainer/`, `scripts/train.sh`, `scripts/import_ollama.sh`); the image builds. The commands are `check-env`, `inspect` (token lengths; over-limit examples are dropped, never truncated), `check-template`, `train` (QLoRA, loss on the last answer only, eval loss before and after, peak VRAM per logged step), and `export` (merge → GGUF Q4_K_M, or base-only for the reconverted baseline).
  - **Still to measure, on the GPU once baselines finish:** `check-env` inside the image, `check-template` per candidate, and a `--max-steps 10` smoke run for peak VRAM at the chosen `--max-seq-len`.
  - **Open question for Qwen3.5:** its hybrid linear-attention layers may run on a slow pure-PyTorch fallback without optional kernels. The smoke run's step time will show whether that matters.
  - **`check-env` passed in the image (2026-09-12):** torch 2.9.1+cu126, sm_86, bf16, bitsandbytes 4-bit, 7.4 GB free.
  - **Gemma 4 prompt parity VERIFIED (2026-09-12).** The HF template for `google/gemma-4-E4B-it` with `enable_thinking=false` renders `<bos><|turn>system\n…<turn|>\n<|turn>user\n…<turn|>\n<|turn>model\n`, with no `<|think|>` marker (thinking on inserts `<|think|>` at the top of the system turn). Against Ollama's `RENDERER gemma4` with `think: false`: identical prompt token counts (970/970/970), and with logprobs requested, identical tokens for 80 positions with **zero** logprob difference.
  - **PEFT's `prepare_model_for_kbit_training` upcasts every non-4-bit parameter to fp32** (read from peft 0.20.0's source: any fp16/bf16 param whose class isn't `Params4bit` → `float32`). On Gemma-4-E4B that includes the large unquantized per-layer embedding tables, at 4 bytes/param instead of 2. `train.py --keep-bf16` does the rest of that preparation (freeze, gradient checkpointing, input grads) without the upcast. `plan-memory` estimates both footprints from the meta-device module tree, and the smoke sweep measures both.
  - **Gemma-4-E4B does NOT fit as loaded by default; offloading its per-layer embeddings to the CPU makes it fit (estimated 2026-09-12, `plan-memory` on the meta device).** 8.61 B parameters, broken down:
    - language linear layers: 3.97 B, quantized to 4-bit
    - embeddings: 3.49 B, NOT quantizable by bitsandbytes. **2.82 B of that is `embed_tokens_per_layer` alone** (~5.25 GiB in bf16).
    - vision/audio towers: 0.48 B
    - `lm_head` is tied to `embed_tokens`. The first estimate double-counted it (10.19 GiB) because `from_config` doesn't tie weights; fixed by calling `tie_weights()` before counting.

    | configuration | static VRAM | left of 7.8 GiB |
    |---|---|---|
    | all on GPU, non-quantized params bf16 | 8.94 GiB | −1.14 (doesn't fit) |
    | all on GPU, PEFT's fp32 upcast | 15.47 GiB | −7.67 (doesn't fit) |
    | `embed_tokens_per_layer` on CPU, fp32 upcast | 4.97 GiB | 2.83 for activations |
    | **`embed_tokens_per_layer` on CPU, `--keep-bf16`** | **3.69 GiB** | **4.11 for activations** |

    The per-layer table is a frozen lookup, so on the CPU each step only transfers small per-token vectors and needs no gradients. That's the same trick that lets Ollama/llama.cpp serve E4B in 3.3 GB. `train.py --cpu-modules model.language_model.embed_tokens_per_layer` builds an explicit device map (`llm_int8_enable_fp32_cpu_offload` for 4-bit). Verified on the meta tree: 12 map entries, only that module on the CPU, every parameter covered, and an unknown module name is rejected. **The smoke sweep has to confirm the estimate, including step time with the CPU lookup.**
  - **LoRA targets on Gemma-4-E4B (discovered, meta device):** 342 layers. q/o/gate/up/down × 42, k/v × 24 (shared-KV layers have none), plus Gemma's `per_layer_input_gate` and `per_layer_projection` × 42. No vision/audio layers. 38.7 M LoRA params at r=16.
  - **Teacher sampling finished (2026-09-12, `codestral-2508`, T=0.7, 4 samples × 74 exercises = 296 episodes):**
    - **Outcomes:** 241 pass (**200 first try, 41 repaired on retry**), 36 tests_failed, 18 compile_failed, 1 dependency_error. 0 infra errors.
    - **Coverage:** 69 of 74 exercises solved at least once. Never solved: bottle-song, circular-buffer, nth-prime, pov, tournament.
  - **`rust-sft-v1` (the first real dataset):**
    - **Train:** 213 examples from 62 exercises (176 solve, 37 repair).
    - **Valid:** 20 examples from 6 held-out exercises (17 solve, 3 repair).
    - **Rejections:** 44 duplicate solves, 1 duplicate repair, 4 added_dependency, plus the failed episodes. Nothing capped.
    - **Duplicates are the notable number:** at T=0.7, Codestral reproduced an identical solution 44 times across 4 samples, so the effective diversity is well below 4 per exercise. Raising temperature, or adding a second sampler (the base model itself), are the levers if the data proves too thin.
  - **smoke-v0** (built from the first ~70 Codestral episodes): 77 train / 7 valid, 57 exercises, 13 repair traces. The build rejected 8 `tests_failed`, 6 `compile_failed` and 1 `added_dependency`. **Tokens under the Gemma tokenizer:** p50 1187, p95 3200, max 6908 (a repair trace). A 4096 cap drops 2 of 77, 6144 drops 1, 8192 keeps all. Answers (the trained span) are at most 1048 tokens.
  - **First smoke run (2026-09-12 23:13): OOM on the first training step at 2048 tokens, and the diagnosis matters more than the failure.**
    - **What worked:** the model loaded (3.5 GiB allocated), LoRA found its 342 targets, and 38.7 M params were trainable.
    - **What failed:** the first step died with **"Tried to allocate 5.25 GiB"**, exactly the size of `embed_tokens_per_layer` in bf16. The table was **not** staying on the CPU.
    - **Why:** accelerate's CPU offload (`AlignDevicesHook`, measured `execution_device: 0, offload: True`) keeps weights on the CPU between calls, but **copies them to the GPU for every forward**. The parameter was a meta placeholder, with the real bf16 table in the hook's weights map. Offload saves memory at rest, not during compute. The `plan-memory` estimate assumed the table never enters VRAM, and that assumption was wrong for this mechanism.
    - **Ruled out:** passing precomputed `per_layer_inputs` (the text model raises `ValueError` if given together with `input_ids`).
    - **Fix 1: run the lookup on the CPU.** `modeling.execute_on_cpu` swaps the module for a `CpuEmbedding` that holds the real weights (taken from the hook's weights map), does `embedding(ids) * scalar_embed_scale` **on the CPU**, and returns the result on the GPU (~44 MB for 2048 tokens). The model's own `get_per_layer_inputs` and projection code runs unchanged.
    - **Fix 2: the next OOM, avoided in advance.** The vocabulary is **262,144** tokens, and the model's loss builds full-sequence logits (2048 × 262144), upcasts them to fp32 and keeps a same-sized gradient. `trainer/loss.py` runs the backbone once, keeps only the answer positions (the next token is in the answer), and does lm_head → softcap (30.0, same order as Gemma 4's forward) → cross-entropy in checkpointed 256-position chunks. It normalizes by the Trainer's `num_items_in_batch`. Evaluation is loss-only.
    - **Checks before the re-run:** the chunked loss must equal the naive full-logits loss and its gradients (numerical test), and peak VRAM on a real forward+backward must be measured.
    - **Numerical checks PASSED (float64, CPU):**
      - **Loss:** chunked answer-only loss vs naive full-logits loss: **0.0 difference in value**, gradients within 2.2e-16 (hidden) and 3.1e-15 (weight). Checked with and without softcap 30.0, on a batch of 2 with different prompt lengths and a masked gap, chunk size 7 < sequence 37. Token counts match (37 / 37).
      - **Embedding:** `CpuEmbedding` output is **bit-identical** (`torch.equal`) to `Gemma4TextScaledWordEmbedding` in bf16, running on the CPU with no grad.
  - **Smoke sweep RESULT (2026-09-12 ~23:20 EDT, both fixes, `--cpu-modules embed_tokens_per_layer --keep-bf16`, 3 steps each on the 3 longest examples that fit, grad-accum 1):**

    | max_seq_len | longest example | static VRAM | peak VRAM | s/step | result |
    |---|---|---|---|---|---|
    | 2048 | 2025 | 3644 MiB | 5301 MiB | 5.0 | ok |
    | **4096** | 3879 | 3644 MiB | **6202 MiB** | 8.3 | **ok** |
    | 8192 | 6908 | n/a | 6749 MiB at failure | n/a | OOM (train) |

    - **Static footprint 3644 MiB matches the `plan-memory` estimate (3.69 GiB)** once the lookup really runs on the CPU.
    - **Activations:** ~0.5 MiB per token between the two passing lengths.
    - **Chosen `MAX_SEQ_LEN=4096`.** It's measured, with ~1.5 GiB headroom. 6144 would put a ~5,500-token example near 7 GiB on a 7.8 GiB card, too close to risk an OOM mid-run.
    - **Cost of the cap:** 7 of 213 `rust-sft-v1` train examples are dropped (3.3%), and 0 of 20 valid. The dropped ones are the longest, so disproportionately **repair traces**, the example type most relevant to the student. Worth revisiting (a 6144 smoke, `expandable_segments`, or gradient checkpointing tweaks) if v1 shows repair behaviour is what's missing.
    - **Smoke loss** was ~0.09 on the longest examples' answer tokens (low, plausible for code the base model already finds likely).
  - **Training v1 STARTED 2026-09-12 ~23:21 EDT:** `rust-sft-v1` (Codestral, 213 train / 20 valid), Gemma-4-E4B rev `ee0ef602`, QLoRA r=16 α=32 on 342 layers, lr 2e-4 cosine, 2 epochs, batch 1 × grad-accum 8, `MAX_SEQ_LEN=4096`, per-layer table on CPU, bf16 non-quantized params, answer-only chunked loss. Initial validation loss (= base model) is measured before training.
    - **Used:** 206 train examples (7 dropped over 4096) and 20 valid (0 dropped).
    - **Initial validation loss (base model, answer tokens only): 0.1227** (20 examples, 18.7 s).
    - **Scale:** 206 examples ÷ (1 × 8) ≈ 26 optimizer steps per epoch, ~52 total.
  - **Training v1 FINISHED (2026-09-12 23:42 EDT, the adapter's file timestamp).** *(Corrected: first recorded as "2026-09-13 ~00:03", a mix-up between the run summary's UTC timestamps and local time.)*
    - **Validation loss 0.1227 → 0.0571 (−53%)** on the 6 held-out exercises.
    - Training loss 0.157 (step 5) → 0.036 (step 50), averaged over 5 steps.
    - Gradient norm 0.56 → ~0.13; the learning rate followed the cosine schedule to ~0.
    - 52 optimizer steps in **20.3 min (23.5 s/step)**. Peak VRAM **6303 MiB**, static 3652 MiB, no OOM. Adapter: 155 MB safetensors.

    | step | 0 | 5 | 10 | 15 | 20 | 25 | 30 | 35 | 40 | 45 | 50 | 52 |
    |---|---|---|---|---|---|---|---|---|---|---|---|---|
    | train loss |   | .157 | .106 | .088 | .059 | .066 | .045 | .050 | .028 | .045 | .036 |   |
    | eval loss | .1227 |   |   |   |   |   |   |   |   |   |   | .0571 |

    - **What this does and doesn't show:** the validation split is by *exercise*, so the loss fell on problems never trained on. It's not just memorization of training problems. But the loss measures how well the model predicts **Codestral's** solutions (the teacher it was trained on), and Codestral is not stronger than the student on the hard set. Lower loss means "writes more like Codestral", not "solves more problems". **Only the benchmark comparison against the reconverted base answers the question.**
  - **Base export + import WORKED (2026-09-12 ~23:47 EDT):**
    - **Conversion:** `convert_hf_to_gguf` recognized `Gemma4ForConditionalGeneration` and wrote `per_layer_token_embd` (10752 × 262144) and `token_embd` in BF16 before quantizing. The bf16 merge/load peaked at ~13 GB host RAM.
    - **Output:** `gemma4-e4b-base-reconverted-Q4_K_M.gguf` is **4.94 GiB** (llama.cpp `b10900`).
    - **Import:** `ollama create` succeeded, and the Modelfile inherited `TEMPLATE {{ .Prompt }}`, `RENDERER gemma4`, `PARSER gemma4` and the base's `PARAMETER temperature 1 / top_k 64 / top_p 0.95`.
    - **First benchmark reply** (polyglot `accumulate`) is well-formed Rust in the canonical file-block format: 507 prompt tokens, 106 output, `finish_reason: stop`, no thinking.
    - **Serving parity CONFIRMED:** `tokens_in` for identical prompts matches Ollama's own `gemma4:e4b` exactly (accumulate 507 = 507, acronym 392 = 392), so the inherited `RENDERER gemma4` renders the same prompt.
    - **Resident size:** 2.94 GiB, fully on GPU. `ollama show` reports **7.5B params vs 8.0B** for Ollama's build, consistent with a text-only conversion that omits the vision/audio towers.
    - **Why the reconverted base was necessary, observed directly:** on `acronym`, Ollama's published build gave `compile_failed` and the reconverted base gave `tests_failed`, from the **same prompt** (same token count). Conversion and quantization alone change answers. Scoring the tuned model against Ollama's own build would have credited (or blamed) training for that difference.
  - **RESULT v1 vs reconverted base (2026-09-13): NEGATIVE. Tuning made the model worse.**

    | benchmark | reconverted base | v1 (tuned on rust-sft-v1) | paired |
    |---|---|---|---|
    | polyglot-rust | pass@1 6.7%, pass@2 **6.7% (2/30)** | pass@1 6.7%, pass@2 **6.7% (2/30)**, same two | p = 1.0 |
    | humaneval-rs | pass@1 72.4%, pass@2 **85.3% (133/156)** | pass@1 60.9%, pass@2 **68.0% (106/156)** | **31 lost, 4 gained, p < 0.001** |

    - **Final outcomes, polyglot:** base 2 pass / 17 compile_failed / 10 tests_failed / 1 timeout. v1 2 / 14 / 10, **+2 dependency_error, +2 no_code, 4 replies truncated at max_tokens** (the base had 0).
    - **Final outcomes, HumanEval-rs:** base 133 / 4 compile_failed / 19 tests_failed. v1 106 / **17 compile_failed** / 33 tests_failed.
    - **Output volume halved:** v1 wrote 62K tokens on HumanEval-rs vs the base's 116K, and 76K vs 139K on polyglot.
    - **The validation loss said the opposite (−53%).** It measured imitation of Codestral's solutions on held-out Exercism problems and could not see a regression on a different task. **Loss on teacher data is not evidence of improvement**; only the benchmark against the reconverted base is.
    - **Side observation:** the reconverted base (llama.cpp Q4_K_M) scored below Ollama's published build on both benchmarks (polyglot 2 vs 5, HumanEval-rs 133 vs 137), from identical prompts. Conversion and quantization cost a little on their own, which is exactly why tuned is compared against reconverted.
    - **Diagnosis in progress** before any v2 training (v2 on the same recipe would likely regress too). Competing explanations:
      1. canonical bare-code replies removed the prose the model reasons with
      2. forgetting (lr 2e-4 × 2 epochs on 206 narrow examples)
      3. broken stopping (turn-end token), suggested by the truncations and no_code replies
      4. merge/quantization mismatch (adapter learned on NF4, merged into bf16, requantized to Q4_K_M)
    - **Diagnosis from the saved replies (no GPU):**
      - **(1) Lost prose: REJECTED.** The base doesn't reason in prose before code either. On the 31 problems v1 lost, *both* start with only the file label (median 15 chars before the first fence).
      - **(3) Broken stopping: REJECTED as the cause.** All 31 lost HumanEval replies ended `finish_reason: stop`. The polyglot truncations are **degenerate repetition loops**, not a missing turn-end after a finished answer: `say` spirals into `encode_chunk_small_digits_digits_digits…`, and `wordy` repeats one `else if parts[i] == "plus"` block until 8192 tokens. Degeneration is a symptom of damage, pointing at (2) or (4).
      - **What actually changed is STYLE:** median reply 1680 → 734 chars on the lost problems. The base writes explicit helper functions; v1 writes terse iterator chains (Codestral's style), and 11 of the 31 losses are compile failures, typically `E0308 mismatched types` against HumanEval's `isize` signatures. **v1 imitated the teacher's style without its skill.**
      - **Still open: (2) training vs (4) export.** This matters beyond v1: if (4), every adapter from this pipeline is suspect. Test: serve the model **as trained** (NF4 base + LoRA in transformers, no merge/GGUF) vs the NF4 base, on the same problems (`scripts/nf4_check.sh`, server `trainer/serve_openai.py`). The set is the 35 problems that flipped (31 lost + 4 gained) plus 15 seeded-random problems both passed.
        - NF4+adapter reproduces the losses → training did it; fix the recipe (lower lr / 1 epoch; the student's own verified solutions, option 2, directly counter style drift).
        - It doesn't → the export did; fix merging before any v2.
      - **NF4 check running (started 2026-09-13):**
        - **Server:** `trainer/serve_openai.py` loaded the NF4 base at 3,497 MiB, with the per-layer table executing on the CPU. It stops on EOS (id 1) or `<turn|>` (id 106).
        - **Speed:** ~13 tok/s in transformers (the first reply was 629 tokens in 49 s), so each 50-problem pass is roughly an hour.
        - **Prompt parity with the GGUF runs is exact:** `HumanEval_104_unique_digits` renders to **410 prompt tokens** in the Ollama reconverted-base run, the Ollama v1 run, and the NF4 server alike. NF4 results are directly comparable to the GGUF ones.
        - **Problem set** (`results/nf4-v1-problem-set.json`): the 31 lost, the 4 gained, and 15 both-passed controls (seed 20260913). `max_tokens` is 2048 for both NF4 runs.
      - **NF4 base pass finished:** 41/50 (pass@1 72%, pass@2 82%), 0 infra errors, 5 replies truncated at 2048.

        | group | GGUF base passed | NF4 base passed |
        |---|---|---|
        | lost by v1 (31) | 31 | 24 |
        | gained by v1 (4) | 0 | 3 |
        | controls (15) | 15 | 14 |

        The **same model in two quantizations disagrees on 11 of 50**, partly because the set was selected on the GGUF base's outcomes. That's why the NF4 test compares NF4 base with NF4 + adapter, never NF4 with GGUF.
      - **NF4 + adapter pass STALLED (stopped 2026-09-13 09:0x EDT after ~6 h, 12/50, all `model_error`).**
        - **Symptom:** every request hit the harness's 900 s timeout (`Connection timed out`). The server was not deadlocked: it answered late, and the log shows `BrokenPipeError` writing to a harness that had already given up.
        - **Diagnosis:** the server ran at **727% CPU, GPU 7%, 15.5 GB RSS**, so it was computing on the CPU. The base server (same model, same `CpuEmbedding`) ran at a normal ~13 tok/s, and training (LoRA attached with `get_peft_model`) at 23 s/step. The only difference is **`PeftModel.from_pretrained`** on a model whose `hf_device_map` still lists a `"cpu"` entry. **Cause CONFIRMED from peft 0.20.0's source:** `PeftModel.load_adapter` checks `hf_device_map` for `"cpu"`/`"disk"` values and, if present, calls `infer_auto_device_map` + `dispatch_model`, re-planning placement for the whole model. The map still said `"cpu"` for `embed_tokens_per_layer` after `execute_on_cpu` had replaced it, so loading the adapter re-dispatched the model and put compute on the CPU.
        - **Fix:** `serve_openai.attach_adapter` attaches the adapter exactly as training did (`get_peft_model` + `set_peft_model_state_dict`, no dispatch) and **raises unless every LoRA tensor loads** (0 missing, 0 unexpected).
        - **Stall alarm:** `harness.run --max-consecutive-errors` (default 3) aborts a run whose server is failing instead of timing out through every item.
        - **Fix verified live (2026-09-13 ~09:15 EDT):**
          - **Complete load:** "684 tensors loaded into 684 LoRA parameters" (342 layers × A/B), 3644 MiB on GPU.
          - **Placement back to normal:** container **CPU 85% (was 727%), RSS 1.3 GiB (was 15.5 GiB)**, GPU doing the work (34% util on a tiny request).
          - **Output:** a correct, cleanly stopped reply at ~7 tok/s on a 34-token answer (short requests understate throughput; an unmerged LoRA is also slower than the base).
          - The 15.5 GiB RSS confirms the re-dispatch had pulled weights into CPU RAM and computed there.
        - **Adapter pass re-running** with `SKIP_BASE=1`: the base pass is kept, and the 12 errored items are retried.
      - **NF4 check RESULT (2026-09-13 ~10:15 EDT): training is the primary cause of the regression.**

        | group (n) | GGUF base | GGUF v1 | NF4 base | NF4 + adapter |
        |---|---|---|---|---|
        | lost by v1 (31) | 31 | 0 | 24 | **19** |
        | gained by v1 (4) | 0 | 4 | 3 | 3 |
        | controls (15) | 15 | 15 | 14 | **12** |
        | **total (50)** | 46 | 19 | **41** | **34** |

        - **The adapter degrades the model with no export involved:** NF4 base 41 → NF4 + adapter 34 (11 lost / 4 gained). The drop appears on the lost set (24 → 19) *and* the controls (14 → 12). **p = 0.118 on its own**, since 50 problems is small, but the same direction as the full 156-problem GGUF regression (p < 0.001).
        - **The style shift reproduces without the export:** output tokens halved under NF4 too (46.6K → 22.7K), matching GGUF (116K → 62K). The terse Codestral style is a *training* effect, not a merge or quantization artifact.
        - **Clean runs:** NF4 adapter pass@1 60.0%, pass@2 68.0%, 0 infra errors, 1 truncated reply.
        - **Limits of this test:** the lost set was selected on GGUF outcomes, so GGUF v1 scores 0 there by construction, and the magnitudes can't be cleanly split between training and export. The export isn't proven harmless, but training is sufficient to explain the regression, in direction and in style.
        - **Consequence for v2 (a changed recipe, not a repeat):**
          - **Anchor the student's style** with gemma4:e4b's own compiler-verified solutions (rejection sampling), started immediately.
          - **Stronger teachers:** gpt-oss:120b + gemma4:31b, not Codestral alone.
          - **Gentler training:** a lower learning rate and one epoch instead of 2e-4 × 2 epochs.
          - **Check drift against the reconverted base on HumanEval-rs** before trusting any loss curve.
      - **Self-sampling interrupted and resumed (2026-09-13):**
        - The session's background-task monitor killed the run at 76/296 episodes ("low on memory").
        - **No kernel OOM**: memory was 20 GB used / 57 GB available afterwards. The largest process was Ollama's `llama-server` for gemma4:e4b at 9.5 GB RSS (the per-layer table sits in host RAM). The stop was a transient spike, not exhaustion.
        - **No data lost:** all 76 episode files were readable, with no partial `.tmp` files (writes are atomic).
        - **Resumed** with the same command; `generate` skips finished episodes.
      - **Self-sampling pass 0 (gemma4:e4b, T=0.8, 74 exercises): 59% pass** (44/74: 32 first try, **12 repaired**; 18 tests_failed, 12 compile_failed).

        | sampler | pass-0 pass rate |
        |---|---|
        | gemma4:e4b (student) | **59%** |
        | codestral-2508 | 84% |
        | gemma4:31b-cloud | 93% |
        | gpt-oss:120b-cloud | 95% |

        The student contributes the fewest verified solutions, but they're **in its own style**, which is the point of the anchor. Four passes should give well over a hundred examples after dedupe, next to the teachers' ~730.
      - **Self-sampling FINISHED (2026-09-13 ~12:30 EDT):** 296 episodes, **178 pass (60%)**: 139 first try, **39 repaired**. 73 tests_failed, 45 compile_failed, **0 errors**. 55/74 exercises solved at least once.
      - **`rust-sft-v2` built** (Codestral + gpt-oss:120b + gemma4:31b + gemma4:e4b self, `--max-per-exercise 16`):
        - **Train:** **909 examples** (763 solve / **146 repair**), 67 exercises. By sampler: gpt-oss 273, gemma31b 244, Codestral 213, **self 179**.
        - **Valid:** 83 examples on the 6 held-out exercises (74 solve / 9 repair; self 15).
        - **Rejections:** 140 tests_failed, 79 compile_failed, 123 duplicate solves, 6 duplicate repairs, 4 added_dependency, 1 dependency_error. **0 capped out.**
        - **Gemma tokens:** p50 1313, p95 3286, max 7405. **23 of 909 train examples over 4096 are dropped (2.5%)**; valid 0.
        - **Mix caveat:** the student's own solutions are 20% of training examples, so teacher style still dominates by volume. The anchor is partial. If v2 still drifts, the next lever is weighting (cap teacher samples per exercise, or oversample self).
      - **Training v2 STARTED (2026-09-13 ~12:35 EDT):** 886 train examples used (23 dropped over 4096), 83 valid, **111 optimizer steps**, GPU 6649 MiB at 100%. **Initial validation loss (base model) 0.3258.** That's *not* comparable to v1's 0.1227: v2's validation set is 83 examples from four samplers, including gpt-oss/gemma31b solutions the base finds less predictable.
      - **Training v2 FINISHED (2026-09-13 13:26 EDT):**
        - **Validation loss 0.3258 → 0.1986 (−39%).**
        - 111 steps in **48.3 min (26.1 s/step)**. Peak VRAM **6433 MiB**, static 3652 MiB, no OOM.
        - **Loss curve:** train loss 0.378 at step 10, then a plateau around 0.18–0.26 through step 110. Gradient norm 0.89 → ~0.32.

        | step | 0 | 10 | 20 | 30 | 40 | 50 | 60 | 70 | 80 | 90 | 100 | 110 | 111 |
        |---|---|---|---|---|---|---|---|---|---|---|---|---|---|
        | train loss |   | .378 | .192 | .202 | .195 | .180 | .237 | .212 | .265 | .227 | .179 | .199 |   |
        | eval loss | .3258 |   |   |   |   |   |   |   |   |   |   |   | .1986 |

        - **Compared with v1:** a smaller relative drop (−39% vs −53%) and a plateau instead of v1's slide to ~0.03. Consistent with a gentler update and less style imitation, **but v1 also looked good on loss and regressed. Only the benchmarks decide.**
      - **Next (chained):** `export v2` → `bench v2` (polyglot + HumanEval-rs), compared against the existing reconverted-base runs (polyglot 2/30, HumanEval-rs 133/156).
      - **The export chain was killed by the session's "low memory" monitor a second time (2026-09-13 ~13:30 EDT).**
        - **What survived:** the export runs in a `docker run` container, and killing the host-side script did **not** stop it. The container kept writing the bf16 GGUF and was left to finish.
        - **What was lost:** the rest of the chain (import, benchmarks) died with the script. No v2 benchmark had started, and Ollama had nothing half-imported.
        - **Not real exhaustion:** 13 GB used and **65 GB available** right after, with the export's Python processes at ~1–2 GB RSS and ~53 GB of page cache from writing 15 GB files. No kernel OOM kill either time. The monitor appears to react to I/O-heavy page-cache growth or brief spikes, not to genuine memory pressure.
        - **Handling:** let the orphaned export finish, then run import + benchmarks as a separate job.
        - **Lesson for long pipelines:** steps inside containers can outlive their launcher, so after any kill, **check for orphaned containers before re-running** a step. A duplicate export would write the same files concurrently.
        - **Outcome:** the orphaned export **completed cleanly at 13:28 EDT**. `gemma4-e4b-rust-v2-Q4_K_M.gguf` is 4.94 GiB (the same size as the base and v1 exports), `export.json` records adapter run `gemma4-e4b-rust-v2` / dataset `rust-sft-v2`, and the intermediates were removed. Memory afterwards: 66 GB available.
      - **RESULT v2 (2026-09-13 ~15:00 EDT): smaller regression than v1, still a regression, and no gain on the hard set.**

        | benchmark | reconverted base | v1 (Codestral, lr 2e-4 × 2 ep) | v2 (4 samplers incl. self, lr 5e-5 × 1 ep) |
        |---|---|---|---|
        | polyglot-rust pass@2 | 6.7% (2/30) | 6.7% (2/30) | **3.3% (1/30)** |
        | humaneval-rs pass@1 | 72.4% | 60.9% | **68.6%** |
        | humaneval-rs pass@2 | **85.3% (133)** | 68.0% (106) | **76.3% (119)** |
        | humaneval-rs output tokens | 116K | 62K | 67K |

        - **v2 vs base, HumanEval-rs:** 22 lost / 8 gained, **p = 0.016**, still a significant regression.
        - **v2 vs v1, HumanEval-rs:** v2 recovered 22 problems v1 lost, **p = 0.029**. The gentler recipe **halved** the damage (−14 vs −27 problems) but did not remove it.
        - **Polyglot:** v2 lost two-bucket (p = 1.0, noise). **Neither adapter improved the hard benchmark.**
        - **Style drift persists at reduced size:** output volume is still ~58% of the base's. Compile failures: base 4, v1 17, v2 10.
        - All runs clean: 0 infra errors, 0 truncated replies in v2, prompt parity confirmed (507 = 507 = 507).
      - **Experiment 1 conclusion so far:** on this 4B-effective model, **distilling compiler-verified Rust solutions made the model worse**, less so with a gentler recipe. The best-supported cause is data style: 80% of v2's examples were teacher-written (gpt-oss 273, gemma31b 244, Codestral 213 vs self 179), and v1's diagnosis showed the regression tracks style imitation. Other open factors: the training pool's Exercism prompt shape (`src/lib.rs` + Cargo) differs from HumanEval-rs's (`src/solution.rs`, doc-comment spec); the pool problems are easier than the hard set; the NF4 → merge → Q4_K_M export was not fully exonerated in magnitude.
      - **Decision point (asked the user before more GPU time).** The cheapest decisive next test is a **self-only adapter** (rejection sampling / STaR: only gemma4:e4b's own 178 verified solutions, gentle recipe). If it holds HumanEval-rs at the base, teacher style is confirmed as the damage; if it still regresses, the cause is the training setup itself (prompt shape, LoRA scope, or export).
      - **Decision (user, 2026-09-13): v3 combines "self-only data" and "change the setup".**
        - **Data:** `rust-sft-v3-self`, **only gemma4:e4b's own verified solutions**: 184 train (147 solve / 37 repair, 51 exercises) + 15 valid (4 exercises).
        - **Setup:** LoRA **r=8** (half of v1/v2), lr 5e-5, up to 2 epochs (~23 steps/epoch).
        - **Drift guard:** evaluate every 5 steps on a validation set **and an anchor set**, the base model's own greedy verified solutions on **80 MBPP-rs problems** (MultiPL-E `mbpp-rs`, 354 problems, same crate format as HumanEval-rs, same pinned revision). Training stops when anchor loss exceeds its untrained baseline by >3%, and the exported adapter is the **last checkpoint still within tolerance** (`trainer/guard.py`, unit-tested).
        - **Not done, on purpose:** matching the *training prompts* to the HumanEval-rs format, and picking checkpoints on HumanEval-rs. Both would tune to the regression test and invalidate it. The anchor set gets the drift detection and the matching format from a *different* problem set. HumanEval-rs and polyglot stay final scores only.
        - **Attribution caveat (recorded before the result):** v3 changes data, LoRA rank, epochs and early stopping at once. A good v3 shows *the combination* avoids the regression, not which part does it. A follow-up would need single-variable ablations.
      - **Anchor set built (2026-09-13 ~14:30 EDT):** gemma4:e4b (greedy, baseline settings) solved **63 of 80** MBPP-rs problems → `anchor-mbpp-rs` = 63 of its own verified solutions (15 tests_failed, 2 compile_failed rejected). 0 contamination flags vs polyglot/HumanEval-rs.
      - **Training v3 FINISHED (2026-09-13 14:31 → 14:57 EDT).** *(The promised "training started" message wasn't sent: the chain went from anchor build straight into training, and the check-in came after it finished.)*
        - 181 train (3 dropped over 4096), 15 valid, 63 anchor. LoRA r=8 → **19.3 M trainable params** (half of v1/v2). 46 steps (2 epochs), 25 min (32.4 s/step incl. evals every 5 steps). Peak VRAM **6178 MiB**. Adapter 77 MB.

        | step | 0 | 5 | 10 | 15 | 20 | 25 | 30 | 35 | 40 | 45 | 46 |
        |---|---|---|---|---|---|---|---|---|---|---|---|
        | valid loss | .1796 | .1766 | .1733 | .1697 | .1661 | .1633 | .1617 | .1605 | .1603 | .1601 | **.1603** |
        | anchor loss | **.1647** | .1518 | .1338 | .1275 | .1246 | .1226 | .1219 | .1215 | .1213 | .1215 | **.1213** |

        - **Drift guard:** limit 0.1697 (baseline × 1.03). Anchor loss **fell 26%** instead of rising, so the guard never fired, there was no early stop, and **checkpoint-46 (final) was exported**.
        - **Read this carefully:** training on the model's *own* verified outputs is expected to *lower* loss on its own style (it sharpens the distribution it already has). So the falling anchor loss shows **v3 did not drift** (unlike v1/v2), but it is **not evidence of improvement**. The guard can only detect movement *away* from the base. The benchmarks decide.
        - **Minor bug found:** the final `evaluate()` re-recorded step 46 in the guard history (a duplicate entry). Harmless (selection tolerates it); fixed in `DriftGuardCallback`, with a test.
      - **Next (chained):** `export v3` (merge → GGUF Q4_K_M → Ollama import) → `bench v3` on polyglot + HumanEval-rs, vs the reconverted base (2/30, 133/156).
      - **RESULT v3 (2026-09-13 ~16:30 EDT): no significant regression, and still no improvement.**

        | benchmark | reconverted base | v1 | v2 | **v3** (self-only, r=8, guarded) |
        |---|---|---|---|---|
        | polyglot pass@2 | 2/30 | 2/30 | 1/30 | **1/30** (lost two-bucket; p = 1.0) |
        | HumanEval-rs pass@1 | 72.4% | 60.9% | 68.6% | **69.9%** |
        | HumanEval-rs pass@2 | **133** | 106 (p < 0.001) | 119 (p = 0.016) | **125** (14 lost / 6 gained, **p = 0.115**) |
        | HumanEval-rs output tokens | 116K | 62K | 67K | **92K** |
        | HumanEval-rs compile_failed | 4 | 17 | 10 | **14** |

        - **The regression shrank monotonically: −27 → −14 → −8.** v3's drop is not statistically distinguishable from the base, though the point estimate is still negative.
        - **Style drift mostly fixed:** output volume is back to 79% of the base's (v1/v2 were ~55%).
        - **No improvement anywhere:** across three adapters, none solved a hard-set problem the base couldn't.
        - **Warning sign:** compile failures are still 14 vs the base's 4.
          - **12 are v3-only, almost all numeric type errors:** `E0308 mismatched types` ×7, `E0369` (arithmetic between mismatched number types), `E0606` (invalid cast), plus a wrong method and a non-iterator.
          - **Likely cause:** HumanEval-rs signatures use `isize`; the Exercism training problems mostly use `u64`/`i32`/`usize`. v3 appears to have picked up the training set's integer-type habits and misapplied them.
          - That's a specific side effect of the training distribution's conventions, not general damage. It's also the clearest mechanism found for why training on one problem set can hurt another.
        - Clean run: prompt parity 507 = 507, 0 infra errors, 0 truncated, no orphaned containers.
      - **Experiment status: at the A/B decision (the user's call).** What's established: the self-only + guarded recipe *avoids damaging* the model; nothing tried *improves* it. Plausible reasons it can't improve at this scale:
        - the training pool (Exercism practice) is easier than the hard set, so no example teaches what the base can't already do
        - a few hundred examples is small for skill, as opposed to style
        - polyglot's n=30 can't detect a small gain even if one existed

        **Per the user's instruction, the brain stays offline until this decision is made.**

---

## Conclusion (2026-09-13): decision B, stop

**Question:** does a QLoRA fine-tune on compiler-verified Rust solutions measurably improve
gemma4:e4b's pass rate on held-out Rust exercises?

**Answer, on this setup: no.** Three adapters, all compared against the base exported through
the identical pipeline:

| adapter | data | recipe | polyglot (hard, n=30) | HumanEval-rs (n=156) |
|---|---|---|---|---|
| base | none | none | 2 | 133 |
| v1 | 206 Codestral | lr 2e-4 × 2 ep, r=16 | 2 | 106 (p < 0.001, worse) |
| v2 | 886 from 3 teachers + self | lr 5e-5 × 1 ep, r=16 | 1 | 119 (p = 0.016, worse) |
| v3 | 181 self-only | lr 5e-5 × 2 ep, r=8, drift guard | 1 | 125 (p = 0.115, not significant) |

**What was learned:**
1. **Teacher distillation on a small, easy problem pool damages a small model through style,
   not skill.** It shortened replies, adopted the teacher's idiom, and added type errors. It
   happens during training itself, not in the export (NF4 check).
2. **Training on the model's own verified solutions, with a smaller guarded update, avoids
   most of the damage but adds nothing.** Self-training sharpens what the model already does.
3. **The residual harm has a mechanism:** v3's 12 new compile errors are mostly integer-type
   mismatches, the training set's `u64`/`i32` habits misapplied to HumanEval's `isize`.
4. **Validation loss was misleading throughout** (−53%, −39%, −11%), and a falling anchor loss
   meant "no drift", not "better". Only benchmarks against the reconverted base told the truth.
5. **Teacher strength is empirical:** the specialist code model (Codestral, 3/30) lost to the
   student (5/30); gpt-oss:120b (20/30) and gemma4:31b (13/30) were the real teachers.

**Why this setup couldn't improve the model (best current understanding):**
- the training pool (Exercism practice) is easier than the hard benchmark, so nothing in it
  is beyond what the base already does
- a few hundred examples can move style but not skill
- polyglot's n=30 cannot detect a small gain

**What a genuine second attempt would need:** harder training data at the benchmark's level,
far more examples, and a larger hard benchmark. That's days of GPU time, with no guarantee on
a 4B-effective model.

**What carries forward** (the actual value of this experiment):
- **Infrastructure:** the `rust-sandbox` + `execute_code(language:"rust")` brain tool; a pinned,
  self-checked multi-benchmark harness with paired significance tests and a stall alarm;
  data generation with contamination checks beyond names; memory planning on the meta
  device; CPU execution for a frozen embedding table; an answer-only chunked loss; an NF4
  adapter server for training-vs-export diagnosis; a drift guard.
- **Knowledge:** the reusable process and every silent failure, in
  [`FINE_TUNING_PLAYBOOK.md`](FINE_TUNING_PLAYBOOK.md).

**Resources:** exported GGUFs in `workspace/finetune/models/` (~5 GB each × 4) and their Ollama
imports (`gemma4-e4b-base-reconverted`, `gemma4-e4b-rust-v1..v3`, ~5.3 GB each) are kept for
now and can be deleted to reclaim ~40 GB. Adapters, datasets and all per-attempt results are kept.
      - **v2 import + benchmarks started (2026-09-13 ~13:30 EDT)** as a separate job: `import_ollama.sh` (inheriting the base's RENDERER/PARSER/PARAMETERs) → `bench v2` on polyglot + HumanEval-rs. Re-running the whole `export v2` phase would have redone the export for nothing. same base, LoRA r=16 on 342 layers, `MAX_SEQ_LEN=4096`, CPU per-layer table, bf16, answer-only loss.
        - **Recipe changed from v1:** **lr 5e-5 (was 2e-4), 1 epoch (was 2)**. About 111 optimizer steps vs v1's 52, so the total update scale (lr × steps) is roughly half of v1's.
        - **Rationale:** v1's drift was a training effect (NF4 check).
        - **Judged only by the benchmarks against the reconverted base**, never by the loss curve.
        - **Lesson worth keeping:** a check that silently times out every item costs hours unnoticed. Future long runs need a stall alarm (e.g. abort after N consecutive `model_error`s).
  - **Next (started immediately, chained with `&&` so a failed export stops before any benchmark):** `export-base` → `export v1` (merge → GGUF Q4_K_M → Ollama import with the base's RENDERER/PARSER) → `bench base` → `bench v1` (polyglot + HumanEval-rs each).
  - **Smoke sweep (`scripts/smoke_train.sh`):** ascending `--max-seq-len`, 3 optimizer steps each on the 3 *longest* examples that fit (OOM happens on the worst case, not the average). It records static VRAM, peak VRAM and s/step, and stops at the first OOM (`train.py` records an OOM and its stage as exit code 3) or once no longer example exists.
  - **The first version of the check was wrong, and the way it was wrong is worth keeping.** It compared *outputs*, which diverged about 50 tokens in, and it declared "formats differ". But `/api/chat` was deterministic against itself (3 of 3 identical), and the logprob run proved the prompts identical. Without `logprobs`, Ollama's `/api/chat` and `/api/generate` decode identical prompts differently; requesting logprobs puts them on the same path. So output identity is not valid evidence of prompt parity. The check now rests on logprob agreement, with output identity reported for information only. The benchmark itself is unaffected, since base and tuned models both go through `/api/chat`.
- [x] **Phase 4: Data.** Teacher + self sampling on the 76-problem pool, compiler-filtered, repair traces, contamination check.
  - **Scripts written 2026-09-12** (`workspace/finetune/datagen/`, while baselines ran):
    - **`contamination`:** finds pool exercises too close to a benchmark exercise.
    - **`generate`:** runs the benchmark protocol on the pool, k samples per exercise, keeping every episode.
    - **`build`:** compile-filters and produces the SFT JSONL.
  - **New risk this surfaced: name exclusion alone doesn't prevent contamination.** Exercism has derived exercises, e.g. `luhn` is in the training pool and `luhn-from` in the benchmark. Pool exercises are flagged on related names, instruction similarity ≥ 0.5, or test-name Jaccard ≥ 0.3, and skipped by default.
  - **Filters decided up front:**
    - Reject solutions that add a crate: the benchmark forbids them, so training on them teaches a `dependency_error`.
    - Reject passing replies that hit `max_tokens`.
    - Dedupe per exercise and cap examples per exercise, round-robin across samplers.
    - Split train/valid by exercise, never by sample.
    - Rewrite assistant turns to the canonical file-block format by default, so no sampler prose is learned.
- [x] **Phase 5: Train + re-evaluate.** Same runtime, same quant, same benchmark. Write the result into this doc, whatever it is. *(Three adapters, v1–v3; negative result.)*
- [ ] ~~**Phase 6: Through the brain.**~~ **Not run:** no adapter improved on the base, so there was nothing worth pinning a brain agent to.

## Results

### Baselines (2026-09-12, brain rested, GPU otherwise empty)

Polyglot Rust, 30 exercises, greedy, 2 tries, `think` off, `num_ctx` 16384, `max_tokens` 8192. Zero infra errors and zero clipped contexts in all three runs.

| model | pass@1 | pass@2 | final: pass / tests_failed / compile_failed / no_code | replies hit max_tokens | GPU residency | model time |
|---|---|---|---|---|---|---|
| **gemma4:e4b** | **10.0%** | **16.7%** (5) | 5 / 7 / 18 / 0 | 2 | 3.3 of 3.3 GB | 42 min |
| qwen3:8b | 3.3% | 10.0% (3) | 3 / 9 / 17 / 1 | 2 | 7.0 of 7.8 GB | 38 min |
| qwen3.5:4b | 0.0% | 6.7% (2) | 2 / 4 / 21 / 3 | 6 | 3.7 of 3.7 GB | 27 min |

- **Solved:**
  - gemma4:e4b: gigasecond, grade-school, say, simple-cipher, two-bucket
  - qwen3:8b: gigasecond, grade-school, nucleotide-codons
  - qwen3.5:4b: gigasecond, two-bucket
- **The models aren't statistically distinguishable.** Exact sign test on discordant pairs: gemma vs qwen3.5 p = 0.25, gemma vs qwen3:8b p = 0.625, qwen3:8b vs qwen3.5 p = 1.0. Thirty problems with pass counts this low can't separate them. This is the small-benchmark caveat above, observed.
- **Compile errors dominate:** 17–21 of 30 final outcomes per model, and 33–44 of all attempts. The retry mostly doesn't fix them. That's the failure compiler-filtered training data targets most directly.
- **Correction:** gemma4:e4b was **fully resident at 3.3 GB**. The 9.6 GB Ollama tag includes vision/audio weights that aren't loaded for text. The earlier claim that it "can't be fully resident" was wrong. qwen3:8b was the one that spilled to the CPU.
- **Base model: gemma4:e4b.** The scores alone don't decide it (see the p-values), so the call rests on:
  - best raw scores on both metrics
  - fully GPU-resident at 3.3 GB, leaving the most room for QLoRA
  - already the brain's local model (llama.cpp sidecar), so a tuned adapter serves the brain directly and advances the orchestrator goal
  - Apache-2.0 on Hugging Face (`google/gemma-4-E4B-it`, not gated)
- **Consequence for the tuned comparison:** a before/after delta of 2–3 problems won't be significant either. Plan to add HumanEval-rs **before** the tuned run, per the rule above, rather than after seeing the result.

### Second benchmark: HumanEval-rs (added 2026-09-12, before any tuned run)

- **Source:** MultiPL-E `nuprl/MultiPL-E`, config `humaneval-rs`, split `test`, **156 problems** (not 161 as I'd assumed), MIT, pinned to dataset revision `28441b60`. The rows API only serves the latest revision, so fetching refuses if upstream has moved. The cached copy keeps loading offline.
- **Mapping:** MultiPL-E scores `prompt + completion + tests`, where the prompt ends after the function's `{` and the tests start with the closing `}` then a `fn main()` of `assert_eq!`s. Here:
  - the model owns `src/solution.rs` (stub = prompt + `todo!()`) and never sees the tests
  - `src/main.rs` is `include!("solution.rs")` + the hidden `main`
  - `cargo run`, and exit code 0 is a pass
  - verified across all 156 rows: every prompt ends with `{`, every test block starts with `}` and has `fn main()`, 2 prompts carry `use` lines, none use `#![…]`
- **Differences from MultiPL-E's own runner:** whole-file replies rather than completions cut at `\n}`, and edition 2021 via Cargo rather than bare `rustc` (edition 2015). Numbers are comparable between our runs, not with published MultiPL-E scores.
- **pass@1 is the headline for HumanEval-rs.** The retry's feedback is the panic from the hidden `main` (e.g. `assertion left == right failed`), which shows values but not the call that failed. That's much weaker than Exercism's named failing tests, so pass@2 means less here.
- **Stub self-check:** 156/156 `tests_failed`, **0 `compile_failed`**, so every problem's crate compiles with its hidden tests attached. There's no reference-solution self-check, because MultiPL-E ships no Rust solutions. Codestral (`codestral-2508`) is run over it as a sanity reference instead: a strong model scoring well is the evidence the problems are passable through this mapping.
- **Contamination vs the training pool:** no new flags (still only `luhn`, `luhn-trait`, both from polyglot's `luhn-from`). 23 pool↔benchmark pairs share a function name and are listed for review, not excluded. **Reviewed 2026-09-12: all 23 are generic-name collisions, not the same task.** Examples: Exercism ciphers' `encode`/`decode` vs `simple-cipher`/HumanEval_93 (a case-swap/vowel-shift, a different problem); `score` in scrabble-score/yacht vs bowling; `add` in custom-set vs HumanEval_53 (integer addition). Kept in the pool. Known weak spot: task-level overlap across differently-worded problem sets can't be detected by statement similarity, so this review is the actual check.
- **Codestral reference result (`codestral-2508`, 2026-09-12):** pass@1 **83.3%**, pass@2 **87.8%** (137/156), with 0 infra errors, 2 compile failures and no truncated replies. A strong model passes nearly all problems through this mapping, so the crate conversion is sound. The 19 problems it never solved are a rough upper bound on "hard or possibly mistranslated". Reference only; benchmark episodes are never training data (`build` refuses benchmark names).
- **gemma4:e4b on HumanEval-rs: pass@1 76.9%, pass@2 87.8% (137/156). That's the same final pass count as Codestral**, with 10 problems solved only by each (sign test p = 1.0). 0 infra errors, 0 clipped, 0 truncated. On the first try Codestral is ahead (83.3% vs 76.9%); after one retry they tie.
  - **What this means: HumanEval-rs is near ceiling and probably memorized.** A 4B-effective local model matching a dedicated code model on HumanEval is the classic signature of benchmark contamination in pretraining, and MultiPL-E's Rust translations inherit the originals' ubiquity. Two consequences:
    1. **Headroom is ~12%.** A tuned model can't show much gain here.
    2. **A gain here can't separate better Rust from better recall.**
  - **Revised role of HumanEval-rs: a regression check** (tuning must not make the model worse at common tasks), **not the evidence of improvement.** The polyglot set, where gemma sits at 16.7% on hard, less-memorized exercises, is where headroom is. Adding HumanEval-rs was still right: it bought statistical power against *regressions*, and it exposed the ceiling before any tuned result could be misread.
  - **Follow-up:** Codestral is being run on polyglot too. Distilling from a teacher only makes sense if the teacher clearly beats the student *where the student is weak*, and HumanEval-rs no longer shows that.
- **Codestral on polyglot (teacher check, 2026-09-12): pass@1 6.7%, pass@2 10.0% (3/30). That's BELOW gemma4:e4b's 16.7% (5/30).** Every exercise Codestral solved, gemma solved too (gemma alone: grade-school, two-bucket; sign test p = 0.5). Final outcomes: 16 tests_failed, 10 compile_failed, 3 pass, 1 dependency_error.
  - **The harness was checked before believing this; the failures are real.**
    - Compile errors are genuine Rust mistakes: borrow checker (poker, react, doubly-linked-list), a lifetime (xorcism), a missing trait bound (fizzy), a missing import (macros), a changed closure signature (accumulate).
    - Test failures are mostly near misses (forth 45/50, book-store 16/18, luhn-from 13/14, grade-school 9/10). decimal overflows in its own arithmetic; scale-generator is off by one (an extra "C").
    - Manifests aren't the cause: final `Cargo.toml` text differs from the stub in only 5/30 (gemma 2/30), no package was renamed, and the one edition change sits next to a genuine type error.
  - **External anchor:** Aider's own leaderboard lists **Codestral 25.01 at 11.1%** on the full polyglot benchmark (all 6 languages, whole-file format, 2025-01-13), Qwen2.5-Coder-32B-Instruct at 16.4% (whole) and Gemma-3-27b-it at 4.9%. Our Rust-only 10.0% for `codestral-2508` is consistent with that, so the harness isn't the outlier, and gemma4:e4b's 16.7% is genuinely competitive for its size. (Not directly comparable: theirs is 225 exercises across languages with Aider's edit formats; ours is the 30 Rust exercises with whole-file replies.)
  - **One classification bug found and fixed:** parallel-letter-frequency added a `[[bench]]` for a nonexistent file, so Cargo couldn't parse the manifest. It was scored `tests_failed`; now it's `manifest_error`. No pass counts change.
  - **Consequence: Codestral is not a stronger teacher than the student on either benchmark** (polyglot 3 vs 5, HumanEval-rs tied at 137). Its Exercism-pool solutions can still teach compile hygiene and the reply format (the pool is easier: 69/74 solved), but there's no basis to expect them to lift gemma above Codestral's own level on hard problems. **Decision needed before training:** train on `rust-sft-v1` as-is, self-sample gemma on the pool (rejection sampling, teacher-free), or find a stronger teacher first.
  - **No stronger free teacher on this key.** The strongest Mistral model listed is Mistral Medium 3.5 (`mistral-medium-2604`; every medium/magistral-medium alias resolves to it; no "large" is listed). A single probe returned `429 rate_limited` with **`x-ratelimit-limit-req-minute: 0`**: the free Experiment plan allows zero requests to it, permanently rather than transiently. Codestral is the strongest model available free on Mistral, and it doesn't beat the student. (Medium is also a reasoning model whose `content` may come back as a list of thinking/text chunks, which `OpenAICompatClient` doesn't handle yet. Irrelevant while it's unavailable.)
  - **Decision (user, 2026-09-12): do both.**
    1. Train an adapter on `rust-sft-v1` (Codestral) as it is. It exercises the full pipeline and tests whether compile-verified data helps at all.
    2. Self-sample gemma4:e4b on the training pool (rejection sampling, teacher-free), build a second dataset from Codestral + gemma's own verified solutions, and train a second adapter.

    Both are compared against the same reconverted base. Pricing for a paid stronger teacher is being looked into separately.
  - **Ollama Cloud as a teacher (checked 2026-09-12):**
    - **Access:** `gemma4:31b-cloud` and `gpt-oss:120b-cloud` both work through the **local Ollama proxy** (`--provider ollama`, no key handling, no local GPU), about 0.4 s for a short reply.
    - **gpt-oss:** ignores `think: false` and reasons anyway (the thinking counts against `max_tokens`).
    - **Pricing page:** the free plan is "a starter amount of usage for a limited set of starter models", **metered in tokens at each model's rate**, 1 concurrent request. Pro is $20/mo ($60 credits, 3 concurrent). **"Prompt or response data is never logged or trained on."**
    - **Licenses:** both models are Apache-2.0.
    - **Caveat:** the free quota is shared with the brain's own cloud calls.
    - **Being tested on polyglot now** (sequentially, since concurrency is 1), with the same rule as Codestral: a teacher is only worth distilling if it clearly beats gemma4:e4b's 5/30. If one does, it becomes a sampler (`cloud-gemma4-31b` / `cloud-gpt-oss-120b` in `samplers.json`) for the second dataset.
  - **gemma4:31b-cloud on polyglot: pass@1 30.0%, pass@2 43.3% (13/30), the first teacher that clearly beats the student.**
    - It solves **all 5** of gemma4:e4b's exercises **plus 8 more** (grep, luhn-from, nucleotide-codons, pig-latin, robot-name, variable-length-quantity, word-count, wordy). **Sign test p = 0.008**, the first comparison in this experiment that isn't noise.
    - 0 infra errors, 0 truncated, 0 clipped. Final outcomes: 13 pass, 9 compile_failed, 8 tests_failed.
    - Cost: 205 s of model time, 88K tokens in / 57K out for 30 exercises.
    - **Same family as the student** (Gemma 4, 31B vs E4B), the most favourable case for distillation: shared tokenizer, and style the student can plausibly imitate.
  - **gpt-oss:120b-cloud on polyglot: pass@1 40.0%, pass@2 66.7% (20/30), the strongest teacher tested.**
    - vs gemma4:e4b: 20 vs 5, **p < 0.001**; it misses only gigasecond of the student's five.
    - vs gemma4:31b-cloud: 20 vs 13, **p = 0.039**; it adds alphametics, book-store, bowling, decimal, fizzy, macros, ocr-numbers and poker.
    - 0 infra errors; 1 reply truncated at `max_tokens`, 1 dependency_error.
    - **Cost:** 65K tokens in / **135K out**, over twice gemma31b's output for the same 30 exercises, because it reasons before answering (the reasoning comes back separately from `content`, so only the code reaches a dataset). 346 s of model time.
    - **Trade-off vs gemma4:31b:** much stronger, but a different model family (different tokenizer and style), whereas gemma31b is the student's own family.
    - **Teacher ranking on polyglot:** gpt-oss:120b 20 > gemma4:31b 13 > gemma4:e4b (student) 5 > Codestral 3.
    - **Decision:** sample both cloud teachers, **gpt-oss first** (strongest, and it spends quota faster, so it goes first in case credits run out), then gemma4:31b. Both feed `rust-sft-v2` with Codestral and the student's own verified solutions. Sampling started 2026-09-12.
    - **Cloud teacher sampling FINISHED (2026-09-12/13), with no quota problems: 0 model/infra errors and 0 truncated replies across 592 episodes.**

      | teacher | pass / 296 | first try | repaired on retry | exercises solved / 74 |
      |---|---|---|---|---|
      | gpt-oss:120b-cloud | 271 | 238 | 33 | 73 |
      | gemma4:31b-cloud | 274 | 226 | **48** | 72 |
      | codestral-2508 | 241 | 200 | 41 | 69 |

      gemma4:31b produced the most repair traces, the example type that matches the student's main failure (compile errors). The free Ollama Cloud plan covered both teachers' full runs.
    - **Teachers-only builds (Codestral + gpt-oss + gemma31b), and a silent cap problem:**
      - **Default `--max-per-exercise 4` (sized for ONE sampler):** 353 train examples, **414 capped out**, i.e. most of the stronger teachers' data discarded without a warning.
      - **Cap 12:** **730 train** (620 solve / **110 repair**, 67 exercises; gpt-oss 273, gemma31b 244, Codestral 213) + 68 valid (6 held-out exercises), 0 capped. Still 101 duplicate solves across teachers (deduped).
      - **Rule:** the cap must scale with the number of sources. `build-v2` now uses 16 (4 sources × 4 samples). `rust-sft-v1` is unaffected (1 sampler, 0 capped).
    - **Earlier decision (before gpt-oss finished):** add `cloud-gemma4-31b` as a sampler for the second dataset, alongside Codestral and gemma4:e4b's self-samples. **Quota caveat:** sampling the 74-exercise pool × 4 is roughly 10× this run's tokens and may exceed the free starter credits. Sampling is resumable, so an exhausted quota costs a pause, not the episodes already written.
- **Local HumanEval-rs baselines (finished 2026-09-12 23:13 EDT):**

  | model | pass@1 | pass@2 | final: pass / tests_failed / compile_failed | truncated | GPU residency | model time |
  |---|---|---|---|---|---|---|
  | codestral-2508 (reference) | 83.3% | 87.8% (137) | 137 / 17 / 2 | 0 | cloud | 7 min |
  | **gemma4:e4b** | 76.9% | **87.8% (137)** | 137 / 14 / 5 | 0 | 3.3 of 3.3 GB | 31 min |
  | qwen3:8b | 63.5% | 74.4% (116) | 116 / 28 / 12 | 0 | 6.8 of 7.8 GB | 34 min |
  | qwen3.5:4b | 56.4% | 74.4% (116) | 116 / 25 / 14 (+1 no_code) | 3 | 3.7 of 3.7 GB | 29 min |

  - **HumanEval-rs separates the candidates where polyglot couldn't.** gemma4:e4b vs qwen3:8b is 137 vs 116, **p = 0.001**; vs qwen3.5:4b, **p < 0.001**. The two Qwens tie.
  - **This retroactively gives the base-model choice statistical support.** It was made on secondary factors, because polyglot's 30 problems couldn't separate the models (p ≥ 0.25).
  - **It's also the concrete payoff of adding a larger benchmark.** At the top, though, gemma ties Codestral, which is why HumanEval-rs is a regression check rather than the improvement signal.
  - 0 infra errors, 0 clipped contexts. qwen3.5:4b was the only model with truncated replies, consistent with its polyglot behaviour.
- **Local baselines started 2026-09-12 21:27 EDT**, brain rested the same way as before (scheduler paused, `llamacpp`/`whisper` stopped, Ollama emptied, GPU at 467 MiB). Order: gemma4:e4b → qwen3:8b → qwen3.5:4b, with the chosen base first. Same settings as the polyglot baselines. Two Mistral jobs ran concurrently and use no GPU (the Codestral reference on HumanEval-rs, and teacher sampling on the pool), but they share the sandbox's two build slots. That affects wall time, not scores.

## What we expect to learn that the big plan doesn't cover

- whether 76 problems is enough, or whether data volume is the real bottleneck
- whether repair traces beat plain solutions
- how much of a gain survives Q4 quantization
- whether a benchmark of 30 can detect the effect at all
- the real cost of the GPU maintenance window on the brain's background work
