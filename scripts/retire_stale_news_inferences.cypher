// Retire legacy news-prompt notes and the empty IBM research notes (2026-10-26).
//
// Nine inference notes from 2026-03-29 .. 2026-04-16 hold a whole news-analyst
// prompt, raw search results included, as their source_context (the old
// `reason` stored Q+A with the prompt as context). They are retrieved often
// (10-19 times each) and on 2026-10-25 one of them put three March articles
// into the daily brief's TECHNOLOGY & AI section through search_web's
// prior-knowledge block (dev_docs get yaml/schedules-daily-news-9).
//
// The two IBM notes are the 2026-10-25 default-workflow run that could not
// search the web: an inference and a `semantic` synthesis that both say
// "no information", stored as if they were knowledge.
//
// Also the 8 GB video (ugKAkCVBcz0, watched 2026-10-24): its source record,
// three chunks, three claims and their evidence notes carry caption garbles
// ("Qwen 3.635B" for Qwen 3.6 35B). Its :Media node goes too, so the video
// can be watched again with the description-aware summariser: run
// manage_workflow run video-learning {url: https://youtu.be/ugKAkCVBcz0}
// afterwards.
//
// Exported first: workspace/backups/retired-notes-2026-10-26.json (the 21
// notes with their relationships) and retired-media-ugKAkCVBcz0.json; the
// brain's snapshot_pre_prune_20261026_130420 also holds them all.
//
// Run with the brain up:
//   export $(grep NEO4J_PASSWORD .env)
//   docker exec -i agent-brain-neo4j-1 cypher-shell -u neo4j -p "$NEO4J_PASSWORD" \
//     < scripts/retire_stale_news_inferences.cypher

:param ids => ['caae447c-70f9-46b1-a7ce-957426dedc05', '3a4917d7-984f-4f9c-a8a8-7f755d4e8f59', 'cc654bb6-7d86-448e-835b-7e13d562cbc7', '928dd7b1-9750-4eea-bb04-c76b4d362d32', '886b2ae5', '3e9c6792', 'cc58bf88', 'c9fe9a35', 'd7a9ed5d', '67435b26-be57-4814-a1b5-e883904f194b', '57c5f955-85a1-4a0e-9b65-63489cb4fb31', '87ad97ae-ee5e-4a46-b46a-ce34b1de5e4f', '7ecb5042-b365-4d1b-926b-9af1ce4eca1c', '6dd0f174-896a-4279-b6ce-2af49b8294c2', 'e57310b7-df90-4a9a-961d-caef8a9f0afa', '8530db5d-43fc-4d32-86b5-0dd01567e356', 'f5954913-b484-4614-b0f3-2b87167ba941', '373f6641-3ef6-4463-94af-e92a2aca28e6', 'c59ea8f6-5b89-4259-891b-f5fa95a4c519', '0b2dd515-757f-41c9-b88d-c3806ca8bb8d', '86d57908-61ae-46fa-a9c2-54bd4360e6b0'];

// -- Preview: short ids are prefixes ------------------------------------------
MATCH (n:Note)
WHERE any(p IN $ids WHERE n.id STARTS WITH p)
OPTIONAL MATCH (chunk:Note)-[:PART_OF]->(n)
RETURN n.id AS id, n.note_type AS type, toString(n.created_at) AS created,
       count(chunk) AS chunks, left(n.source_context, 60) AS source_context;

// -- Chunks first (none expected) ----------------------------------------------
MATCH (chunk:Note)-[:PART_OF]->(n:Note)
WHERE any(p IN $ids WHERE n.id STARTS WITH p)
DETACH DELETE chunk;

// -- The notes -----------------------------------------------------------------
MATCH (n:Note)
WHERE any(p IN $ids WHERE n.id STARTS WITH p)
DETACH DELETE n
RETURN count(*) AS deleted;

// -- The 8 GB video's :Media node, so it can be watched again -------------------
MATCH (m:Media {id: 'ugKAkCVBcz0'})
DETACH DELETE m
RETURN count(*) AS media_deleted;
