# Harness² plan: the brain improves its own agents from evidence

Planned 2026-10-23 from the owner's hand-off of Kai's video on Google's
*Harness²: Recursive Agent Harnessing for an Open World* (arXiv 2610.14224),
read in full from `arxiv.org/html/2610.14224`. Built by the brain itself on free
models (dogfood run), overseen and finished where it could not.

## 1. What the paper does

An agent's quality is its model plus its **harness**: eight editable components
(system prompt, guardrails, memory, skills, tool guidance, scripts, sub-agents,
middleware plugin). Harness² improves the harness online, with no
ground-truth verifier, at two levels:

- **Task level: Propose → Probe → Compose.** Run the current harness once (the
  *reference*: it shows how the harness behaves, not whether it is right). An
  *editor* writes two candidates: an **invariant** one (how the agent works:
  planning, tool use, context, delegation) and a **domain** one (what a
  practitioner knows). Each is run on the same task under the same model; the
  trajectories are compared with the reference (mitigated / unchanged /
  regressed). A *composer* merges supported edits, adopts one, or keeps the
  current harness, and every decision must cite a concrete observation, not a
  presumed score. Budget k runs it in parallel (width) or in sequence (depth).
- **Harnessing level: a library.** After each task a reflector stores the
  adopted harness with an EXPERIENCE.md (the situation, the lesson, the
  evidence), only when the contrasts support it. The next task starts from the
  base harness again; the library is looked up on demand, never inherited.

Editor rules worth keeping: every edit traces to a trajectory moment; prefer a
divergence between runs, then a repeated concrete failure, then a visible
risk; one defect in one rollout is noise; prefer skills with a trigger line,
then scripts, sub-agents, plugins, and always-on text last ("the expensive
option", paid every run); keep the deliverable's shape. Ablations: without the
library −3.1/−1.8, without scoping (INV/DOM) −2.9/−3.7, without composition
−1.6/−1.9 points. Unguided refinement (no reference, probe or library) made
things worse (Code 73.4 → 63.2). Most gains were plumbing: skills and scripts
are half of all edits, tool-error rates fell on every benchmark. Failure modes:
the selector picks a weaker candidate; a wrong premise propagates across
candidates; noise.

## 2. Where the brain already does this

| Harness² | Agent Brain today |
|---|---|
| Harness as editable files | Profiles, workflows, declared tools, schemas, behaviors are plugin YAML and graph nodes, never Rust literals |
| Guardrails | Write-boundary validation (tool schemas, workflow steps, `when`, profiles), arg coercion, the read-only Cypher guard, the claim grounding and contradiction gates |
| Discard rules for a bad edit | `profiles::validate` refuses unknown fields, missing tools, bad routing targets and regexes; `manage_plugin write` checks every file as its loader would |
| Skills with a trigger line | Front-door behaviors (`when`/`unless` regex → a deterministic `lookup`), mined weekly by `behavior-candidates` from repeated first calls |
| Scripts over prose for deterministic work | The standing rule "when a behaviour must happen, don't make it a decision"; SQL/Cypher steps; declared tools |
| Sub-agents | Spawnable profiles via `run_agent`, depth 1, with the routing policy per run |
| Evidence over scores | `run_agent verify` against the real tool trace; the tool receipt; capability-mining quotes the `[FAILURE]` that justifies a candidate |
| Scorecards | `agent_scorecard`, `workflow_scorecard`, program reviews, `arg_checks`, routing/retrieval/cypher golden sets |
| Experience library | Dev notes: rule + history with the measurement behind it, searched before a change |
| The human gate | Plugin bundles, proposals and program reviews are drafts the owner enables |

## 3. Gaps

1. **No probe.** A profile or prompt edit is adopted without running it against
   the current one on the same task. The 2026-10-23 `intake` work was
   Propose-Probe-Compose done by hand: three rounds, each a reference run, a
   diagnosis from its trajectory, an edit, a re-run. Nothing in the brain can
   run an agent under a candidate prompt, and a real run has side effects (the
   tests left two duplicate todos and a duplicate article task).
2. **No trajectories.** `agent_runs` keeps counts (`tool_calls`,
   `tools_failed`), not which tools ran with which arguments and what came back,
   nor the task. A composer has nothing to contrast; a picker cannot find the
   failing run to replay.
3. **Proposals are prose, not harnesses.** capability-mining, diagnose and
   program reviews produce text and todos. None produces a candidate profile,
   none is probed, none cites a trajectory moment.
4. **The library is written by people.** Dev notes record lessons from Claude
   Code sessions; the brain neither writes harness experience from its own
   contrasts nor reads it when editing an agent.
5. **Always-on text.** The front door carries 6.4K characters on every turn on
   a paid model; `sections` are conditional on tools only, not on the turn, so
   a block like HANDED A LINK is paid on turns with no link.
6. **The brain reads abstracts.** `ingest_article` on `arxiv.org/abs/…` reads
   the abstract page (3,188 characters for this paper), so the brain's own
   analysis of Harness² never saw its method.

## 4. Phases

Each phase names who builds it. "Brain" means a brain agent on a free model
(Ollama Cloud gpt-oss, Mistral codestral, or local E4B where one-shot), run by
`run_agent` with an explicit `model` (ROUTING_PAID_FOR admits sub-agents to
paid models, so every dogfood call pins a free one and the Anthropic ledger is
checked after). Claude Code reviews, applies, tests and deploys; it writes a
piece itself only when the brain's draft fails.

- **Phase 0, read the whole paper.** A reading rule list
  (`plugins/media/read-rules.yaml`: a URL pattern and the URL to read
  instead, e.g. arXiv `abs` → `html`) applied by `fetch_article`; re-read
  2610.14224. Brain drafts the Rust and the YAML.
- **Phase 1, trajectories and probes** (Rust, `crates/chat`, `crates/skills`).
  - A run's trajectory: each call's tool, arguments (cut), success and the head
    of its result, plus the answer. `run_agent` returns it as `trajectory`;
    `agent_runs` stores it with the task's head (`trajectory`, `task`).
  - `run_agent harness: {system_prompt?, tools?}` runs the agent under a
    candidate prompt and/or toolset (tools checked against the registry, never
    beyond it); `probe: true` runs every write tool as a stub that returns
    `not run (probe): would call <tool>` and records it, so a probe has no side
    effects.
- **Phase 2, the improvement loop** (YAML, a `harness` bundle): workflow
  `harness-improve {agent, task, why}`: reference probe → library lookup →
  `harness-editor` writes the invariant and domain candidates (each edit
  citing a trajectory moment, component order as §1) → probe both → 
  `harness-composer` compares the three trajectories and decides keep / adopt /
  merge with the observation behind each decision → probe the result → write
  the composed profile to the workspace, a `harness-library/<agent>` dev note
  only when the contrast supports it, and a todo for the owner with the diff.
  Never adopts by itself: profiles are git YAML the owner commits.
- **Phase 3, what to improve** (YAML): weekly `harness-candidates` mines
  `agent_runs` trajectories for an agent with the same failure in 2+ runs (one
  failure is noise) and thumbs-down chat turns, and starts `harness-improve`
  on the worst, k=1, free models only.
- **Phase 4, on-demand prompt text** (Rust, `profiles.rs`): a section's `when`
  regex, matched against the turn's message (or the agent's task), leaves it out
  otherwise. Measured on `eval/routing_golden.yaml` before the front door's
  link and pipeline blocks move into sections.

