# Fine-Tuning Playbook

**What this is:** the reusable lessons from Experiment 1 (teaching gemma4:e4b to
write better Rust), written so the next fine-tune (a different model, a
different specialty) starts from what we learned instead of rediscovering it.

- **The lab notebook** with every measurement is
  the `RUST_FINETUNE_EXPERIMENT` dev notes (`scripts/dev_docs.sh search "<question>" RUST_FINETUNE_EXPERIMENT`).
- **The long-term plan** is the `FINE_TUNING_PLAN` dev notes (kind `plan`).
- **The code** is `workspace/finetune/` (its own git repo).

**Status:** written 2026-09-12; results completed 2026-09-13, when Experiment 1
**concluded with a negative result** (no adapter improved the model; the recipe that
avoids harming it is known). Everything below was learned the hard way.

---

## The one-paragraph version

Fine-tune for **behaviour, verified by a machine**, never for knowledge. Build
the benchmark and its self-checks before touching a model. Measure every
candidate base and every candidate teacher on the *hard* benchmark before
deciding anything. Generate data separately from filtering it. Estimate memory
before loading weights. Compare the tuned model only against the base exported
through the *identical* pipeline. And assume every number is wrong until
you've looked at what produced it: the failures that cost us the most gave
plausible scores, not errors.

---

## 1. Choose the task

- **Pick behaviour with an automatic verifier.** Rust has a compiler and tests,
  so correctness is checked mechanically and no model grades another. Other
  good candidates: JSON matching a schema, SQL that runs against a real
  database, Cypher that passes `cypher_guard`, a lint-clean schedule. Voice and
  style have no verifier, so they need human ratings and are much harder to do
  honestly.
- **Don't fine-tune knowledge.** The graph holds facts; weights go stale and
  can't be cited.
- **Start outside the system you eventually want to change.** We built a
  standalone harness instead of wiring training into the brain first. That
  surfaced a dozen problems cheaply that would have been confusing inside the
  brain.

## 2. Environment (this machine)

- **Hardware:** RTX 3060 Ti, 8 GB; driver 535 (CUDA 12.2); 78 GB RAM, 32 cores.
- **CUDA compatibility is image-specific.** PyTorch's cu126 wheels run on the
  535 driver via minor-version compatibility (measured). The llama.cpp
  upstream 12.8 image does not, because it bundles a forward-compat libcuda
  stub. **Test the actual image** (`scripts/train.sh check-env`); don't reason
  from version numbers.
- **Everything runs in containers.** Nothing is installed on the host; the
  harness is stdlib-only Python.
- **Training and serving can't share the GPU.** To "rest the brain":
  1. `scheduler_control stop`
  2. `docker compose stop llamacpp whisper`
  3. unload Ollama models (`keep_alive: 0`)
  4. keep the brain container up (tests run through its `execute_code`)

  Restore in reverse. Write the rested state into memory, so a later session
  knows why the scheduler is off.
- **Cloud models don't need the GPU.** Mistral via API, and Ollama Cloud via
  the local Ollama proxy (`gemma4:31b-cloud`, `gpt-oss:120b-cloud`), can run
  while the GPU is busy. They share the sandbox's build slots, which affects
  wall time, not scores.

## 3. Build the benchmark before any model touches it

1. **Pin the source.** Commit sha or dataset revision in `sources.json`. If an
   upstream API only serves "latest", refuse to fetch when it has moved.
2. **Self-check with no model at all.**
   - *Stub* (starter code unchanged) must score 0%, which proves failures are
     detected.
   - *Reference* solutions must pass. Ours scored 24/30, and the 6 misses were
     reference solutions needing crates the benchmark forbids, now a named,
     expected result.
   - If no references exist (MultiPL-E has none for Rust), check that every
     stub **compiles** and run a strong model as a sanity reference.
3. **Keep the model from seeing the tests.** The model never sees hidden tests,
   and extraction only accepts the allowed solution paths, so a reply can't
   overwrite the tests.
