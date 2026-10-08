// One-off backfill, 2026-10-07 (deep-dive "1 passed" relayed as success).
// Run AFTER the image with the failed-verification label is deployed:
//   docker exec -i agent-brain-neo4j-1 cypher-shell -u neo4j -p password \
//     < scripts/backfill_failed_verification.cypher
// Both statements are idempotent.

// D1. Mark agent answers that failed verification, and their chunks, so
// retrieval labels them FAILED VERIFICATION. The reason is the text between
// the marker and the sentence agent_runner appends after it.
// Expected on 2026-10-07: 14 parents, 33 chunks.
MATCH (p:Note)
WHERE p.verification IS NULL
  AND (p.content STARTS WITH 'UNVERIFIED —'
       OR p.content =~ '(?s)^## [^\n]*\n\nUNVERIFIED — .*')
WITH p, split(split(p.content, 'UNVERIFIED — ')[1],
              '. Treat everything below as unchecked')[0] AS reason
SET p.verification = 'failed', p.verification_reason = reason
WITH p, reason
OPTIONAL MATCH (c:Note)-[:PART_OF]->(p)
FOREACH (_ IN CASE WHEN c IS NULL THEN [] ELSE [1] END |
  SET c.verification = 'failed', c.verification_reason = reason)
RETURN count(DISTINCT p) AS parents, count(c) AS chunks;

// D2. Deep-dive chunks stored as `semantic` under an `unsourced_synthesis`
// parent (written before chunks inherited the parent type, ~2026-10-06).
// Expected on 2026-10-07: 13.
MATCH (c:Note)-[:PART_OF]->(p:Note)
WHERE p.source_context = 'deep_dive'
  AND COALESCE(c.note_type, 'semantic') <> COALESCE(p.note_type, 'semantic')
SET c.note_type = p.note_type
RETURN count(c) AS retyped;
