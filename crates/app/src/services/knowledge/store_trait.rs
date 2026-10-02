//! `KnowledgeStore` impl — the trait object `KnowledgeSkill` holds; pure forwarding.

use super::*;

#[async_trait::async_trait]
impl crate::services::traits::KnowledgeStore for KnowledgeService {
    async fn store_note(
        &self,
        content: &str,
        note_type: Option<&str>,
        source_context: Option<&str>,
        event_at: Option<&str>,
        provenance: Option<ProvenanceFlag>,
    ) -> anyhow::Result<(String, usize)> {
        KnowledgeService::store_note(
            self,
            content,
            note_type,
            source_context,
            event_at,
            provenance,
        )
        .await
    }

    async fn store_context_inference(
        &self,
        question: &str,
        answer: &str,
        inferences: &[String],
    ) -> Option<String> {
        KnowledgeService::store_context_inference(self, question, answer, inferences).await
    }

    async fn link_note_to_task(&self, note_id: &str, task_id: &str) -> anyhow::Result<bool> {
        KnowledgeService::link_note_to_task(self, note_id, task_id).await
    }

    async fn search_notes(
        &self,
        query: &str,
        limit: usize,
        graph_hops: usize,
        note_type: Option<&str>,
        source_context: Option<&str>,
    ) -> anyhow::Result<Vec<serde_json::Value>> {
        KnowledgeService::search_notes(self, query, limit, graph_hops, note_type, source_context)
            .await
    }

    async fn search_notes_readonly(
        &self,
        query: &str,
        limit: usize,
        graph_hops: usize,
        note_type: Option<&str>,
        source_context: Option<&str>,
    ) -> anyhow::Result<Vec<serde_json::Value>> {
        KnowledgeService::search_notes_readonly(
            self,
            query,
            limit,
            graph_hops,
            note_type,
            source_context,
        )
        .await
    }

    async fn search_notes_with_entity_expansion(
        &self,
        query: &str,
        limit: usize,
        graph_hops: usize,
        note_type: Option<&str>,
        source_context: Option<&str>,
    ) -> anyhow::Result<Vec<serde_json::Value>> {
        KnowledgeService::search_notes_with_entity_expansion(
            self,
            query,
            limit,
            graph_hops,
            note_type,
            source_context,
        )
        .await
    }

    async fn find_related_notes(&self, note_id: &str) -> anyhow::Result<Vec<(String, f64)>> {
        KnowledgeService::find_related_notes(self, note_id).await
    }

    async fn prune_old_notes(
        &self,
        days_stale: i64,
        min_accesses: i64,
        score_threshold: Option<f64>,
        lambda: Option<f64>,
        dry_run: bool,
        min_retain: i64,
        max_pct: f64,
    ) -> anyhow::Result<usize> {
        KnowledgeService::prune_old_notes(
            self,
            days_stale,
            min_accesses,
            score_threshold,
            lambda,
            dry_run,
            min_retain,
            max_pct,
        )
        .await
    }

    async fn consolidate_memories(
        &self,
        topic: &str,
        limit: usize,
    ) -> anyhow::Result<(String, usize, String)> {
        KnowledgeService::consolidate_memories(self, topic, limit).await
    }

    async fn synthesize_knowledge(
        &self,
        topic: &str,
        limit: usize,
    ) -> anyhow::Result<(String, String)> {
        KnowledgeService::synthesize_knowledge(self, topic, limit).await
    }

    async fn review_due_notes(&self, limit: usize) -> anyhow::Result<Vec<serde_json::Value>> {
        KnowledgeService::review_due_notes(self, limit).await
    }

    async fn reason(
        &self,
        question: &str,
        limit: usize,
        store_inference: bool,
    ) -> anyhow::Result<(String, Vec<String>, f64, Vec<String>, Option<String>)> {
        KnowledgeService::reason(self, question, limit, store_inference).await
    }

    async fn audit_action(
        &self,
        action: &str,
        context: Option<&str>,
    ) -> anyhow::Result<(bool, f64, Vec<String>, Vec<String>, String)> {
        KnowledgeService::audit_action(self, action, context).await
    }

    async fn explain_reasoning(
        &self,
        decision: &str,
        task_id: Option<&str>,
        limit: usize,
    ) -> anyhow::Result<(String, Vec<serde_json::Value>)> {
        KnowledgeService::explain_reasoning(self, decision, task_id, limit).await
    }

    async fn export_graph_visualization(
        &self,
        max_nodes: usize,
    ) -> anyhow::Result<(Vec<serde_json::Value>, Vec<serde_json::Value>)> {
        KnowledgeService::export_graph_visualization(self, max_nodes).await
    }

    async fn get_note(&self, id: &str) -> anyhow::Result<Option<serde_json::Value>> {
        KnowledgeService::get_note(self, id).await
    }

    async fn search_by_entity(
        &self,
        entity_name: &str,
        entity_type: Option<&str>,
        limit: usize,
    ) -> anyhow::Result<Vec<serde_json::Value>> {
        KnowledgeService::search_by_entity(self, entity_name, entity_type, limit).await
    }

    async fn list_notes_filtered(
        &self,
        limit: usize,
        note_type: Option<&str>,
        source_context: Option<&str>,
    ) -> anyhow::Result<Vec<serde_json::Value>> {
        KnowledgeService::list_notes_filtered(self, limit, note_type, source_context).await
    }

    async fn label_buckets(
        &self,
    ) -> anyhow::Result<(Vec<serde_json::Value>, Vec<serde_json::Value>, i64)> {
        KnowledgeService::label_buckets(self).await
    }

    async fn list_notes(
        &self,
        limit: usize,
        note_type: Option<&str>,
    ) -> anyhow::Result<Vec<serde_json::Value>> {
        KnowledgeService::list_notes(self, limit, note_type).await
    }

    async fn recall_window(
        &self,
        spec: crate::services::traits::RecallSpec<'_>,
    ) -> anyhow::Result<serde_json::Value> {
        KnowledgeService::recall_window(self, spec).await
    }

    async fn delete_note(&self, id: &str) -> anyhow::Result<bool> {
        KnowledgeService::delete_note(self, id).await
    }

    async fn update_note(&self, id: &str, content: &str) -> anyhow::Result<bool> {
        KnowledgeService::update_note(self, id, content).await
    }

    async fn reason_structured(
        &self,
        question: &str,
        limit: usize,
        store_inference: bool,
        run_critic: bool,
    ) -> anyhow::Result<ReasonOutput> {
        KnowledgeService::reason_structured(self, question, limit, store_inference, run_critic)
            .await
    }

    async fn create_gap_tasks(
        &self,
        gaps: &[String],
        triggering_note_id: &str,
    ) -> anyhow::Result<Vec<String>> {
        KnowledgeService::create_gap_tasks(self, gaps, triggering_note_id).await
    }
}
