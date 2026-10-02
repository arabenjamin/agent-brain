// Backfill (:Media)-[:FROM_SOURCE]->(:MediaSource) for every video ingested
// before 2026-10-01. The watch task carried only the URL, so the source name
// never reached ingest_media and no video was ever linked (0 of 1,089).
// A YouTube MediaSource's `ref` is the channel id every :Media node carries.
// Idempotent (MERGE); safe to run with the brain up:
//   export $(grep NEO4J_PASSWORD .env)
//   docker exec -i agent-brain-neo4j-1 cypher-shell -u neo4j -p "$NEO4J_PASSWORD" \
//     < scripts/backfill_media_from_source.cypher

MATCH (m:Media) WHERE coalesce(m.channel_id, '') <> ''
  AND NOT (m)-[:FROM_SOURCE]->()
MATCH (s:MediaSource {ref: m.channel_id})
MERGE (m)-[:FROM_SOURCE]->(s)
SET m.source_media_name = s.name
RETURN count(m) AS linked;

MATCH (m:Media)
RETURN count(m) AS videos,
       sum(CASE WHEN EXISTS { (m)-[:FROM_SOURCE]->() } THEN 1 ELSE 0 END) AS linked_total;
