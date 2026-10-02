//! Spaced-repetition review and pruning.

use super::*;

impl KnowledgeService {
    pub async fn review_due_notes(&self, limit: usize) -> Result<Vec<Value>> {
        let cypher = r#"
        MATCH (n:Note)
        WHERE n.next_review_at <= datetime()
          AND NOT COALESCE(n.note_type, 'semantic') IN ['consolidated', 'outcome', 'news', 'news_raw']
        RETURN n.id AS id, n.content AS content, n.note_type AS note_type,
               toString(n.next_review_at) AS next_review_at, n.access_count AS access_count
        ORDER BY n.next_review_at ASC LIMIT $limit
        "#;

        let query = neo4rs::query(cypher).param("limit", limit as i64);
        let rows = self.neo4j.execute(query).await?;

        let mut results = Vec::new();
        for row in rows {
            let id = row.get::<String>("id").unwrap_or_default();
            let content = row.get::<String>("content").unwrap_or_default();
            let note_type = row
                .get::<String>("note_type")
                .unwrap_or_else(|_| "semantic".to_string());
            let next_review = row.get::<String>("next_review_at").unwrap_or_default();
            let access_count = row.get::<i64>("access_count").unwrap_or(0);
            results.push(serde_json::json!({
                "id": id,
                "content": content,
                "note_type": note_type,
                "next_review_at": next_review,
                "access_count": access_count
            }));
        }

        Ok(results)
    }

