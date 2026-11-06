// Re-derive refuted and disputed claims under the owner-confirmation rule
// (2026-11-05). A contradiction now refutes only once the owner confirms it
// (CONTRADICTED_BY.confirmed_at); until then the claim is `contested` and the
// weekly claim-refutation-review lists it. Every contradiction recorded before
// this has no confirmed_at, so its claim becomes contested. A claim marked
// refuted with no contradiction on record at all (3 on 2026-11-05) goes back
// to unverified. Mirrors ClaimStatus::from_evidence. Idempotent: run it once
// the new code is live, and again if a refutation was written in between.

// 1. Count (dry run)
MATCH (c:Note {note_type:'claim'}) WHERE c.claim_status IN ['refuted', 'disputed']
OPTIONAL MATCH (c)-[con:CONTRADICTED_BY]->() WHERE con.dismissed_at IS NULL
RETURN c.claim_status AS status,
       count(CASE WHEN con.confirmed_at IS NOT NULL THEN 1 END) AS confirmed_edges,
       count(CASE WHEN con IS NOT NULL AND con.confirmed_at IS NULL THEN 1 END) AS pending_edges,
       count(DISTINCT c) AS claims;

// 2. Re-derive
MATCH (c:Note {note_type:'claim'}) WHERE c.claim_status IN ['refuted', 'disputed']
OPTIONAL MATCH (c)-[cor:CORROBORATED_BY]->()
WITH c, count(cor) AS corr
OPTIONAL MATCH (c)-[con:CONTRADICTED_BY]->() WHERE con.dismissed_at IS NULL
WITH c, corr,
     count(CASE WHEN con.confirmed_at IS NOT NULL THEN 1 END) AS confirmed,
     count(CASE WHEN con IS NOT NULL AND con.confirmed_at IS NULL THEN 1 END) AS pending
SET c.claim_status = CASE
      WHEN confirmed > 0 AND corr > 0 THEN 'disputed'
      WHEN confirmed > 0 THEN 'refuted'
      WHEN pending > 0 THEN 'contested'
      WHEN corr > 0 THEN 'corroborated'
      ELSE 'unverified' END,
    c.contradicting_count = confirmed,
    c.contested_count = pending,
    c.reset_reason = CASE WHEN confirmed = 0 AND pending = 0
      THEN 'marked refuted with no contradiction on record (2026-11-05)'
      ELSE c.reset_reason END
RETURN c.claim_status AS status, count(*) AS n;
