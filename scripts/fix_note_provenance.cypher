// Relabel notes the brain wrote that say a person wrote them (HBI plan Phase 9).
//
// Until 2026-10-08 store_note defaulted to provenance 'user_input' whenever the
// caller passed none, and store_episodic_note hardcoded it. On 2026-10-08 that
// was 11,513 notes (3,979 video summaries, 3,753 scheduler dispatch records,
// 2,096 claim-evidence records, …), plus 769 with no provenance at all. Recall
// shows the label to the model. The code now decides it from the caller
// (knowledge::effective_provenance); this fixes the notes written before.
//
//   user_input           kept only for a person's text: source_context 'hbi'
//                        (written in the HBI) and 'resume' (the user's résumé)
//   system_record        records the brain's code builds: scheduler_dispatch,
//                        claim_evidence, code_version …, git-hook …
//   synthesis_inference  everything else; a model chose the words
//
// Each relabelled note keeps the old value in provenance_was ('none' when it
// had none) and gets provenance_relabelled_at, so the change can be audited or
// undone: SET n.provenance = CASE n.provenance_was WHEN 'none' THEN null
// ELSE n.provenance_was END. Run AFTER the code fix is deployed. Idempotent.

// 1. Dry run: what would change, by writer
MATCH (n:Note)
WHERE (n.provenance = 'user_input' OR n.provenance IS NULL)
  AND NOT coalesce(n.source_context, '') IN ['hbi', 'resume']
WITH n, CASE
    WHEN n.source_context IN ['scheduler_dispatch', 'claim_evidence']
      OR n.source_context STARTS WITH 'code_version '
      OR n.source_context STARTS WITH 'git-hook ' THEN 'system_record'
    ELSE 'synthesis_inference' END AS becomes
RETURN becomes, coalesce(n.provenance, 'none') AS was, count(*) AS notes
ORDER BY notes DESC;

// 2. Apply
MATCH (n:Note)
WHERE (n.provenance = 'user_input' OR n.provenance IS NULL)
  AND NOT coalesce(n.source_context, '') IN ['hbi', 'resume']
SET n.provenance_was = coalesce(n.provenance, 'none'),
    n.provenance_relabelled_at = datetime(),
    n.provenance = CASE
      WHEN n.source_context IN ['scheduler_dispatch', 'claim_evidence']
        OR n.source_context STARTS WITH 'code_version '
        OR n.source_context STARTS WITH 'git-hook ' THEN 'system_record'
      ELSE 'synthesis_inference' END
RETURN n.provenance AS provenance, count(*) AS relabelled;

// 3. Check: every user_input note should now trace to a human path
MATCH (n:Note) WHERE n.provenance = 'user_input' OR n.provenance IS NULL
RETURN coalesce(n.provenance, 'none') AS provenance, n.source_context AS ctx, count(*) AS notes;
