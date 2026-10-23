// Reset claim refutations recorded before the contradiction gate.
//
// Until 2026-10-22 a judge's "contradicts" went straight to a CONTRADICTED_BY
// edge, unchecked: of 68 refutations graded by hand, 53 were wrong (the
// evidence said the same thing in other units, or did not mention the claim
// at all). The gate (claims::confirm_contradiction + the grounding veto in
// claim_skill.rs) writes `quote` and `conflict` on every edge it lets through,
// so an edge without `quote` is one no gate checked. Those are reset; the
// gated verifier re-judges the claims from fresh evidence.
//
// Run AFTER the gate is deployed (edges written before it have no quote; edges
// after it always do, so the verifier need not be paused). Run the three
// statements one at a time; read step 1 first.

// 1. Count (dry run)
MATCH (c:Note {note_type:'claim'})-[r:CONTRADICTED_BY]->()
WHERE r.quote IS NULL
RETURN c.claim_status AS status, count(DISTINCT c) AS claims, count(r) AS edges;

// 2. Tag the evidence notes behind those edges. They stay as the audit trail.
MATCH (c:Note {note_type:'claim'})-[r:CONTRADICTED_BY]->(e:Note)
WHERE r.quote IS NULL
SET e.ungated_contradiction = true
RETURN count(DISTINCT e) AS evidence_notes_tagged;

// 3. Delete the edges and re-derive status as recompute_status does
//    (ClaimStatus::from_evidence). Corroboration is untouched, so the domains
//    and tier stay. Clearing last_verify_attempt_at puts the claims at the
//    front of the verification queue (one web search each).
MATCH (c:Note {note_type:'claim'})-[r:CONTRADICTED_BY]->()
WHERE r.quote IS NULL
WITH c, collect(r) AS rs
FOREACH (x IN rs | DELETE x)
WITH DISTINCT c
OPTIONAL MATCH (c)-[cor:CORROBORATED_BY]->()
WITH c, count(cor) AS corr
OPTIONAL MATCH (c)-[con:CONTRADICTED_BY]->()
WITH c, corr, count(con) AS contra
SET c.claim_status = CASE
      WHEN corr > 0 AND contra > 0 THEN 'disputed'
      WHEN corr > 0 THEN 'corroborated'
      WHEN contra > 0 THEN 'refuted'
      ELSE 'unverified' END,
    c.contradicting_count = contra,
    c.last_verify_attempt_at = null,
    c.reset_reason = 'ungated contradiction, before 2026-10-22'
RETURN c.claim_status AS status, count(*) AS n;