4. **Classify outcomes precisely.** "Failed" hides the lesson. We needed
   `compile_failed`, `tests_failed`, `dependency_error` (reached for a
   forbidden crate), `manifest_error` (broke Cargo.toml), `no_code`,
   `timeout`, and `infra_error` (never scored as a model failure).
5. **Know the benchmark's statistical power.** At 30 problems one problem is
   3.3 points; our three candidates (2, 3 and 5 solved) were indistinguishable
   (sign test p ≥ 0.25). `harness.report` pairs two runs and gives an exact
   sign test. **Decide the suites before the tuned run**, never after seeing a
   result.
6. **Check for ceiling and memorization.** If a small local model ties a
   strong code model, the benchmark is saturated or memorized. gemma4:e4b tied
   Codestral at 137/156 on HumanEval-rs. That benchmark then becomes a
   *regression* check, not evidence of improvement. The hard benchmark (gemma
   16.7%) is where improvement shows.
7. **Anchor to an external number when a result looks wrong.** Codestral
   scoring 10% looked like a harness bug. Aider's leaderboard lists Codestral
   at 11.1%. Check the failures *and* an outside source before believing or
   dismissing a number.

## 4. Silent skews: things that produced plausible scores, not errors

Every one of these happened in this experiment. Check for each in any new
harness.

| skew | symptom | fix |
|---|---|---|
| Missing crate scored as a test failure | reference solutions "failed tests" | `dependency_error` outcome |
| Rustc warnings / backtraces / per-test lines / thread ids in feedback | retry prompt full of noise; real failures cut by the length cap; prompts differ run to run | strip them before feedback goes back |
| `max_tokens` too low | reply cut inside its final code block; **extraction scored an earlier draft** from the same reply | raise the cap (8192); flag every capped reply |
| Ollama's default 4096-token context | prompt silently clipped | always send `num_ctx`; count `ctx_clipped` |
| Thinking models | whole budget spent thinking, empty answer | `think: false`; record thinking length (gpt-oss ignores it) |
| Broken `Cargo.toml` scored as a test failure | "tests None/None" | `manifest_error` outcome |
| **"Outputs differ" read as "prompts differ"** | parity check failed on identical prompts | Ollama's `/api/chat` and `/api/generate` decode differently unless logprobs are requested; **compare logprobs**, not outputs |
| Output text of a sampler assumed to be a string | (caught before use) | reasoning models may return content as a list of chunks |

**Always look at the raw record behind a surprising number.** Every attempt
is saved with its full reply, extracted files, test output and exact feedback.

## 5. Choose the base model

- **Measure the candidates, and decide on more than the score when scores
  don't separate them.** We picked gemma4:e4b because it had the best raw
  score, was fully GPU-resident, and is already the brain's local model.
- **Check what size actually loads.** Ollama's 9.6 GB tag for gemma4:e4b
  includes vision/audio weights; it loads text-only at 3.3 GB. The model
  that spilled to CPU was the one with the smaller tag.

## 6. Choose the teacher (or decide not to have one)

- **A teacher is only worth distilling if it clearly beats the student on the
  hard benchmark.** Codestral scored *below* gemma4:e4b on polyglot (3 vs 5)
  and tied on HumanEval-rs. The easy training pool didn't show this; it
  solved 69/74 there. Test teachers where the student is weak.
