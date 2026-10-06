// scripts/retype_null_result_notes.cypher
//
// Re-type research/gap notes whose answer is "the evidence did not cover it"
// from `semantic` to `episodic` (2026-10-03).
//
// These were stored as `semantic` — knowledge — and consolidation, which reads
// semantic notes only, merged them as if they were findings: 4 of the 8 sources
// of the "what is a knowledge gap" consolidation (retired 2026-10-03) were notes
// saying the sources contained nothing about a knowledge gap. As `episodic`
// they stay retrievable as a record of a search that came up empty, and leave
// consolidation's source pool.
//
// Predicate: a short note (< 900 chars) whose FIRST paragraph after the heading
// says the provided context/sources/results do not contain the answer. A note
// that answers first and lists what is missing afterwards (## STILL UNKNOWN) is
// not touched. `retyped_from` / `retyped_reason` make the change reversible:
//   MATCH (n:Note {retyped_reason:'null_result'}) SET n.note_type = n.retyped_from
//
// The write path is fixed separately: research steps now write a leading
// `NO FINDINGS` line and store_note stores such a note as episodic.
MATCH (n:Note {note_type: 'semantic'})
WHERE NOT (n)-[:PART_OF]->()
  AND size(n.content) < 900
  AND n.content =~ '(?is)^#+ [^\n]*\n+\s*(\*\*)?(the provided|based on the provided|regarding|the web search|the search)[^\n]{0,240}\\b(do|does) not (contain|provide|offer|include|mention|address)\\b.*'
SET n.note_type = 'episodic',
    n.retyped_from = 'semantic',
    n.retyped_reason = 'null_result',
    n.retyped_at = datetime()
RETURN n.source_context AS source_context, count(*) AS retyped
ORDER BY retyped DESC;
