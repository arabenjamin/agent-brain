// Retype the 2026-08-31 chat-authored mesh "deep dive" notes from 'semantic'
// to 'unsourced_synthesis'.
//
// WHY: on 2026-08-31 a `/chat` session on `gpt-oss:120b-cloud` was asked to
// create technical deep-dives on Reticulum, "metastatic", and Tailscale. It ran
// ZERO retrieval — confirmed against the DuckDB `search_usage` ledger, whose
// last row that day (13:09 EDT) predates the turn (13:34 EDT) by 25 minutes —
// and wrote all three from the model's own prior, each closing with a fabricated
// `Sources:` line.
//
// One of the three does not exist at all. "Metastatic" is a typo of *Meshtastic*
// that entered the graph on 2026-08-11 in a Todo snapshot ("mesh networks (e.g.,
// metastatic and reticulum)"). Rather than recognising the typo or flagging the
// term as unknown, the model invented a Rust mesh-networking crate around it —
// a gossip protocol, a Noise XX handshake, `cargo add metastatic`, an async
// constructor, and a citation to a GitHub repository that has never existed.
// The Reticulum notes are wrong in the subtler way that matters more: the real
// PyPI package is `rns` and the daemon is `rnsd`, not `pip install reticulum` /
// `reticulum-keygen` / `reticulum -c reticulum.conf` as stored.
//
// All 15 were stored as `note_type: semantic` — the one type `label_claims`
// deliberately does NOT mark on retrieval, because it means "knowledge the
// brain established". Left alone they are indistinguishable, at retrieval time,
// from material the brain actually verified, which is precisely how the
// `unsourced_synthesis` migration of 2026-08-10 describes a laundering cycle
// starting.
//
// WHAT THIS CHANGES: type only. Content, embeddings, timestamps, and every edge
// (MENTIONS, RELATES_TO, PART_OF) are left exactly as they are. Nothing is
// deleted: the record of what happened is the evidence for the write guard that
// now prevents it (`TurnWriteGuard` in `clients/chat.rs`), and a deleted note
// cannot be re-read later to check that claim.
//
// Chunks are retyped too. `store_note` splits long content into
// `(:Note)-[:PART_OF]->(:Note)` children which are independently retrievable
// and which inherit `source_context` from their parent, so 10 of these 15 are
// chunks. Leaving them `semantic` would keep an unlabelled fragment of every
// fabricated note in circulation — the same "labelled in one retrieval path but
// not the other" failure that made the original claim labelling ineffective.
// Because chunks inherit `source_context`, the single MATCH below catches
// parents and children alike.
//
// SAFETY: idempotent. Guarded on `COALESCE(note_type,'semantic') = 'semantic'`,
// so re-running matches only what has not already been converted and a partial
// run can simply be repeated. Scoped to three exact `source_context` values
// written by one turn, so it cannot reach any other note.
//
// RUN:
//   docker exec -i agent-brain-neo4j-1 cypher-shell -u neo4j -p "$NEO4J_PASSWORD" \
//     < scripts/migrate_fabricated_mesh_notes.cypher

MATCH (n:Note)
WHERE n.source_context IN [
        'technical_deep_dive_metastatic',
        'technical_deep_dive_reticulum',
        'technical_deep_dive_tailscale'
      ]
  AND COALESCE(n.note_type, 'semantic') = 'semantic'
SET n.note_type = 'unsourced_synthesis'
RETURN n.source_context AS source_context,
       count(*)         AS retyped
ORDER BY source_context;