Validation of the loop: a known-answer test. Start `harness-improve` on
`intake` with its round-1 prompt (before the status, Briefings & Intake and
self-source fixes) and the round-1 task; the loop should find at least one of
the three fixes found by hand.

## 5. Status

Planned 2026-10-23; all phases built and deployed the same day, on free models
only (0 Anthropic calls; `dev_docs get harness2-plan/2026-10-23-dogfood-what-free-models-could-build-and-the-plum`
has who built what and what failed).

- Phase 0 built: `read-rules.yaml`; the paper re-read at 36,000 characters (was 3,188).
- Phase 1 built: `trajectory.rs` (written by the brain's deep-diver on gpt-oss),
  `run_agent probe/harness`, `agent_runs.task/trajectory`, the probe allowlist.
- Phase 2+3 built: the `harness` bundle (drafted by the brain's toolsmith on
  gpt-oss in three rounds, reviewed, promoted to `plugins/harness`, enabled).
- Phase 4 built: section `when`; the front door's HANDED A LINK block is the
  first (719 of 6,360 characters, paid only on a turn with a link).
- Overseeing fixed five plumbing bugs, the largest that the agent loop ran
  tools an agent was never offered (now refused at dispatch).
- Five live runs on `intake` (a real defect: it names a source but does not
  always start reading it; 3 of 5 reference runs). The plumbing held every time;
  the gpt-oss judges did not, and each run's failure became a fix:
  1. right decision; the reflector did intake's job with intake's tools (dispatch
     ran unoffered tools: fixed in the loop);
  2. "keep" (the reference was fine that time); editors cited WHY instead of the
     run, the composer quoted an edit as evidence (both must quote a step);
  3. wrong: an editor read a probe STUB as a failure, the composer adopted the
     regressing edit, the reflector wrote a false library lesson (removed; lessons
     now enter the library only on the owner's adoption; editors told what a STUB is);
  4. right outcome, runs conflated by the composer and reflector (every
     trajectory now opens with a one-line CALLS sequence);
  5. clean: defect shown, both candidates fixed it, composer cited the right
     run's step, reflector filed one adoption todo.
- Adopted 2026-10-23: run 5's edit, applied by hand to `intake.yaml` (its composed
  prompt carried the editor's TOOLS list and the test task appended, so it was not
  copied whole); 4 of 4 probes now start reading the source (was 2 of 5). The
  lesson is `harness-library/intake`. Follow-ups are brain-ops todos.
- harness-candidates ships off (`enabled: false`): turn it on once more runs
  come out like run 5, or with a stronger judge model for composer/reflector.
- Not done: the known-answer test (needs a `base harness` input to start from
  the round-1 intake prompt); the front door cannot be probed (not spawnable).
