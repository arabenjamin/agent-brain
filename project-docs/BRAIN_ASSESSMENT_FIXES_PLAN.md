# Brain assessment fixes — plan (2026-09-24)

Written late at night after an assessment of recent notes, claim verification, chats and logs. Work top to bottom; each step says how to check it worked.

**Progress (2026-09-26):** Step 6 done (compose line). Step 1 done in code (tests green; needs rebuild). Step 2 script written and parse-checked; **not run** — the live count had grown to 367 edges / 197 non-URL sources because the old verifier is still running, so deploy Step 1 first, then run it. Step 3 done (`ToolCallParse` in `clients/chat.rs`; the 03:42 payload is a test fixture, replayed end-to-end through `run_text_loop` against wiremock). Steps 4–5 await decisions.

**Before starting:** the working tree already has uncommitted changes (`.env.example`, `CLAUDE.md`, `README.md`, `contexts/GeneralDefault-Orchrestator.yaml`, `project-docs/STATUS.md`, `schemas/context_profile.json`, `ingress/`, `scratch.pad`). Commit or set those aside first so these fixes land as their own commits. Per the branch rules, update the docs (CLAUDE.md, this plan) in the same change as the code.

## What the assessment found

| # | Problem | Severity | Evidence |
|---|---|---|---|
| 1 | Claim verification uses our own notes as evidence | High | 84% of attempts corroborated since 09-12 vs 41% before; 169 of 303 corroborations since then cite a graph note, not a URL |
| 2 | A malformed tool call in chat fails silently, and the reply says it succeeded | High | Chat `375ks4hedk8mueyyaf5`, 03:42 UTC: "I am now setting this schedule up"; no schedule exists; one WARN in the log |
| 3 | Front door on Gemini answered without retrieval and invented citation IDs | High | Same chat, turns 7–9: one Gemini call, zero tools; cited `0020786f` and `75368a57`, which don't exist |
| 4 | GDELT schedule calls a URL that doesn't exist | Medium | `intelligence-agency-claims-monitor`: 14 of 15 runs since 09-08 returned 404, all recorded as completed |
| 5 | `schemas/` isn't mounted, so envelope validation is off | Medium | Startup WARN `schemas dir unreadable — envelope validation disabled` |
| 6 | SerpApi quota is spent | Low | Out of searches 09-23; the ladder falls back to SearXNG correctly |
| 7 | YouTube channel feeds 404 in bulk | Low | Documented throttling; tasks still created daily (12–22/day) |

## Step 1 — Stop the verifier from citing the graph (problem 1)

**Mechanism.** `verify_one` (`crates/app/src/skills/claims.rs`) calls `search_web`. Since 2026-09-12, `SearchSkill::with_prior_knowledge` (`skills/search.rs`) prefixes every successful result with a "WHAT WE ALREADY KNOW" block of matching notes. That block quotes the claim itself. `assess_evidence` reads it and answers "supports", often naming the claim's own note as `source`. `check_independence` then counts domains across the whole text, unrelated web results included, and passes. Example: the quote "My legs!" was corroborated by a sleep.com article on vivid dreams.

**Changes.**
1. `SearchInput` (`skills/search.rs:1505`): add `#[serde(default = "default_true")] prior_knowledge: bool`. Skip `with_prior_knowledge` when it's false, on all three return paths (lines ~981, ~995, ~1011). Add it to the tool's `input_schema` and describe it as meant for verifiers, which need external evidence only.
2. `verify_one`: pass `"prior_knowledge": false`.
3. Also in `verify_one`: after an `assess_evidence` "supports", reject the verdict unless `verdict.source` is a URL, or a domain, that appears among the search results' `link` fields. Record it as `support_rejected_unsourced`, following the `support_rejected_not_independent` pattern.
4. `evidence_domains` / `check_independence`: count domains from result `link` fields only, not from any text in the body, so a quoted note can't add domains.
5. Tests:
   - `search_web` with `prior_knowledge:false` returns no block.
   - A support whose source is a graph note is rejected.
   - Domains are counted from `link` fields only.

**Check it worked.** After a rebuild, run `verify_claims` with a small `limit`. New `claim_evidence` notes contain no "WHAT WE ALREADY KNOW". Over a day or two the corroboration rate should drop back toward the 40% range, and refutations should reappear.

## Step 2 — Undo the contaminated corroborations (problem 1, data)

The judge's input was contaminated for **every** verification since 09-12, not only the 169 edges whose source isn't a URL. So reset all corroborations recorded since then and let the fixed verifier re-earn the real ones. This is a separate deliverable from Step 1.

Create `scripts/reset_selfcorroborated_claims.cypher`. Count first, then run the reset with the brain stopped or the verifier paused:

```cypher
// 1. Count (dry run)
MATCH (c:Note {note_type:'claim'})-[r:CORROBORATED_BY]->()
WHERE r.found_at >= datetime('2026-09-12')
RETURN count(DISTINCT c) AS claims, count(r) AS edges;

// 2. Reset
MATCH (c:Note {note_type:'claim'})-[r:CORROBORATED_BY]->()
WHERE r.found_at >= datetime('2026-09-12')
WITH c, collect(r) AS rs
FOREACH (x IN rs | DELETE x)
WITH DISTINCT c
SET c.claim_status = CASE
      WHEN EXISTS { (c)-[:CORROBORATED_BY]->() } AND EXISTS { (c)-[:CONTRADICTED_BY]->() } THEN 'disputed'
      WHEN EXISTS { (c)-[:CORROBORATED_BY]->() } THEN 'corroborated'
      WHEN EXISTS { (c)-[:CONTRADICTED_BY]->() } THEN 'refuted'
      ELSE 'unverified' END,
    c.last_verify_attempt_at = NULL,
    c.reset_reason = 'self-corroboration via search_web prior_knowledge, 2026-09-12..2026-09-24'
RETURN c.claim_status AS status, count(*) AS n;
```