- **Measure several teachers; labels predict nothing.** Polyglot results
  against the student's 5/30:

  | teacher | solved | vs student |
  |---|---|---|
  | gpt-oss:120b-cloud (general reasoning model) | **20/30** | p < 0.001 |
  | gemma4:31b-cloud (student's own family, larger) | 13/30 | p = 0.008 |
  | Codestral 2508 (specialist code model) | 3/30 | *below* the student |

  The model sold as a code specialist was the worst teacher; a general
  reasoning model was the best. A larger sibling of the student is a strong
  middle option because it shares tokenizer and style.
- **Reasoning teachers cost more quota per example.** gpt-oss produced 135K
  output tokens for 30 exercises against gemma31b's 57K. With a metered free
  plan, sample the strongest teacher first so an exhausted quota costs the
  weaker data, not the best.
- **When no teacher is stronger, rejection sampling is the teacher-free
  alternative.** Sample the student itself, keep only compiler-verified
  successes, train on those.
- **Check terms per provider and per model, and write down the date:**
  - *Mistral* (checked 2026-09-12): only image outputs restricted; the free
    plan trains on your prompts.
  - *Ollama Cloud*: "never logged or trained on".
  - *Model licences*: gpt-oss and Gemma 4 are Apache-2.0.
  - *Excluded*: Gemini and Anthropic outputs (terms), and OpenAI-hosted models
    (user preference).
- **Probe free tiers before planning around them.** Mistral Medium 3.5 returned
  `x-ratelimit-limit-req-minute: 0`, i.e. never available on the free plan.
  Ollama Cloud's free plan is 1 concurrent request, metered in tokens, and
  shared with the brain.
- **Pin model ids**, not aliases (`codestral-2508`, not `codestral-latest`).

## 7. Generate and filter data

- **Sample and filter as separate steps.** `generate` keeps every episode,
  pass or fail; `build` filters. Changing a filter never costs model time.
- **Contamination is more than names.**
  - *Name exclusion* missed derived exercises (`luhn` in the pool, `luhn-from`
    in the benchmark).
  - *Automatic flags:* related names, statement similarity, and overlapping
    test names.
  - *Shared function names* are listed for human review; the 23 we reviewed
    were all generic collisions. Task-level overlap across
    differently-worded benchmarks is still the weak spot, and the review is
    the actual check.
- **Filter for habits the benchmark punishes:** solutions that add a crate
  (the benchmark forbids crates), and passing replies that hit `max_tokens`.
- **Expect duplicates.** At T=0.7 Codestral produced 44 identical solutions
  over 4 samples per exercise. Dedupe, and cap examples per problem.
- **Scale the per-problem cap with the number of sources.** A cap sized for
  one sampler (4) silently discarded 414 of 767 usable examples once three
  teachers were combined. Check `capped_out` in every dataset manifest.
- **Keep repair traces** (failed attempt → test output → fix). They match the
  benchmark's retry, and they're what a model that fails to compile most needs.
  We got 41 of them from 296 teacher episodes.
- **Split train/validation by problem, never by sample.**
- **Rewrite replies to the canonical output format**, so the model learns the
  format and none of a teacher's prose.

## 8. Plan memory before loading weights

- **Estimate on the meta device** (`plan-memory`): config only, no weights, no
  GPU.
  - *Embeddings aren't quantized by bitsandbytes.* Gemma-4-E4B's
    `embed_tokens_per_layer` is 2.82 B params, 5.25 GiB in bf16. Static
    footprint all on GPU: 8.94 GiB. Doesn't fit.
  - *PEFT's `prepare_model_for_kbit_training` upcasts every non-4-bit parameter
    to fp32*, which makes that 15.5 GiB. Read the library source rather than
    assuming.
  - *Tie weights before counting.* Our first estimate double-counted the
    tied `lm_head` (10.19 GiB).
  - *Frozen lookup tables can live on the CPU.* `--cpu-modules` + `--keep-bf16`
    brings E4B to 3.69 GiB static, with 4.11 GiB left for activations.
- **Discover LoRA targets from the module tree,** excluding vision/audio.
  E4B has 342 targets, including Gemma's `per_layer_*` projections that a
  hardcoded q/k/v/o list would miss.
- **Smoke-test on the longest examples,** because out-of-memory happens on the
  worst case. `smoke_train.sh` sweeps sequence lengths, records static and peak
  VRAM and s/step, and treats an OOM as a recorded result, including the stage
  where it happened.
- **Drop over-length examples, never truncate them.** A truncated example
  teaches half an answer.
- **CPU offload in accelerate is NOT CPU execution.** `device_map` → "cpu"
  keeps weights on the CPU *between* calls, but `AlignDevicesHook` copies them
  to the GPU for every forward. The first smoke run died trying to allocate
  exactly the offloaded table (5.25 GiB). For a lookup table, replace the module
  with one that runs the lookup on the CPU and moves only the result
  (`CpuEmbedding`: bit-identical output).
- **Large vocabularies need an answer-only loss.** Gemma 4's 262,144-token vocab
  makes full-sequence logits (plus fp32 upcast plus gradient) the next OOM.
  Compute hidden states once, keep answer positions, and run head → softcap →
  cross-entropy in checkpointed chunks. **Verify it equals the naive loss** (we
  got 0.0 difference in float64, gradients within 1e-15) before trusting it.
- **Measured (Gemma-4-E4B, RTX 3060 Ti, both fixes):** static 3644 MiB (the
  estimate said 3.69 GiB); peak 5301 MiB at 2048 tokens (5.0 s/step), 6202 MiB
  at 4096 (8.3 s/step), OOM at 8192. Activations ~0.5 MiB/token. **Pick the
  largest measured length with real headroom**, then count what it drops. At
  4096 the dropped examples were the longest, i.e. mostly repair traces.

## 9. Compare fairly

- **Tuned vs the *reconverted* base, never vs the published model.** Export
  the untuned base through the same merge→GGUF→quantize pipeline and benchmark
  it too. Ollama's published build was converted by Ollama. **Observed:** the
  same prompt (identical token count) gave `compile_failed` on Ollama's
  gemma4:e4b and `tests_failed` on our llama.cpp-converted base. Conversion
  alone changes answers. Our build was also 7.5B vs 8.0B params (text-only) and
  2.94 GiB resident.
- **Verify serving parity by token count on real prompts.** After importing,
  compare `tokens_in` for the same benchmark prompts against the published
  model (507 = 507, 392 = 392 here). A mismatch means the renderer differs.
- **Serve with identical prompt handling.** Ollama's gemma4/qwen3.5 use
  built-in `RENDERER`/`PARSER` plus sampling `PARAMETER`s the harness doesn't
  override. `import_ollama.sh` copies them from the base.
- **Training prompts must equal serving prompts.** `check-template` compares
  Ollama's rendering against the HF template **by logprobs** (Gemma 4:
  identical, zero difference over 80 tokens).
