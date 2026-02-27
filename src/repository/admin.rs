use neo4rs::query;
use tracing::info;

use crate::repository::{Neo4jClient, RepositoryError};

/// Summary of what a `delete_api` or `reset_graph` call would affect.
#[derive(Debug, serde::Serialize)]
pub struct CleanupStats {
    pub resources: u32,
    pub endpoints: u32,
    pub parameters: u32,
    pub healing_events: u32,
    pub schemas: u32,
}

impl Neo4jClient {
    /// Count nodes that would be removed by `delete_api_cascade`.
    pub async fn count_api_nodes(&self, api_name: &str) -> Result<CleanupStats, RepositoryError> {
        let q = query(
            "MATCH (r:Resource)
             WHERE toLower(r.name) = toLower($name)
             OPTIONAL MATCH (r)-[:HAS_ENDPOINT]->(e:Endpoint)
             OPTIONAL MATCH (e)-[:REQUIRES_PARAM]->(p:Parameter)
             OPTIONAL MATCH (e)-[:HAS_HISTORY]->(h:HealingEvent)
             OPTIONAL MATCH (e)-[:RETURNS_SCHEMA|ACCEPTS_SCHEMA]->(s:Schema)
             RETURN
               count(DISTINCT r) AS resources,
               count(DISTINCT e) AS endpoints,
               count(DISTINCT p) AS parameters,
               count(DISTINCT h) AS healing_events,
               count(DISTINCT s) AS schemas",
        )
        .param("name", api_name);

        let rows = self.execute(q).await?;
        if let Some(row) = rows.into_iter().next() {
            Ok(CleanupStats {
                resources:      row.get::<i64>("resources").unwrap_or(0)      as u32,
                endpoints:      row.get::<i64>("endpoints").unwrap_or(0)      as u32,
                parameters:     row.get::<i64>("parameters").unwrap_or(0)     as u32,
                healing_events: row.get::<i64>("healing_events").unwrap_or(0) as u32,
                schemas:        row.get::<i64>("schemas").unwrap_or(0)        as u32,
            })
        } else {
            Ok(CleanupStats { resources: 0, endpoints: 0, parameters: 0, healing_events: 0, schemas: 0 })
        }
    }

    /// Cascade-delete all nodes belonging to a single ingested API.
    ///
    /// Deletes: Resource → Endpoints → Parameters → HealingEvents.
    /// Also deletes Schemas that are *exclusively* linked to this API's endpoints.
    pub async fn delete_api_cascade(&self, api_name: &str) -> Result<CleanupStats, RepositoryError> {
        // First snapshot the counts so we can return them.
        let stats = self.count_api_nodes(api_name).await?;

        // Delete Parameters, HealingEvents, and exclusive Schemas first (leaves before trunk).
        self.run(
            query(
                "MATCH (r:Resource)
                 WHERE toLower(r.name) = toLower($name)
                 OPTIONAL MATCH (r)-[:HAS_ENDPOINT]->(e:Endpoint)
                 OPTIONAL MATCH (e)-[:REQUIRES_PARAM]->(p:Parameter)
                 OPTIONAL MATCH (e)-[:HAS_HISTORY]->(h:HealingEvent)
                 DETACH DELETE p, h",
            )
            .param("name", api_name),
        )
        .await?;

        // Delete Schemas only referenced by this API's endpoints.
        self.run(
            query(
                "MATCH (r:Resource)
                 WHERE toLower(r.name) = toLower($name)
                 MATCH (r)-[:HAS_ENDPOINT]->(e:Endpoint)
                 MATCH (e)-[:RETURNS_SCHEMA|ACCEPTS_SCHEMA]->(s:Schema)
                 WHERE NOT EXISTS {
                     MATCH (other:Endpoint)-[:RETURNS_SCHEMA|ACCEPTS_SCHEMA]->(s)
                     WHERE NOT (r)-[:HAS_ENDPOINT]->(other)
                 }
                 DETACH DELETE s",
            )
            .param("name", api_name),
        )
        .await?;

        // Delete Endpoints, then the Resource itself.
        self.run(
            query(
                "MATCH (r:Resource)
                 WHERE toLower(r.name) = toLower($name)
                 OPTIONAL MATCH (r)-[:HAS_ENDPOINT]->(e:Endpoint)
                 DETACH DELETE e, r",
            )
            .param("name", api_name),
        )
        .await?;

        info!(api = %api_name, ?stats, "Deleted API cascade");
        Ok(stats)
    }

    /// Find groups of duplicate endpoints (same Resource + path + method).
    /// Returns (resource_name, path, method, duplicate_count) for each group.
    pub async fn find_duplicate_endpoints(
        &self,
    ) -> Result<Vec<(String, String, String, u32)>, RepositoryError> {
        let q = query(
            "MATCH (r:Resource)-[:HAS_ENDPOINT]->(e:Endpoint)
             WITH r.name AS resource, e.path AS path, e.method AS method, count(e) AS cnt
             WHERE cnt > 1
             RETURN resource, path, method, cnt
             ORDER BY resource, path, method",
        );

        let rows = self.execute(q).await?;
        Ok(rows
            .into_iter()
            .map(|row| {
                let resource: String = row.get("resource").unwrap_or_default();
                let path: String    = row.get("path").unwrap_or_default();
                let method: String  = row.get("method").unwrap_or_default();
                let cnt: i64        = row.get("cnt").unwrap_or(0);
                (resource, path, method, cnt as u32)
            })
            .collect())
    }

