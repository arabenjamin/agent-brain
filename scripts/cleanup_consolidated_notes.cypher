// Cleanup: remove redundant consolidated notes produced by the consolidation
// re-selection loop (fixed 2026-08-03 in services/knowledge.rs — source selection
// now honours next_review_at, so the same notes are no longer re-summarized
// every cycle; single source notes had accumulated 1500+ SUMMARIZED_BY edges).
//
// Keeps the 25 newest consolidated notes; deletes the rest with their edges.
//
// Run the preview first, then the delete:
//   export $(grep NEO4J_PASSWORD .env)
//   docker exec -i agent-brain-neo4j-1 cypher-shell -u neo4j -p "$NEO4J_PASSWORD" \
//     < scripts/cleanup_consolidated_notes.cypher
//
// (cypher-shell runs each ;-terminated statement in its own implicit
// transaction, which CALL ... IN TRANSACTIONS requires.)

// -- Preview: how many notes will be deleted --------------------------------
MATCH (c:Note {note_type: 'consolidated'})
WITH c ORDER BY c.created_at DESC
SKIP 25
RETURN count(c) AS notes_to_delete;

// -- Delete in batches ------------------------------------------------------
MATCH (c:Note {note_type: 'consolidated'})
WITH c ORDER BY c.created_at DESC
SKIP 25
CALL {
  WITH c
  DETACH DELETE c
} IN TRANSACTIONS OF 200 ROWS;

// -- Reset the spaced-rep schedule on the notes the loop kept re-summarizing,
// -- so they don't all come due at once when their +30d bumps expire.
// -- Staggers reviews over 30 days by hashing the note id.
MATCH (n:Note)
WHERE n.next_review_at IS NOT NULL
  AND NOT coalesce(n.note_type, 'semantic') IN ['consolidated']
  AND EXISTS { MATCH (n)-[:SUMMARIZED_BY]->() }
SET n.next_review_at = datetime() + duration({days: 1 + abs(id(n) % 30)});

// -- Verify -----------------------------------------------------------------
MATCH (c:Note {note_type: 'consolidated'})
RETURN count(c) AS remaining_consolidated;
