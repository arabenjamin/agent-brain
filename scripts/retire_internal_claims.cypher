// Retire claims about the brain itself (2026-10-16). The daily brief's self
// sections ("The brain logged 176 notes", "One dead-letter job was recorded")
// were run through claim extraction, and the verifier searched the web for
// them: one was even "refuted". The brain's own ledger answers those; they are
// not claims about the world. Code fix: plugins/epistemics/internal-claims.yaml
// + crate/src/internal.rs (never extracted, never verified) and the daily
// brief's `exclude_sections`.
//
// $ids: the 40 found 2026-10-16 (30 by the detector, 10 by hand: brief
// bookkeeping no rule catches, and August's claims about the brief itself).
// Backup of each claim with its edges, and of its evidence notes:
// workspace/backups/internal-claims-2026-10-16.json. Run with the brain up:
//   export $(grep NEO4J_PASSWORD .env)
//   docker exec -i agent-brain-neo4j-1 cypher-shell -u neo4j -p "$NEO4J_PASSWORD" \
//     -P "ids => $(cat ids.json)" < scripts/retire_internal_claims.cypher

// -- Preview -------------------------------------------------------------------
MATCH (c:Note {note_type: 'claim'}) WHERE c.id IN $ids
RETURN count(c) AS claims_to_delete;

// -- Their evidence notes (each attempt stores one, linked or not), when no
//    other claim relies on them -------------------------------------------------
MATCH (c:Note {note_type: 'claim'}) WHERE c.id IN $ids
WITH collect(c.content) AS texts
MATCH (e:Note {source_context: 'claim_evidence'})
WHERE any(t IN texts WHERE e.content STARTS WITH 'Evidence gathered for claim: ' + t
      OR e.content STARTS WITH 'Evidence gathered for claim (via the SearXNG fallback — keyed engines were unavailable): ' + t)
  AND NOT EXISTS {
    MATCH (o:Note)-[:CORROBORATED_BY|CONTRADICTED_BY]->(e) WHERE NOT o.id IN $ids
  }
OPTIONAL MATCH (chunk:Note)-[:PART_OF]->(e)
DETACH DELETE chunk, e
RETURN count(DISTINCT e) AS evidence_deleted;

// -- The claims ------------------------------------------------------------------
MATCH (c:Note {note_type: 'claim'}) WHERE c.id IN $ids
DETACH DELETE c
RETURN count(c) AS claims_deleted;
