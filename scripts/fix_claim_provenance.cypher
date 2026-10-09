// Relabel extracted claims: provenance 'user_input' → 'synthesis_inference'.
//
// Until HBI plan Phase 8 (2026-10-08), store_claim (services/claims.rs) wrote
// every claim with provenance 'user_input', the label for something a person
// stored. Claims are extracted from articles, videos and reports by an LLM, and
// recall shows the label to the model, so all 4,510 told it a person had
// asserted them. Nothing filters or weights on provenance; this changes the
// label only.
//
// store_claim is the only writer of note_type 'claim', so every claim is
// extracted. Run AFTER the fixed store_claim is deployed, or claims written in
// between keep the old label. Idempotent: a second run matches nothing.

// 1. Count (dry run)
MATCH (c:Note {note_type: 'claim'})
RETURN c.provenance AS provenance, count(*) AS claims;

// 2. Relabel
MATCH (c:Note {note_type: 'claim', provenance: 'user_input'})
SET c.provenance = 'synthesis_inference'
RETURN count(c) AS relabelled;