- **Same quantization for both** (Q4_K_M), same benchmarks, same settings.
- **[pending]** The tuned-vs-base result on polyglot (improvement) and
  HumanEval-rs (regression).

## 9b. Diagnose a regression before changing anything

What happened with adapter v1: validation loss fell 53%, then HumanEval-rs dropped
133 → 106 (p < 0.001) and polyglot didn't move.

- **Loss on teacher data measures imitation, not capability.** The validation split
  was by problem and still reported steady improvement: it measured how well the model
  predicted the *teacher's* solutions. **Never ship on a loss curve; ship on the
  benchmark against the reconverted base.**
- **Read the replies before theorizing.** Two plausible explanations were rejected
  directly from the saved replies:
  - *Lost reasoning prose:* the base didn't write prose before code either.
  - *Broken stop token:* every regressed reply stopped cleanly; the truncated ones
    were repetition loops.

  What had actually changed was **style**: replies halved in length and switched to
  the teacher's terse idiom, bringing new type errors. **Style without skill.**
- **Separate training from export by serving the adapter as trained**
  (`trainer/serve_openai.py`: NF4 base + LoRA, no merge or GGUF) against the NF4 base.
  The regression and the halved output both reproduced, so training was the cause.
- **Beware selection bias in a diagnostic set.** Problems chosen *because* one run
  failed them score 0 for that run by construction. Include random controls, and
  compare only runs that weren't used to select the set.
- **Two tooling traps hit during diagnosis:**
  - **PEFT re-dispatch:** in peft 0.20.0, `PeftModel.from_pretrained` re-plans device
    placement (`infer_auto_device_map` + `dispatch_model`) when `hf_device_map`
    contains `"cpu"`. With an offloaded embedding that put compute on the CPU (727%
    CPU, 7% GPU). Attach adapters with `get_peft_model` + `set_peft_model_state_dict`,
    as training does, and verify every LoRA tensor loaded.
  - **Silent stalls:** a server timing out on every request cost ~6 h overnight
    before anyone looked. `harness.run --max-consecutive-errors` (default 3) now
    aborts such a run.