Before running it:
- Check that the `CASE` matches `recompute_status` in `services/claims.rs`, including whether it also sets `corroborating_count` / `contradicting_count`. If it does, set those here too.
- Setting `last_verify_attempt_at` to null puts these claims at the front of the verification queue. With about 300 claims, that's about 300 web searches spread over the daily sweeps.

Leave the old `claim_evidence` notes in place, since they're the audit trail, and optionally tag them `contaminated: true`.

## Step 3 — Tell the user when a tool call fails to parse (problem 2)

`extract_tool_call` (`clients/chat.rs:4017`) logs at WARN and returns `None`. The caller at `:3688` then treats the text as a finished answer, so the user reads "I am now setting this up" and nothing happened.

**Change.** When the text contains a `<tool_call>` opener but parsing fails, return an error variant instead of `None`. The loop then feeds a correction back to the model, as it does for other tool errors: "your tool call was not valid JSON: <serde error>; re-emit it." Allow one retry. If the retry also fails, append a visible line to the reply saying the tool call was malformed and did not run, and name the tool. Never let prose that promises an action stand when the action didn't run. `tool_trace_receipt` already exists for this kind of check, so reuse it.

**Test.** Feed the 03:42 malformed `manage_scheduled_task` payload (the third step object is never closed) and assert the turn does not end with a silent success.

## Step 4 — Front-door model choice (problem 3). Decision needed

In the same chat, gemma4:31b-cloud handed every question to `knowledge-worker` (turns 1–5). After the profile was saved at 03:34 UTC pinned to `gemini-2.5-flash`, Gemini answered turn 7 from conversation memory, with no tool calls and invented citation IDs.

Options:
- **A.** Pin the front door back to `gemma4:31b-cloud` (free Ollama Cloud, `no_log`, so the profile could also go back to `data_class: private`).
- **B.** Keep Gemini and add a rule: any answer with `[[id:…]]` citations must come from a turn in which a retrieval tool ran.
- **Structural either way:** after the reply is composed, check each cited `id:` against the graph and flag the ones that don't exist. This catches invented IDs whichever model is in use.

Recommendation: A plus the ID check. Measure against a twin before deciding, the way CLAUDE.md's "Going local" section does.

## Step 5 — GDELT (problem 4). Decision needed

The schedule calls `https://api.gdeltproject.org/api/v2/events/events`, which doesn't exist. Its `mode:"timeline"` and `timespan:"24"` aren't valid GDELT values either. The real keyless API is the DOC 2.0 API. A probe on 2026-09-24 of `…/api/v2/doc/doc?query=(Mossad OR GRU OR CIA) sourcelang:english&mode=artlist&format=json&timespan=24h` worked, but the results were noisy (a wrestling preview, Oscars coverage, sott.net).

Options:
- **Fix it for volume signals:** `mode=timelinevol` with `timespan=7d`, and feed the `reason` step only the spike. Treat GDELT as a "coverage is spiking" tripwire, never as evidence. If articles are needed, add `domainis:` filters from the `news` SourceList.
- **Disable it:** `SET s.enabled=false, s.paused_reason='GDELT endpoint 404 since 09-08; see BRAIN_ASSESSMENT_FIXES_PLAN'`.

Either way:
- Delete or label the 15 "no data" inference notes (`Q: Based on the GDELT event data…`).
- Make the schedule's `http_request` step fail on a non-2xx status, or add a step that checks the result, so a 404 stops counting as completed. Longer term, this is a lint idea: an `http_request` step whose result is never checked.

## Step 6 — Mount `schemas/` (problem 5)

Add `- SCHEMAS_DIR=/home/agent/agent-brain/schemas` to the `agent-brain` environment in `docker-compose.yml`, next to `CHAINS_DIR`. The directory is already under the mounted repo; confirm with `docker exec agent-brain-agent-brain-1 ls /home/agent/agent-brain/schemas`. **Check it worked:** the startup WARN disappears.

## Step 7 — Search quota (problem 6). Optional

Google News through SerpApi works (143 of 149 calls succeeded in 30 days). Switching to the `serpapi` engine wouldn't save anything, because it uses the same 250-a-month account. What drained the quota was a burst of 88 `serpapi` calls on 08-31, on top of about 210 a month from the news brief.
- Optional: create a Google Custom Search key and CX (100 queries a day, free; enable the Custom Search API in the GCP project) and set `GOOGLE_API_KEY`/`GOOGLE_CX`. That gives a real Google option that doesn't share the SerpApi quota.
- Optional: find the source of the 08-31 burst (`search_usage` rows for that day) and cap it.

## Not doing

- **YouTube 404s (problem 7):** documented throttling, already retried; leave it.
- **The `nia-actor-motivation-monitoring` schedule from the chat:** it was never saved, and the draft had several defects:
  - `provider_hint` inside `arguments`.
  - `:Actor`/`:Threat` labels that don't exist.
  - Search results dropped before `reason`.
  - `source_record` used for the brain's own speculation.

  If we still want to watch actors' motives, design it on purpose, as a claim-based monitor of named, sourced reports with `unsourced_synthesis` for anything inferred, and only after Step 1. Otherwise the verifier would mark its speculation corroborated.

## Morning order

1. Step 6 (one line) and Step 1 (verifier fix, tests), then rebuild.
2. Step 2 (run the count query, then the reset), with the verifier fixed first.
3. Step 3 (tool-call parse failure).
4. Decide Steps 4 and 5, then apply.
5. Update CLAUDE.md: the Epistemics section (verifier input must be external only) and the Self-knowledge section (the graph-first block is for chat agents, not verifiers).