    /// Delete stale notes. When score_threshold/lambda provided, uses adaptive decay scoring.
    /// When dry_run=true, returns candidate count without deleting.
    /// `min_retain` and `max_pct` guard against over-pruning: at most `max_pct` of total notes
    /// are removed per call, and the graph never drops below `min_retain` notes.
    #[allow(clippy::too_many_arguments)]
    pub async fn prune_old_notes(
        &self,
        days_stale: i64,
        min_accesses: i64,
        score_threshold: Option<f64>,
        lambda: Option<f64>,
        dry_run: bool,
        min_retain: i64,
        max_pct: f64,
    ) -> Result<usize> {
        if !dry_run
            && self.auto_snapshot_prune
            && let Some(svc) = &self.snapshot_svc
        {
            match svc.take_snapshot(Some("pre_prune")).await {
                Ok((path, _)) => info!(path = %path.display(), "Pre-prune snapshot taken"),
                Err(e) => warn!("Pre-prune snapshot failed (continuing): {}", e),
            }
        }

        let use_adaptive = score_threshold.is_some() || lambda.is_some();

        // Count candidates using the appropriate filter (same WHERE as the real delete).
        let candidates: usize = if use_adaptive {
            let threshold = score_threshold.unwrap_or(0.1);
            let lam = lambda.unwrap_or(0.1);
            let cypher = r#"
            MATCH (n:Note)
            WHERE NOT COALESCE(n.note_type, 'semantic') IN ['consolidated', 'reflection', 'news']
            OPTIONAL MATCH (other:Note)-[:RELATES_TO]->(n)
            WITH n, count(other) AS in_degree,
                 duration.between(n.last_accessed_at, datetime()).days AS days_idle
            WITH n,
                 toFloat(COALESCE(n.access_count, 0) + 1) / (1.0 + toFloat(in_degree))
                     * exp(-$lambda * toFloat(days_idle)) AS decay_score
            WHERE decay_score < $threshold
            RETURN count(n) AS total
            "#;
            let rows = self
                .neo4j
                .execute(
                    neo4rs::query(cypher)
                        .param("lambda", lam)
                        .param("threshold", threshold),
                )
                .await?;
            rows.first()
                .and_then(|r| r.get::<i64>("total").ok())
                .unwrap_or(0) as usize
        } else {
            let cypher = r#"
            MATCH (n:Note)
            WHERE n.last_accessed_at < datetime() - duration({days: $days})
              AND n.access_count < $min_accesses
              AND NOT COALESCE(n.note_type, 'semantic') IN ['consolidated', 'reflection', 'news']
            RETURN count(n) AS total
            "#;
            let rows = self
                .neo4j
                .execute(
                    neo4rs::query(cypher)
                        .param("days", days_stale)
                        .param("min_accesses", min_accesses),
                )
                .await?;
            rows.first()
                .and_then(|r| r.get::<i64>("total").ok())
                .unwrap_or(0) as usize
        };

        if dry_run {
            return Ok(candidates);
        }

        // Retention safeguard: cap deletion so we never go below min_retain or exceed max_pct.
        let total_rows = self
            .neo4j
            .execute(neo4rs::query("MATCH (n:Note) RETURN count(n) AS total"))
            .await?;
        let total: i64 = total_rows
            .first()
            .and_then(|r| r.get::<i64>("total").ok())
            .unwrap_or(0);

        let headroom = (total - min_retain).max(0);
        let pct_cap = (total as f64 * max_pct).floor() as i64;
        let safe_limit = (candidates as i64).min(headroom).min(pct_cap).max(0);

        if safe_limit <= 0 {
            warn!(
                total,
                min_retain, candidates, "Pruning skipped — retention safeguard would be breached"
            );
            return Ok(0);
        }
        if safe_limit < candidates as i64 {
            warn!(
                safe_limit,
                candidates, total, max_pct, min_retain, "Pruning capped by retention safeguard"
            );
        }

        if use_adaptive {
            let threshold = score_threshold.unwrap_or(0.1);
            let lam = lambda.unwrap_or(0.1);
            let cypher = r#"
            MATCH (n:Note)
            WHERE NOT COALESCE(n.note_type, 'semantic') IN ['consolidated', 'reflection']
            OPTIONAL MATCH (other:Note)-[:RELATES_TO]->(n)
            WITH n, count(other) AS in_degree,
                 duration.between(n.last_accessed_at, datetime()).days AS days_idle
            WITH n,
                 toFloat(COALESCE(n.access_count, 0) + 1) / (1.0 + toFloat(in_degree))
                     * exp(-$lambda * toFloat(days_idle)) AS decay_score
            WHERE decay_score < $threshold
            WITH n ORDER BY decay_score ASC
            WITH collect(n)[0..$safe_limit] AS stale
            FOREACH (n IN stale | DETACH DELETE n)
            RETURN size(stale) AS total
            "#;
            let rows = self
                .neo4j
                .execute(
                    neo4rs::query(cypher)
                        .param("lambda", lam)
                        .param("threshold", threshold)
                        .param("safe_limit", safe_limit),
                )
                .await?;
            let deleted = rows
                .first()
                .and_then(|r| r.get::<i64>("total").ok())
                .unwrap_or(0) as usize;
            self.prune_stale_entities().await;
            return Ok(deleted);
        }

        // Legacy path: days_stale / min_accesses
        let cypher = r#"
        MATCH (n:Note)
        WHERE n.last_accessed_at < datetime() - duration({days: $days})
          AND n.access_count < $min_accesses
          AND NOT COALESCE(n.note_type, 'semantic') IN ['consolidated', 'reflection', 'news']
        WITH n ORDER BY n.access_count ASC, n.last_accessed_at ASC
        WITH collect(n)[0..$safe_limit] AS stale
        FOREACH (n IN stale | DETACH DELETE n)
        RETURN size(stale) AS total
        "#;
        let rows = self
            .neo4j
            .execute(
                neo4rs::query(cypher)
                    .param("days", days_stale)
                    .param("min_accesses", min_accesses)
                    .param("safe_limit", safe_limit),
            )
            .await?;
        let deleted = rows
            .first()
            .and_then(|r| r.get::<i64>("total").ok())
            .unwrap_or(0) as usize;
        self.prune_stale_entities().await;
        Ok(deleted)
    }

    /// Delete stale entities after a note prune: any entity with no remaining
    /// MENTIONS edges, plus single-mention entities older than 30 days. Entity
    /// extraction over-generates (most entities are mentioned exactly once);
    /// a single-mention entity can never act as a MENTIONS→Entity←MENTIONS
    /// bridge in graph-expanded search, so past the 30-day grace window it is
    /// pure noise. Best-effort: failures are logged, never propagated.
    pub(super) async fn prune_stale_entities(&self) {
        let cypher = r#"
        MATCH (e:Entity)
        OPTIONAL MATCH (n:Note)-[:MENTIONS]->(e)
        WITH e, count(n) AS mentions
        WHERE mentions = 0
           OR (mentions = 1
               AND (e.created_at IS NULL
                    OR e.created_at < datetime() - duration({days: 30})))
        DETACH DELETE e
        RETURN count(e) AS n
        "#;
        match self.neo4j.execute(neo4rs::query(cypher)).await {
            Ok(rows) => {
                let n = rows
                    .first()
                    .and_then(|r| r.get::<i64>("n").ok())
                    .unwrap_or(0);
                if n > 0 {
                    info!(deleted = n, "Pruned stale entities");
                }
            }
            Err(e) => warn!("Stale entity pruning failed (non-fatal): {}", e),
        }
    }
}
