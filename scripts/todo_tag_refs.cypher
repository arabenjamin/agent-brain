// Todo tags become references (todo fec2fc34, 2026-10-20; dev note
// todos/tags-are-references). Old free-word tags that name a real thing with
// the same meaning become kind:name and get their TAGGED link; every other
// free word stays as it is (shown as an old tag, removable, not addable).
// Not mapped on purpose: "claude" (the todos mean Claude Code, not the
// Claude entity), "planning" (an entity typed technology), "finetuning"
// (spelled unlike the "fine-tuning" entity). Idempotent.
//
//   docker compose exec -T neo4j cypher-shell -u neo4j -p "$NEO4J_PASSWORD" < scripts/todo_tag_refs.cypher

UNWIND [
  {old: '"agent-brain"', new: '"topic:agent-brain"', label: 'Topic', name: 'agent-brain'},
  {old: '"CI/CD"', new: '"entity:ci/cd"', label: 'Entity', name: 'ci/cd'}
] AS m
MATCH (t:Todo) WHERE t.tags CONTAINS m.old
SET t.tags = replace(t.tags, m.old, m.new)
WITH t, m
CALL (t, m) {
  MATCH (x) WHERE m.label IN labels(x) AND x.name = m.name
  MERGE (t)-[r:TAGGED]->(x) SET r.created_at = coalesce(r.created_at, datetime())
}
RETURN m.new AS tag, count(t) AS todos;
