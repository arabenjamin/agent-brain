// The developer plugin's Neo4j constraints and indexes, applied by its
// `start`. Statements end with `;` and must be IF NOT EXISTS.

// Architecture snapshots (arch.rs).
CREATE CONSTRAINT arch_snapshot_id IF NOT EXISTS FOR (a:ArchSnapshot) REQUIRE a.id IS UNIQUE;
CREATE INDEX arch_snapshot_taken_idx IF NOT EXISTS FOR (a:ArchSnapshot) ON (a.taken_at);

// Dev notes (dev_notes.rs): the label's constraint and its own indexes, so dev
// history never mixes with :Note retrieval.
CREATE CONSTRAINT dev_note_id IF NOT EXISTS FOR (d:DevNote) REQUIRE d.id IS UNIQUE;
CREATE VECTOR INDEX dev_note_embeddings IF NOT EXISTS FOR (d:DevNote) ON (d.embedding)
  OPTIONS {indexConfig: {`vector.dimensions`: 1024, `vector.similarity_function`: 'cosine'}};
CREATE FULLTEXT INDEX dev_note_fulltext IF NOT EXISTS FOR (d:DevNote)
  ON EACH [d.title, d.content, d.subsystem];