    /// Delete duplicate endpoints, keeping the oldest (lowest id lexicographically).
    /// Returns the number of duplicate nodes removed.
    pub async fn purge_duplicate_endpoints(&self) -> Result<u32, RepositoryError> {
        let q = query(
            "MATCH (r:Resource)-[:HAS_ENDPOINT]->(e:Endpoint)
             WITH r, e.path AS path, e.method AS method, collect(e) AS dupes
             WHERE size(dupes) > 1
             UNWIND tail(dupes) AS dup
             DETACH DELETE dup
             RETURN count(dup) AS deleted",
        );

        let rows = self.execute(q).await?;
        let deleted = rows
            .into_iter()
            .next()
            .and_then(|row| row.get::<i64>("deleted").ok())
            .unwrap_or(0) as u32;

        info!(deleted, "Purged duplicate endpoints");
        Ok(deleted)
    }

    /// Count Schema nodes with no Endpoint relationships.
    pub async fn count_orphaned_schemas(&self) -> Result<u32, RepositoryError> {
        let q = query(
            "MATCH (s:Schema)
             WHERE NOT EXISTS { MATCH ()-[:RETURNS_SCHEMA|ACCEPTS_SCHEMA|LINKS_TO]->(s) }
               AND NOT EXISTS { MATCH (s)-[:LINKS_TO]->() }
             RETURN count(s) AS cnt",
        );

        let rows = self.execute(q).await?;
        Ok(rows
            .into_iter()
            .next()
            .and_then(|row| row.get::<i64>("cnt").ok())
            .unwrap_or(0) as u32)
    }

    /// Delete Schema nodes with no Endpoint or Schema relationships.
    /// Returns the number of nodes removed.
    pub async fn purge_orphaned_schemas(&self) -> Result<u32, RepositoryError> {
        let q = query(
            "MATCH (s:Schema)
             WHERE NOT EXISTS { MATCH ()-[:RETURNS_SCHEMA|ACCEPTS_SCHEMA|LINKS_TO]->(s) }
               AND NOT EXISTS { MATCH (s)-[:LINKS_TO]->() }
             DETACH DELETE s
             RETURN count(s) AS deleted",
        );

        let rows = self.execute(q).await?;
        let deleted = rows
            .into_iter()
            .next()
            .and_then(|row| row.get::<i64>("deleted").ok())
            .unwrap_or(0) as u32;

        info!(deleted, "Purged orphaned schemas");
        Ok(deleted)
    }

    /// Count all API-data nodes (Resource/Endpoint/Schema/Parameter/HealingEvent).
    pub async fn count_api_graph(&self) -> Result<CleanupStats, RepositoryError> {
        let q = query(
            "MATCH (r:Resource) WITH count(r) AS resources
             MATCH (e:Endpoint) WITH resources, count(e) AS endpoints
             MATCH (p:Parameter) WITH resources, endpoints, count(p) AS parameters
             MATCH (h:HealingEvent) WITH resources, endpoints, parameters, count(h) AS healing_events
             MATCH (s:Schema) WITH resources, endpoints, parameters, healing_events, count(s) AS schemas
             RETURN resources, endpoints, parameters, healing_events, schemas",
        );

        let rows = self.execute(q).await?;
        if let Some(row) = rows.into_iter().next() {
            Ok(CleanupStats {
                resources:      row.get::<i64>("resources").unwrap_or(0)      as u32,
                endpoints:      row.get::<i64>("endpoints").unwrap_or(0)      as u32,
                parameters:     row.get::<i64>("parameters").unwrap_or(0)     as u32,
                healing_events: row.get::<i64>("healing_events").unwrap_or(0) as u32,
                schemas:        row.get::<i64>("schemas").unwrap_or(0)        as u32,
            })
        } else {
            Ok(CleanupStats { resources: 0, endpoints: 0, parameters: 0, healing_events: 0, schemas: 0 })
        }
    }

    /// Wipe ALL API data (Resource, Endpoint, Schema, Parameter, HealingEvent).
    /// Knowledge graph data (Notes, Tasks, Procedures, WorkingMemory, etc.) is preserved.
    pub async fn reset_api_graph(&self) -> Result<CleanupStats, RepositoryError> {
        let stats = self.count_api_graph().await?;

        self.run(query(
            "MATCH (n)
             WHERE n:Resource OR n:Endpoint OR n:Schema OR n:Parameter OR n:HealingEvent
             DETACH DELETE n",
        ))
        .await?;

        info!(?stats, "Reset API graph");
        Ok(stats)
    }
}
