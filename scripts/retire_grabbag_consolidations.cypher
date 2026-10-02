// Retire every consolidated note built by the old selection (fixed 2026-10-01
// in services/knowledge.rs). The old consolidate_memories summarised the top-K
// due notes by similarity to a fixed generic topic ("recent experiences and
// knowledge"), with no clustering and with housekeeping notes (chat turns, todo
// reviews, adversarial JSON) admitted. Every result was a grab-bag of unrelated
// records presented as one paragraph of knowledge — e.g. a Hugging Face attack,
// birds of prey, and a film scene's "subtle wave" in one note. 295 existed,
// retrieved 1,749 times.
//
// Deleting a consolidated note also deletes its SUMMARIZED_BY edges, which is
// what makes its sources eligible for the new seed-and-cluster consolidation.
// The sources themselves are untouched.
//
// CUTOFF is when the clustering build went live; set it before running so a
// cluster-built note is never deleted. Take a snapshot first (the brain's
// pre_consolidate snapshots are MERGE-restorable) and run with the brain up:
//   export $(grep NEO4J_PASSWORD .env)
//   docker exec -i agent-brain-neo4j-1 cypher-shell -u neo4j -p "$NEO4J_PASSWORD" \
//     -P 'cutoff => datetime("2026-10-01T00:00:00Z")' \
//     < scripts/retire_grabbag_consolidations.cypher

// -- Preview -------------------------------------------------------------------
MATCH (c:Note {note_type: 'consolidated'})
WHERE c.created_at < $cutoff
OPTIONAL MATCH (chunk:Note)-[:PART_OF]->(c)
RETURN count(DISTINCT c) AS consolidated_to_delete, count(chunk) AS chunks_to_delete;

// -- Chunks first (they would otherwise be orphaned) ----------------------------
MATCH (chunk:Note)-[:PART_OF]->(c:Note {note_type: 'consolidated'})
WHERE c.created_at < $cutoff
CALL {
  WITH chunk
  DETACH DELETE chunk
} IN TRANSACTIONS OF 200 ROWS;

// -- The consolidated notes ----------------------------------------------------
MATCH (c:Note {note_type: 'consolidated'})
WHERE c.created_at < $cutoff
CALL {
  WITH c
  DETACH DELETE c
} IN TRANSACTIONS OF 200 ROWS;

// The freed sources need no rescheduling: each was bumped +30d when it was
// consolidated, at a different time, so they come due spread across a month.

// -- Verify --------------------------------------------------------------------
MATCH (c:Note {note_type: 'consolidated'})
RETURN count(c) AS consolidated_remaining;
