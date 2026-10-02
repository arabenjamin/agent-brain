//! Graph export for the frontend visualization.

use super::*;

impl KnowledgeService {
    /// Export the full knowledge graph as a node/edge structure for visualisation.
    ///
    /// Returns `(nodes, edges)` where each node is
    /// `{"id","label","type","note_type?}` and each edge is
    /// `{"source","target","type","weight"}`.
    ///
    /// Node types: `note`, `entity`, `task`
    /// Edge types: `relates_to`, `mentions`, `part_of`, `summarized_by`,
    ///             `reflects_on`, `subtask_of`, `depends_on`, `derived_from`
    pub async fn export_graph_visualization(
        &self,
        max_nodes: usize,
    ) -> Result<(Vec<Value>, Vec<Value>)> {
        let limit = max_nodes as i64;

        // --- Notes ---
        let note_q = neo4rs::query(
            "MATCH (n:Note) \
             RETURN n.id AS id, n.content AS content, n.note_type AS note_type \
             ORDER BY n.last_accessed_at DESC LIMIT $limit",
        )
        .param("limit", limit);

        let mut nodes: Vec<Value> = Vec::new();
        let note_rows = self.neo4j.execute(note_q).await.unwrap_or_default();
        for row in note_rows {
            let id = row.get::<String>("id").unwrap_or_default();
            let content = row.get::<String>("content").unwrap_or_default();
            let note_type = row
                .get::<String>("note_type")
                .unwrap_or_else(|_| "semantic".to_string());
            let label: String = content.chars().take(60).collect();
            nodes.push(json!({
                "id": id,
                "label": label,
                "type": "note",
                "note_type": note_type
            }));
        }

        // --- Entities (most-mentioned first) ---
        let entity_limit = (max_nodes / 4).max(20) as i64;
        let entity_q = neo4rs::query(
            "MATCH (e:Entity)<-[r:MENTIONS]-() \
             RETURN e.id AS id, e.name AS name, e.entity_type AS entity_type, \
                    count(r) AS mentions \
             ORDER BY mentions DESC LIMIT $limit",
        )
        .param("limit", entity_limit);

        let entity_rows = self.neo4j.execute(entity_q).await.unwrap_or_default();
        for row in entity_rows {
            let id = row.get::<String>("id").unwrap_or_default();
            let name = row.get::<String>("name").unwrap_or_default();
            let entity_type = row
                .get::<String>("entity_type")
                .unwrap_or_else(|_| "unknown".to_string());
            nodes.push(json!({
                "id": id,
                "label": name,
                "type": "entity",
                "entity_type": entity_type
            }));
        }

        // --- Tasks ---
        let task_limit = (max_nodes / 8).max(10) as i64;
        let task_q = neo4rs::query(
            "MATCH (t:Task) \
             RETURN t.id AS id, t.goal AS goal, t.status AS status \
             ORDER BY t.created_at DESC LIMIT $limit",
        )
        .param("limit", task_limit);

        let task_rows = self.neo4j.execute(task_q).await.unwrap_or_default();
        for row in task_rows {
            let id = row.get::<String>("id").unwrap_or_default();
            let goal = row.get::<String>("goal").unwrap_or_default();
            let status = row.get::<String>("status").unwrap_or_default();
            let label: String = goal.chars().take(60).collect();
            nodes.push(json!({
                "id": id,
                "label": label,
                "type": "task",
                "status": status
            }));
        }

        // Build the set of node IDs so we only emit edges between included nodes
        let node_ids: std::collections::HashSet<String> = nodes
            .iter()
            .filter_map(|n| n["id"].as_str().map(str::to_string))
            .collect();

        // --- Edges ---
        let mut edges: Vec<Value> = Vec::new();

        let edge_queries: &[(&str, &str, f64)] = &[
            (
                "MATCH (a:Note)-[r:RELATES_TO]->(b:Note) \
                 WHERE a.id IN $ids AND b.id IN $ids \
                 RETURN a.id AS src, b.id AS tgt, r.similarity AS weight",
                "relates_to",
                1.0,
            ),
            (
                "MATCH (a:Note)-[:MENTIONS]->(b:Entity) \
                 WHERE a.id IN $ids AND b.id IN $ids \
                 RETURN a.id AS src, b.id AS tgt, 1.0 AS weight",
                "mentions",
                1.0,
            ),
            (
                "MATCH (a:Note)-[:PART_OF]->(b:Note) \
                 WHERE a.id IN $ids AND b.id IN $ids \
                 RETURN a.id AS src, b.id AS tgt, 1.0 AS weight",
                "part_of",
                0.5,
            ),
            (
                "MATCH (a:Note)-[:SUMMARIZED_BY]->(b:Note) \
                 WHERE a.id IN $ids AND b.id IN $ids \
                 RETURN a.id AS src, b.id AS tgt, 1.0 AS weight",
                "summarized_by",
                0.5,
            ),
            (
                "MATCH (a:Note)-[:REFLECTS_ON]->(b:Task) \
                 WHERE a.id IN $ids AND b.id IN $ids \
                 RETURN a.id AS src, b.id AS tgt, 1.0 AS weight",
                "reflects_on",
                0.8,
            ),
            (
                "MATCH (a:Task)-[:SUBTASK_OF]->(b:Task) \
                 WHERE a.id IN $ids AND b.id IN $ids \
                 RETURN a.id AS src, b.id AS tgt, 1.0 AS weight",
                "subtask_of",
                0.7,
            ),
            (
                "MATCH (a:Note)-[:DERIVED_FROM]->(b:Note) \
                 WHERE a.id IN $ids AND b.id IN $ids \
                 RETURN a.id AS src, b.id AS tgt, 1.0 AS weight",
                "derived_from",
                0.6,
            ),
        ];

        let ids_vec: Vec<String> = node_ids.iter().cloned().collect();
        for (cypher, edge_type, default_weight) in edge_queries {
            if let Ok(rows) = self
                .neo4j
                .execute(neo4rs::query(cypher).param("ids", ids_vec.clone()))
                .await
            {
                for row in rows {
                    let src = row.get::<String>("src").unwrap_or_default();
                    let tgt = row.get::<String>("tgt").unwrap_or_default();
                    let weight = row.get::<f64>("weight").unwrap_or(*default_weight);
                    if !src.is_empty() && !tgt.is_empty() {
                        edges.push(json!({
                            "source": src,
                            "target": tgt,
                            "type": edge_type,
                            "weight": weight
                        }));
                    }
                }
            }
        }

        Ok((nodes, edges))
    }
}
