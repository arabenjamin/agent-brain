//! `/api/graph`: the seeded neighbourhood the Graph panel draws.

use super::{RestState, graph};
use agent_brain_repository::Neo4jClient;
use axum::{Extension, Json, extract::Query, response::IntoResponse};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;

/// Most seeds `/api/graph` takes; more are ignored.
const GRAPH_MAX_SEEDS: usize = 60;

/// RELATES_TO neighbours kept per seed (most similar first).
const GRAPH_NEIGHBOURS_PER_SEED: i64 = 4;

/// Entities shown around a seeded graph (most-mentioned by its notes first).
const GRAPH_SEED_ENTITIES: i64 = 40;

/// Nodes for a seeded graph: the seed notes (flagged `seed`), each one's
/// nearest RELATES_TO neighbours, and the entities those notes mention most
/// (URLs and dates left out: they are noise at this scale).
async fn seeded_graph_nodes(neo4j: &Neo4jClient, seeds: &[String]) -> Vec<Value> {
    let label = |content: &str| -> String { content.chars().take(60).collect() };
    let mut nodes = Vec::new();
    let rows = neo4j
        .execute(
            neo4rs::query(
                "MATCH (s:Note) WHERE s.id IN $seeds \
                 OPTIONAL MATCH (s)-[r:RELATES_TO]-(m:Note) \
                 WITH s, m, r ORDER BY r.similarity DESC \
                 WITH s, collect(m)[0..$per] AS ms \
                 UNWIND ([s] + ms) AS n \
                 WITH DISTINCT n \
                 RETURN n.id AS id, n.content AS content, n.note_type AS note_type, \
                        toString(n.created_at) AS created_at",
            )
            .param("seeds", seeds.to_vec())
            .param("per", GRAPH_NEIGHBOURS_PER_SEED),
        )
        .await
        .unwrap_or_default();
    let seed_set: std::collections::HashSet<&str> = seeds.iter().map(String::as_str).collect();
    let mut note_ids = Vec::new();
    for row in rows {
        let id = row.get::<String>("id").unwrap_or_default();
        if id.is_empty() {
            continue;
        }
        let content = row.get::<String>("content").unwrap_or_default();
        nodes.push(json!({
            "id": id,
            "label": label(&content),
            "type": "note",
            "note_type": row.get::<String>("note_type").unwrap_or_else(|_| "semantic".to_string()),
            "created_at": row.get::<String>("created_at").unwrap_or_default(),
            "seed": seed_set.contains(id.as_str()),
        }));
        note_ids.push(id);
    }
    let entity_rows = neo4j
        .execute(
            neo4rs::query(
                "MATCH (n:Note)-[:MENTIONS]->(e:Entity) \
                 WHERE n.id IN $ids AND NOT e.entity_type IN ['url', 'date'] \
                 WITH e, count(DISTINCT n) AS c ORDER BY c DESC LIMIT $limit \
                 RETURN e.id AS id, e.name AS name, e.entity_type AS entity_type, \
                        toString(e.created_at) AS created_at",
            )
            .param("ids", note_ids)
            .param("limit", GRAPH_SEED_ENTITIES),
        )
        .await
        .unwrap_or_default();
    for row in entity_rows {
        nodes.push(json!({
            "id": row.get::<String>("id").unwrap_or_default(),
            "label": row.get::<String>("name").unwrap_or_default(),
            "type": "entity",
            "entity_type": row.get::<String>("entity_type").unwrap_or_else(|_| "unknown".to_string()),
            "created_at": row.get::<String>("created_at").unwrap_or_default(),
        }));
    }
    nodes
}

