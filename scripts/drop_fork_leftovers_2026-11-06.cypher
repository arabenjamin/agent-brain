// Drop what the original agent-api fork left in the graph (owner, 2026-11-06;
// CODEBASE_SPLIT_PLAN § 8.8). Eight labels no code reads or writes:
// ApiCredential, Endpoint, HealingEvent, ModelSpec, Parameter, Resource,
// Schema, DynamicTool. On 2026-11-06: 185 nodes (Parameter 117, Schema 66,
// one ApiCredential, one HealingEvent), none with a relationship; 7
// constraints; 6 indexes. Backup, with every createStatement:
// workspace/backups/fork-leftovers-2026-11-06.json. Idempotent: run each
// statement on its own (schema and data changes cannot share a transaction).

// 1. Count (dry run)
MATCH (n) WHERE any(l IN labels(n) WHERE l IN ['ApiCredential','Endpoint','HealingEvent','ModelSpec','Parameter','Resource','Schema','DynamicTool'])
OPTIONAL MATCH (n)-[r]-()
RETURN labels(n)[0] AS label, count(DISTINCT n) AS nodes, count(r) AS relationships;

// 2. The nodes (none has a relationship; DETACH only guards a later surprise)
MATCH (n) WHERE any(l IN labels(n) WHERE l IN ['ApiCredential','Endpoint','HealingEvent','ModelSpec','Parameter','Resource','Schema','DynamicTool'])
DETACH DELETE n;

// 3. The constraints
DROP CONSTRAINT api_credential_api_name IF EXISTS;
DROP CONSTRAINT endpoint_id IF EXISTS;
DROP CONSTRAINT healing_event_id IF EXISTS;
DROP CONSTRAINT model_spec_name IF EXISTS;
DROP CONSTRAINT parameter_id IF EXISTS;
DROP CONSTRAINT resource_id IF EXISTS;
DROP CONSTRAINT schema_id IF EXISTS;

// 4. The indexes (auto-named: Endpoint.path, Parameter.name, Endpoint.method,
//    ModelSpec.provider, DynamicTool.created_at, Schema.name)
DROP INDEX index_3e70cd43 IF EXISTS;
DROP INDEX index_4b401a13 IF EXISTS;
DROP INDEX index_695bd8b9 IF EXISTS;
DROP INDEX index_bc48adff IF EXISTS;
DROP INDEX index_eadbcaaf IF EXISTS;
DROP INDEX index_ebca1545 IF EXISTS;
