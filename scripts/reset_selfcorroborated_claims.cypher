// Reset claim corroborations recorded while the verifier could read our own notes.
//
// From 2026-09-12 (graph-first block added to search_web) until the verifier
// fix (search_web prior_knowledge:false + support_rejected_unsourced), every
// `verify_claims` judge read a "WHAT WE ALREADY KNOW" block that quoted our own
// notes — often the claim itself. The corroboration rate went 41% → 84%, and 169
// of 303 corroborations cite a graph note rather than a URL. The input was
// contaminated for EVERY verification in the window, so all corroborations in it
// are reset, not only the 169; the fixed verifier re-earns the real ones.
//
// Run AFTER the verifier fix is deployed. The upper bound is the deploy time of
// 8f8bd66 (2026-09-26 17:48 UTC): edges after it came from the fixed verifier
// and must survive, so the verifier need not be paused. Run the three statements one at a time; read step 1 first.
// See project-docs/BRAIN_ASSESSMENT_FIXES_PLAN.md, Step 2.

// 1. Count (dry run)
MATCH (c:Note {note_type:'claim'})-[r:CORROBORATED_BY]->()
WHERE r.found_at >= datetime('2026-09-12') AND r.found_at < datetime('2026-09-26T17:48:00Z')
RETURN count(DISTINCT c) AS claims, count(r) AS edges;

// 2. Tag the evidence notes behind those edges. They stay as the audit trail.
MATCH (c:Note {note_type:'claim'})-[r:CORROBORATED_BY]->(e:Note)
WHERE r.found_at >= datetime('2026-09-12') AND r.found_at < datetime('2026-09-26T17:48:00Z')
SET e.contaminated = true
RETURN count(DISTINCT e) AS evidence_notes_tagged;

// 3. Delete the edges and re-derive status the way recompute_status does
//    (services/claims.rs: ClaimStatus::from_evidence). corroboration_tier needs
//    classify_domains (a graph lookup), so it is cleared where corroboration
//    remains and set to 'none' where none does; the next recompute restores it.
//    Clearing last_verify_attempt_at puts these claims at the front of the
//    verification queue (~1 web search each, spread over the daily sweeps).
MATCH (c:Note {note_type:'claim'})-[r:CORROBORATED_BY]->()
WHERE r.found_at >= datetime('2026-09-12') AND r.found_at < datetime('2026-09-26T17:48:00Z')
WITH c, collect(r) AS rs
FOREACH (x IN rs | DELETE x)
WITH DISTINCT c
OPTIONAL MATCH (c)-[cor:CORROBORATED_BY]->()
WITH c, count(cor) AS corr, collect(cor) AS cors
OPTIONAL MATCH (c)-[con:CONTRADICTED_BY]->()
WITH c, corr, cors, count(con) AS contra
WITH c, corr, contra,
     reduce(acc = [], r IN cors |
       acc + [d IN COALESCE(r.domains, []) WHERE NOT d IN acc]) AS domains
SET c.claim_status = CASE
      WHEN corr > 0 AND contra > 0 THEN 'disputed'
      WHEN corr > 0 THEN 'corroborated'
      WHEN contra > 0 THEN 'refuted'
      ELSE 'unverified' END,
    c.corroborating_count = corr,
    c.contradicting_count = contra,
    c.corroborating_domains = domains,
    c.corroboration_tier = CASE WHEN corr > 0 THEN null ELSE 'none' END,
    c.last_verify_attempt_at = null,
    c.reset_reason = 'self-corroboration via search_web prior_knowledge, 2026-09-12..2026-09-24'
RETURN c.claim_status AS status, count(*) AS n;