/// GET /api/graph?max_nodes=N | ?seeds=id,id — graph nodes and edges for visualisation.
pub async fn handle_get_graph(
    Extension(state): Extension<Arc<RestState>>,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let neo4j = graph!(state);

    let max_nodes: i64 = params
        .get("max_nodes")
        .and_then(|v| v.parse().ok())
        .unwrap_or(200);

    // `?seeds=a,b,c`: the neighbourhood of those notes (the HBI's search
    // results) instead of the most recently used part of the graph.
    let seeds: Vec<String> = params
        .get("seeds")
        .map(|s| {
            s.split(',')
                .map(|x| x.trim().to_string())
                .filter(|x| !x.is_empty())
                .take(GRAPH_MAX_SEEDS)
                .collect()
        })
        .unwrap_or_default();

    let mut nodes: Vec<Value> = Vec::new();

    if !seeds.is_empty() {
        nodes = seeded_graph_nodes(neo4j, &seeds).await;
    } else {
        // Notes
        let note_rows = neo4j
            .execute(
                neo4rs::query(
                    "MATCH (n:Note) \
                 RETURN n.id AS id, n.content AS content, n.note_type AS note_type, \
                        toString(n.created_at) AS created_at \
                 ORDER BY n.last_accessed_at DESC LIMIT $limit",
                )
                .param("limit", max_nodes),
            )
            .await
            .unwrap_or_default();
        for row in note_rows {
            let id = row.get::<String>("id").unwrap_or_default();
            let content = row.get::<String>("content").unwrap_or_default();
            let note_type = row
                .get::<String>("note_type")
                .unwrap_or_else(|_| "semantic".to_string());
            let created_at = row.get::<String>("created_at").unwrap_or_default();
            let label: String = content.chars().take(60).collect();
            nodes.push(json!({ "id": id, "label": label, "type": "note", "note_type": note_type, "created_at": created_at }));
        }

        // Entities (most-mentioned first)
        let entity_limit = (max_nodes / 4).max(20);
        let entity_rows = neo4j
            .execute(
                neo4rs::query(
                    "MATCH (e:Entity)<-[r:MENTIONS]-() \
                 RETURN e.id AS id, e.name AS name, e.entity_type AS entity_type, count(r) AS mentions, \
                        toString(e.created_at) AS created_at \
                 ORDER BY mentions DESC LIMIT $limit",
                )
                .param("limit", entity_limit),
            )
            .await
            .unwrap_or_default();
        for row in entity_rows {
            let id = row.get::<String>("id").unwrap_or_default();
            let name = row.get::<String>("name").unwrap_or_default();
            let entity_type = row
                .get::<String>("entity_type")
                .unwrap_or_else(|_| "unknown".to_string());
            let created_at = row.get::<String>("created_at").unwrap_or_default();
            nodes.push(json!({ "id": id, "label": name, "type": "entity", "entity_type": entity_type, "created_at": created_at }));
        }

        // Tasks
        let task_limit = (max_nodes / 8).max(10);
        let task_rows = neo4j
            .execute(
                neo4rs::query(
                    "MATCH (t:Task) \
                 RETURN t.id AS id, t.goal AS goal, t.status AS status, \
                        toString(t.created_at) AS created_at \
                 ORDER BY t.created_at DESC LIMIT $limit",
                )
                .param("limit", task_limit),
            )
            .await
            .unwrap_or_default();
        for row in task_rows {
            let id = row.get::<String>("id").unwrap_or_default();
            let goal = row.get::<String>("goal").unwrap_or_default();
            let status = row.get::<String>("status").unwrap_or_default();
            let created_at = row.get::<String>("created_at").unwrap_or_default();
            let label: String = goal.chars().take(60).collect();
            nodes.push(json!({ "id": id, "label": label, "type": "task", "status": status, "created_at": created_at }));
        }
    }

    let node_ids: std::collections::HashSet<String> = nodes
        .iter()
        .filter_map(|n| n["id"].as_str().map(str::to_string))
        .collect();
    let ids_vec: Vec<String> = node_ids.iter().cloned().collect();

    // Edges
    let mut edges: Vec<Value> = Vec::new();
    let edge_queries: &[(&str, &str)] = &[
        (
            "MATCH (a:Note)-[r:RELATES_TO]->(b:Note) WHERE a.id IN $ids AND b.id IN $ids \
          RETURN a.id AS src, b.id AS tgt, r.similarity AS weight",
            "relates_to",
        ),
        (
            "MATCH (a:Note)-[:MENTIONS]->(b:Entity) WHERE a.id IN $ids AND b.id IN $ids \
          RETURN a.id AS src, b.id AS tgt, 1.0 AS weight",
            "mentions",
        ),
        (
            "MATCH (a:Note)-[:SUMMARIZED_BY]->(b:Note) WHERE a.id IN $ids AND b.id IN $ids \
          RETURN a.id AS src, b.id AS tgt, 1.0 AS weight",
            "summarized_by",
        ),
        (
            "MATCH (a:Note)-[:REFLECTS_ON]->(b:Task) WHERE a.id IN $ids AND b.id IN $ids \
          RETURN a.id AS src, b.id AS tgt, 1.0 AS weight",
            "reflects_on",
        ),
        (
            "MATCH (a:Task)-[:SUBTASK_OF]->(b:Task) WHERE a.id IN $ids AND b.id IN $ids \
          RETURN a.id AS src, b.id AS tgt, 1.0 AS weight",
            "subtask_of",
        ),
        (
            "MATCH (a:Note)-[:DERIVED_FROM]->(b:Note) WHERE a.id IN $ids AND b.id IN $ids \
          RETURN a.id AS src, b.id AS tgt, 1.0 AS weight",
            "derived_from",
        ),
    ];

    for (cypher, label) in edge_queries {
        let rows = neo4j
            .execute(neo4rs::query(cypher).param("ids", ids_vec.clone()))
            .await
            .unwrap_or_default();
        for row in rows {
            let src = row.get::<String>("src").unwrap_or_default();
            let tgt = row.get::<String>("tgt").unwrap_or_default();
            let weight = row.get::<f64>("weight").unwrap_or(1.0);
            edges.push(json!({ "source": src, "target": tgt, "type": label, "weight": weight }));
        }
    }

    Json(json!({
        "node_count": nodes.len(),
        "edge_count": edges.len(),
        "seeded": !seeds.is_empty(),
        "nodes": nodes,
        "edges": edges,
    }))
    .into_response()
}
