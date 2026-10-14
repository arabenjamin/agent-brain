//! Checking that graph ids cited in a reply exist.

use super::*;

/// Most cited ids checked per reply. A reply citing more than this is checked
/// on its first N; the check is a guard, not an audit.
pub(super) const MAX_CITED_IDS: usize = 20;

/// Every `id:<uuid>` in a reply — the form the retrieval labels use
/// (`[[SOURCE RECORD · id:…]]`), and therefore the form a model copies.
pub(super) fn cited_ids(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (i, _) in text.match_indices("id:") {
        let rest = text[i + 3..].trim_start();
        let Some(candidate) = rest.get(..36) else {
            continue;
        };
        let is_uuid = candidate.char_indices().all(|(j, c)| match j {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        });
        let id = candidate.to_ascii_lowercase();
        if is_uuid && !out.contains(&id) {
            out.push(id);
            if out.len() == MAX_CITED_IDS {
                break;
            }
        }
    }
    out
}

/// Answers which of a reply's cited ids do not exist. `None` = could not
/// check; the reply then goes out unchanged (fail open).
#[async_trait::async_trait]
pub trait CitationChecker: Send + Sync {
    async fn missing(&self, ids: &[String]) -> Option<Vec<String>>;
}

/// Checks cited ids against `:Note` and `:Task` through `neo4j_query`.
pub(super) struct GraphCitationChecker(pub(super) ToolHandler);

#[async_trait::async_trait]
impl CitationChecker for GraphCitationChecker {
    async fn missing(&self, ids: &[String]) -> Option<Vec<String>> {
        // `neo4j_query` params are strings only, so the list is inlined. Safe:
        // `cited_ids` admits nothing but hex and dashes in UUID shape.
        let list = ids
            .iter()
            .map(|id| format!("'{id}'"))
            .collect::<Vec<_>>()
            .join(",");
        let cypher = format!(
            "UNWIND [{list}] AS id \
             OPTIONAL MATCH (n:Note {{id: id}}) \
             OPTIONAL MATCH (t:Task {{id: id}}) \
             RETURN id, (n IS NOT NULL OR t IS NOT NULL) AS found"
        );
        let result = self
            .0
            .execute(
                "neo4j_query",
                Some(json!({ "cypher": cypher, "readonly": true })),
            )
            .await;
        if result.is_error == Some(true) {
            debug!("citation check query failed — reply left unchanged");
            return None;
        }
        let text = result.content.first().and_then(|c| match c {
            agent_brain_protocol::Content::Text { text } => Some(text.as_str()),
            _ => None,
        })?;
        let v: Value = serde_json::from_str(text).ok()?;
        let rows = v.get("rows")?.as_array()?;
        Some(
            rows.iter()
                .filter(|r| r.get("found").and_then(|f| f.as_bool()) == Some(false))
                .filter_map(|r| r.get("id").and_then(|i| i.as_str()).map(String::from))
                .collect(),
        )
    }
}

/// The line appended to a reply that cites ids the graph does not hold.
pub(super) fn citation_notice(missing: &[String]) -> String {
    let ids = missing
        .iter()
        .map(|id| format!("`{id}`"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "⚠ Citation check: {} cited id(s) do not exist in the knowledge graph — {ids}. \
         The statements attached to them are unsupported; treat them as unverified.",
        missing.len()
    )
}

/// Append a notice when a reply cites ids that do not exist.
///
/// On 2026-09-24 a front-door turn on gemini-2.5-flash answered from memory,
/// ran no tools, and cited `[[SOURCE RECORD · id:75368a57-…]]` and
/// `id:0020786f-…` — the retrieval-label format, with ids that exist nowhere.
/// A prompt rule cannot stop a model inventing an id; checking the ids can,
/// whichever model is in use.
pub(super) async fn check_citations(
    content: String,
    checker: Option<&dyn CitationChecker>,
) -> String {
    let Some(checker) = checker else {
        return content;
    };
    let ids = cited_ids(&content);
    if ids.is_empty() {
        return content;
    }
    match checker.missing(&ids).await {
        Some(missing) if !missing.is_empty() => {
            warn!(missing = ?missing, "Reply cites graph ids that do not exist");
            format!("{content}\n\n{}", citation_notice(&missing))
        }
        _ => content,
    }
}

#[cfg(test)]
mod citation_check_tests {
    use super::*;

    const REAL: &str = "11111111-2222-4333-8444-555555555555";
    const FAKE: &str = "75368a57-810a-4a6c-947b-a279c2999086";

    /// Stands in for the graph: only `REAL` exists.
    struct OnlyReal;
    #[async_trait::async_trait]
    impl CitationChecker for OnlyReal {
        async fn missing(&self, ids: &[String]) -> Option<Vec<String>> {
            Some(ids.iter().filter(|i| i.as_str() != REAL).cloned().collect())
        }
    }

    struct Unreachable;
    #[async_trait::async_trait]
    impl CitationChecker for Unreachable {
        async fn missing(&self, _: &[String]) -> Option<Vec<String>> {
            None
        }
    }

    #[test]
    fn cited_ids_reads_the_label_form_and_ignores_non_uuids() {
        let t = format!("a [[SOURCE RECORD · id:{FAKE}]] b id: {REAL} c id:not-a-uuid d id:{FAKE}");
        assert_eq!(cited_ids(&t), vec![FAKE.to_string(), REAL.to_string()]);
    }

    /// The 2026-09-24 reply: an invented id gets a visible notice.
    #[tokio::test]
    async fn an_invented_id_is_flagged_and_a_real_one_is_not() {
        let reply = format!("Claim one [[SOURCE RECORD · id:{FAKE}]]. Claim two [[id:{REAL}]].");
        let out = check_citations(reply.clone(), Some(&OnlyReal)).await;
        assert!(out.starts_with(&reply));
        assert!(
            out.contains("Citation check") && out.contains(FAKE),
            "{out}"
        );
        assert!(!out.contains(&format!("`{REAL}`")), "{out}");

        let clean = format!("Only real [[id:{REAL}]].");
        assert_eq!(check_citations(clean.clone(), Some(&OnlyReal)).await, clean);
    }

    #[tokio::test]
    async fn an_unavailable_check_leaves_the_reply_alone() {
        let reply = format!("x [[id:{FAKE}]]");
        assert_eq!(
            check_citations(reply.clone(), Some(&Unreachable)).await,
            reply
        );
        assert_eq!(check_citations(reply.clone(), None).await, reply);
    }

    /// End to end through the forwarder: the client and working memory both
    /// get the flagged text.
    #[tokio::test]
    async fn the_forwarder_flags_the_final_message() {
        let (inner_tx, inner_rx) = mpsc::channel::<ChatEvent>(8);
        let (tx, mut rx) = mpsc::channel::<ChatEvent>(8);
        let (result_tx, mut result_rx) = mpsc::channel::<String>(1);
        inner_tx
            .send(ChatEvent::Message {
                content: format!("see [[id:{FAKE}]]"),
            })
            .await
            .unwrap();
        drop(inner_tx);
        forward_chat_events(
            inner_rx,
            tx,
            result_tx,
            None,
            "q".into(),
            Some(Arc::new(OnlyReal)),
            None,
        )
        .await;
        let mut shown = String::new();
        while let Ok(e) = rx.try_recv() {
            if let ChatEvent::Message { content } = e {
                shown = content;
            }
        }
        assert!(shown.contains("Citation check"), "{shown}");
        assert_eq!(result_rx.recv().await.unwrap(), shown);
    }
}