## 10. Keep notes that survive

- **The harness is its own git repo.** Results are committed: every attempt's
  reply, test output and feedback.
- **Every run writes its configuration:**
  - `run.json`: model digest and quantization, harness commit.
  - `train_summary.json`: base revision, dataset manifest sha256,
    hyperparameters, library versions, loss before and after, peak VRAM.
  - `export.json`: GGUF sha256, llama.cpp tag.
- **Negative results are results.** "Codestral isn't a stronger teacher" was one
  of the most useful findings.
- **Write the correction next to the original claim.** We said gemma4:e4b
  couldn't be fully resident, then measured 3.3 GB. The notebook keeps both.

---

## Checklist for the next fine-tune

1. [ ] Task has a machine verifier; behaviour, not knowledge.
2. [ ] Benchmark pinned; stub and reference self-checks exact; outcomes classified.
3. [ ] Benchmark power understood; a ceiling/memorization check done; suites fixed before tuning.
4. [ ] Harness checked against the §4 silent-skew table on a smoke run.
5. [ ] Candidate bases measured; real loaded size checked.
6. [ ] Teacher measured on the **hard** benchmark vs the student; terms and free-tier limits probed; ids pinned. Or rejection sampling.
7. [ ] Contamination checked beyond names; filters for punished habits; dedupe; split by problem.
8. [ ] `plan-memory` on the meta device; smoke sweep on longest examples.
9. [ ] Prompt parity by logprobs; tuned vs reconverted base; same quantization.
10. [ ] Everything recorded: configs, hashes, negative results, corrections.

## Results (Experiment 1, gemma4:e4b → Rust)

All scores are vs the untuned base exported through the same pipeline.

| adapter | data | recipe | polyglot (hard) | HumanEval-rs (regression check) |
|---|---|---|---|---|
| base | none | none | 2/30 | **133/156** |
| v1 | 206 Codestral solutions | lr 2e-4 × 2 epochs | 2/30 (p = 1.0) | **106** (p < 0.001, regression) |
| v2 | 886: gpt-oss 273, gemma31b 244, Codestral 213, self 179 | lr 5e-5 × 1 epoch | 1/30 (p = 1.0) | **119** (p = 0.016, regression; +13 vs v1, p = 0.029) |
| v3 | 181 self-only (student's own verified solutions) | LoRA r=8, lr 5e-5, 2 epochs, drift guard on an MBPP-rs anchor (never fired) | 1/30 (p = 1.0) | **125** (14 lost / 6 gained, p = 0.115, not significant) |

- **Self-only data + a small, guarded update removed most of the damage** (−27 → −14 → −8)
  and brought output length back to 79% of the base's. **It did not produce any gain.**
  Training on your own verified outputs sharpens what you already do; it can't teach
  what you can't already produce. Expect "no harm", not "better".
- **A drift guard on the model's own solutions detects drift, not improvement.** v3's
  anchor loss *fell* 26%, as self-training makes it do. A falling anchor loss meant
  "no drift" and nothing more.

- **Does compile-verified distillation help this model?** **Not so far. Both adapters made
  it worse**, and neither improved the hard benchmark. The validation loss fell 53% (v1)
  and 39% (v2) regardless; it measured imitation, not capability.
- **Does a gentler recipe help?** Yes, partly: it **halved** the regression (−27 → −14
  problems) but didn't remove it.
- **Do the student's own verified solutions help?** Not yet separable: v2 changed the recipe
  *and* added self-samples and stronger teachers at once, and self-samples were only 20% of
  the data. **Lesson: change one variable per adapter when a result needs explaining.** A
  self-only adapter is the clean test.
- **What the regression looks like:** shorter, terser replies in the teacher's idiom, more
  type errors. Style without skill. It reproduces without the export (NF4 check).
- **Cost of v1 + v2:** about 20 + 48 min of training, 4 benchmark passes (~35–45 min each),
  ~5 h of self-sampling, and two cloud teachers' full 296-episode runs within Ollama
  Cloud's free plan. Plus ~6 h lost to a silent stall and two runs killed by the session's
  memory monitor.
