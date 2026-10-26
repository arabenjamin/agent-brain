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
// Take a snapshot first, then run with the brain up:
//   export $(grep NEO4J_PASSWORD .env)
//   docker exec -i agent-brain-neo4j-1 cypher-shell -u neo4j -p "$NEO4J_PASSWORD" \
//     < scripts/retire_stale_news_inferences.cypher

:param ids => ['caae447c-70f9-46b1-a7ce-957426dedc05', '3a4917d7-984f-4f9c-a8a8-7f755d4e8f59', 'cc654bb6-7d86-448e-835b-7e13d562cbc7', '928dd7b1-9750-4eea-bb04-c76b4d362d32', '886b2ae5', '3e9c6792', 'cc58bf88', 'c9fe9a35', 'd7a9ed5d', '67435b26-be57-4814-a1b5-e883904f194b', '57c5f955-85a1-4a0e-9b65-63489cb4fb31'];

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
