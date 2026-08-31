// Retype the pre-2026-08-10 "Gap synthesis" notes from 'semantic' to
// 'unsourced_synthesis'.
//
// WHY: before 2026-08-10 `chains/fill-knowledge-gap.yaml` searched only internal
// notes, which cannot fill a knowledge gap by construction — a gap is precisely
// what the graph does not contain. The chain answered from the model's own prior
// and stored the result as `semantic`: "knowledge the brain established". Those
// notes cite nothing. Measured at migration time: 380 parents, 0 containing a
// URL, 0 with a `source_context`, none carrying the `## ANSWER` / `## WHAT THIS
// ADDS` / `## STILL UNKNOWN` headings the fixed chain emits. The 648 gap notes
// written after the fix are 95% cited and are deliberately NOT touched here.
//
// The fix landed in the chain; the notes it had already written stayed, and
// `label_claims` labels by type, so they kept reaching reasoning unlabelled and
// indistinguishable from cited material. On 2026-08-24 one of them — asserting a
// "clear correlation" between HBM/CoWoS scarcity and SLM adoption — was the top
// hit for a chat query and was relayed as a confirmed NIA finding at "Confidence:
// High". The `tech_dependency_synthesis` note that spawned it had concluded
// INSUFFICIENT EVIDENCE and named that exact correlation as its WEAKEST LINK.
// Uncertainty became a gap task, the gap task produced unsourced prose, and the
// prose came back as the answer to the question it had failed to settle.
//
// WHAT THIS CHANGES: type only. Content, embeddings, timestamps and every edge
// are left exactly as they are — provenance is the thing being repaired, so
// nothing is deleted. `unsourced_synthesis` is labelled on retrieval
// ("UNSOURCED SYNTHESIS — the brain's own reasoning, cites no source") and is
// excluded from consolidation source selection, so these can no longer be
// rewritten into a `consolidated` summary that drops the label with the type.
//
// Chunks are retyped too. `store_note` splits long content into
// `(:Note)-[:PART_OF]->(:Note)` children which are independently retrievable, so
// leaving them `semantic` would keep an unlabelled fragment of every one of
// these notes in circulation — the same "labelled in one retrieval path but not
// the other" failure that made the original claim labelling ineffective.
//
// SAFETY: idempotent. Both statements are guarded on
// `COALESCE(note_type,'semantic') = 'semantic'`, so re-running matches only what
// has not already been converted and a partial run can simply be repeated. The
// date and content guards are evaluated against the parent in both statements,
// so a chunk is converted only when its parent qualifies.
//
// Run with:
//   docker exec -i agent-brain-neo4j-1 cypher-shell -u "$NEO4J_USER" \
//     -p "$NEO4J_PASSWORD" -f /path/to/this/file
// Safe to run with the brain up: nothing writes `## Gap synthesis` notes dated
// before 2026-08-10 any more, so there is no concurrent writer to race.

// --- parents ----------------------------------------------------------------
MATCH (n:Note)
WHERE n.content STARTS WITH '## Gap synthesis'
  AND n.created_at < datetime('2026-08-10T00:00:00Z')
  AND COALESCE(n.note_type, 'semantic') = 'semantic'
  AND NOT (n)-[:PART_OF]->()
SET n.note_type = 'unsourced_synthesis',
    n.retyped_at = datetime(),
    n.retyped_reason = 'pre-2026-08-10 fill-knowledge-gap chain: internal-notes-only, no external source'
RETURN count(*) AS parents_retyped;

// --- chunks (guards evaluated on the parent) ---------------------------------
MATCH (c:Note)-[:PART_OF]->(p:Note)
WHERE p.content STARTS WITH '## Gap synthesis'
  AND p.created_at < datetime('2026-08-10T00:00:00Z')
  AND COALESCE(c.note_type, 'semantic') = 'semantic'
SET c.note_type = 'unsourced_synthesis',
    c.retyped_at = datetime(),
    c.retyped_reason = 'chunk of a pre-2026-08-10 unsourced gap synthesis note'
RETURN count(*) AS chunks_retyped;

// --- verification ------------------------------------------------------------
// Expect: 0 rows still typed 'semantic' matching the predicate.
MATCH (n:Note)
WHERE n.content STARTS WITH '## Gap synthesis'
  AND n.created_at < datetime('2026-08-10T00:00:00Z')
  AND COALESCE(n.note_type, 'semantic') = 'semantic'
RETURN count(*) AS remaining_semantic_should_be_zero;

// Expect: post-2026-08-10 gap notes untouched and still 'semantic'.
MATCH (n:Note)
WHERE n.content STARTS WITH '## Gap synthesis'
  AND n.created_at >= datetime('2026-08-10T00:00:00Z')
RETURN COALESCE(n.note_type, 'semantic') AS note_type, count(*) AS c
ORDER BY c DESC;
