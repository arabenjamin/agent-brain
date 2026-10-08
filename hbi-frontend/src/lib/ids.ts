/** Graph ids (notes, entities, tasks, jobs) are UUIDs. */
const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

/** The query is a bare id (surrounding whitespace allowed), not a search phrase. */
export function asEntityId(query: string): string | null {
  const q = query.trim();
  return UUID.test(q) ? q.toLowerCase() : null;
}
