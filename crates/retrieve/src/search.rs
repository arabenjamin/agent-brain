//! Search Skill — web search with credentials loaded from ApiContext nodes.
//!
//! API keys are no longer stored as skill fields. Instead each engine's
//! `ApiContext` node in Neo4j holds the env var name; credentials are
//! resolved at call time via `std::env::var`.  Falls back to the legacy
//! env vars (`SERPAPI_KEY`, `BRAVE_API_KEY`, etc.) when no context is found
//! so existing deployments keep working without a schema migration.

use std::time::Instant;

use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::search_engines;
use crate::search_quality;
use crate::search_query::{QuerySpec, Recency};
use crate::search_rotation::{self, Rung, Standing};
use agent_brain_knowledge::web_result::is_search_page;
use agent_brain_protocol::Skill;
use agent_brain_protocol::{ToolCallResult, ToolDefinition, parse_args};
use agent_brain_repository::{Neo4jClient, SearchAttempt, TelemetryClient};

/// How long a quota-exhausted engine stays demoted to the back of the ladder.
/// Sized for daily-reset quotas (Google CSE); a monthly cap simply stays demoted
/// because it keeps re-recording the error on each retry.
const QUOTA_COOLDOWN_HOURS: i64 = 6;

/// How long the union of the source lists' domains is cached for the
/// ledger's known-source count.
const KNOWN_DOMAINS_TTL_SECS: u64 = 600;

/// Whether `search_web` rotates its first pick across budgeted engines
/// (`SEARCH_ROTATION=off` restores the plain ladder).
fn rotation_enabled() -> bool {
    !matches!(
        std::env::var("SEARCH_ROTATION")
            .unwrap_or_default()
            .trim()
            .to_lowercase()
            .as_str(),
        "off" | "false" | "0" | "no"
    )
}

/// How the head of a call's ladder was chosen: the ledger's `picked` for
/// the first attempt (a named engine is `pinned`, a later rung `fallback`).
#[derive(Debug, Clone, Copy, PartialEq)]
enum Head {
    /// The engine that leads for the caller's tool (`lead_for`), on pace.
    Lead,
    /// The rotation's pick among on-pace budgeted engines.
    Rotation,
    /// Plain ladder order (rotation off, or nothing on pace).
    Ladder,
}

impl Head {
    fn as_str(self) -> &'static str {
        match self {
            Head::Lead => "lead",
            Head::Rotation => "rotation",
            Head::Ladder => "ladder",
        }
    }
}

/// Who is searching: the routing purpose (`direct` when nothing scoped
/// one: an MCP tool call), the queue job's tool, and the workflow run's Task.
fn caller() -> (String, Option<String>, Option<String>) {
    let purpose = agent_brain_llm::routing::ROUTE_CONTEXT
        .try_with(|c| c.purpose.as_str().to_string())
        .unwrap_or_else(|_| "direct".to_string());
    let tool = agent_brain_llm::ctx::CURRENT_TOOL
        .try_with(|t| t.clone())
        .ok()
        .flatten();
    (purpose, tool, agent_brain_llm::ctx::current_owner_task())
}

/// Classify an engine failure for the usage ledger.
///
/// `quota_exhausted` is the load-bearing case — it is what turns "the brain
/// silently stopped learning" into a queryable, alertable event.
fn classify_search_error(error_text: &str) -> &'static str {
    let lower = error_text.to_lowercase();
    if lower.contains("run out of searches")
        || lower.contains("not enough credits")
        || lower.contains("out of credits")
        || lower.contains("quota")
        || lower.contains("insufficient credits")
        || lower.contains("insufficient_credits")
    {
        "quota_exhausted"
    } else if lower.contains("rate limit")
        || lower.contains("429")
        || lower.contains("too many requests")
    {
        // A per-minute limit (TinyFish: 30/min) clears in seconds; counting
        // it as a spent quota would bench the engine for six hours. The
        // rotation's own minute/hour counters keep it from recurring.
        "rate_limited"
    } else if lower.contains("not configured") {
        "not_configured"
    } else if lower.contains("has not been used in project") || lower.contains("is disabled") {
        "api_disabled"
    } else if lower.contains("401") || lower.contains("403") || lower.contains("unauthorized") {
        "auth_error"
    } else if lower.contains("request failed") || lower.contains("timed out") {
        "network_error"
    } else {
        "api_error"
    }
}

/// `search_web`'s `engine` property, from the loaded engine files: their
/// names as the `enum`, and the ladder the call will actually walk
/// (`SEARCH_ENGINE_ORDER`, else the files' `rank` order). Hand-written, the
/// list and the order went stale whenever an engine or the order changed.
/// No files loaded (a test outside the repo root): no `enum`, since an empty
/// one would reject every value.
fn engine_property() -> Value {
    let engines = search_engines::engines();
    let configured: Vec<String> = std::env::var("SEARCH_ENGINE_ORDER")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    let ladder = if configured.is_empty() {
        search_engines::default_order(engines)
    } else {
        configured
    };
    let named_only: Vec<&str> = engines
        .iter()
        .map(|e| e.name.as_str())
        .filter(|n| !ladder.iter().any(|l| l == n))
        .collect();
    let mut description = format!(
        "Preferred engine, tried first. Omit to use the ladder ({}); engines without a key \
         are skipped, and the rest are still used as fallbacks.",
        ladder.join(" → ")
    );
    if !named_only.is_empty() {
        description.push_str(&format!(
            " Only when named here: {}.",
            named_only.join(", ")
        ));
    }
    if engines.iter().any(|e| e.name == "google_news") {
        description.push_str(" 'google_news' is SerpApi's Google News engine (news only).");
    }
    let mut prop = json!({ "type": "string", "description": description });
    if !engines.is_empty() {
        prop["enum"] = json!(engines.iter().map(|e| e.name.as_str()).collect::<Vec<_>>());
    }
    prop
}

/// Build the ordered engine ladder. Pure so the ordering rules are testable
/// without a telemetry database or a live environment.
///
/// `configured` is the raw `SEARCH_ENGINE_ORDER` value (empty = `default`, the
/// engines' declared `rank` order), `requested` is the caller's preferred
/// engine, and `exhausted` lists engines that recently reported a spent quota.
fn order_engines(
    configured: &str,
    default: &[String],
    requested: Option<&str>,
    exhausted: &[String],
) -> Vec<String> {
    let mut ladder: Vec<String> = configured
        .split(',')
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    if ladder.is_empty() {
        ladder = default.to_vec();
    }

    // A requested engine goes to the head but never truncates the ladder:
    // the caller wants an answer more than it wants that specific engine.
    if let Some(req) = requested.map(|r| r.trim().to_lowercase())
        && !req.is_empty()
    {
        ladder.retain(|e| *e != req);
        ladder.insert(0, req);
    }

    // Demote rather than drop: a daily quota that reset overnight recovers by
    // itself, but must not cost a wasted round-trip on every search until then.
    if !exhausted.is_empty() {
        let (fresh, cooling): (Vec<String>, Vec<String>) =
            ladder.into_iter().partition(|e| !exhausted.contains(e));
        ladder = fresh;
        ladder.extend(cooling);
    }

    ladder
}

/// Drop engines with no working credentials from an already-ordered ladder so
/// they don't burn a rung failing `not_configured` on every single search.
/// `keep` is the subset of engines known to be configured (a keyless engine
/// always is). Unlike quota exhaustion (demote-and-recover) a missing key
/// never fixes itself at runtime, so these are removed outright.
///
/// Never empties the ladder: if `keep` is empty (nothing configured at all) the
/// original ladder is returned so the caller still attempts a search and
/// surfaces a real per-engine error rather than silently doing nothing. Pure so
/// the rule is testable without env vars or a live graph.
fn retain_configured(ladder: Vec<String>, keep: &[String]) -> Vec<String> {
    let filtered: Vec<String> = ladder
        .iter()
        .filter(|e| keep.iter().any(|k| k == *e))
        .cloned()
        .collect();
    if filtered.is_empty() {
        ladder
    } else {
        filtered
    }
}

pub struct SearchSkill {
    client: Client,
    telemetry: Option<TelemetryClient>,
    neo4j: Option<Neo4jClient>,
    /// Used only by the "do we already know this?" probe, and only through
    /// `search_notes_readonly` so the probe never bumps a note's access count.
    /// Optional: absent it, the probe falls back to BM25 over the raw query.
    knowledge: Option<std::sync::Arc<dyn agent_brain_knowledge::KnowledgeStore>>,
    /// Every source list's domains, and when they were read.
    known_domains: std::sync::Mutex<Option<(Instant, Vec<String>)>>,
}

impl SearchSkill {
    pub fn new(telemetry: Option<TelemetryClient>, neo4j: Option<Neo4jClient>) -> Self {
        Self {
            client: Client::new(),
            telemetry,
            neo4j,
            knowledge: None,
            known_domains: std::sync::Mutex::new(None),
        }
    }

    /// The union of every `:SourceList`'s domains (cached ten minutes): what
    /// "known source" means in the ledger.
    async fn known_domains(&self) -> Vec<String> {
        if let Ok(guard) = self.known_domains.lock()
            && let Some((at, domains)) = guard.as_ref()
            && at.elapsed().as_secs() < KNOWN_DOMAINS_TTL_SECS
        {
            return domains.clone();
        }
        let Some(neo4j) = &self.neo4j else {
            return vec![];
        };
        let domains: Vec<String> = match neo4j
            .execute(neo4rs::query(
                "MATCH (s:SourceList) UNWIND coalesce(s.domains, []) AS d \
                 RETURN collect(DISTINCT toLower(d)) AS domains",
            ))
            .await
        {
            Ok(rows) => rows
                .first()
                .and_then(|r| r.get::<Vec<String>>("domains").ok())
                .unwrap_or_default(),
            Err(e) => {
                warn!(error = %e, "search: could not read source-list domains");
                vec![]
            }
        };
        if let Ok(mut guard) = self.known_domains.lock() {
            *guard = Some((Instant::now(), domains.clone()));
        }
        domains
    }

    /// Give the prior-knowledge probe hybrid (vector + BM25) recall.
    ///
    /// BM25 alone misses a paraphrase, which is most of what "do we already
    /// know this?" has to catch — the stored note rarely uses the searcher's
    /// wording. `search_notes_readonly` cannot fail the search: an embed error
    /// degrades it to BM25 internally, which is exactly the hand-rolled
    /// behaviour this replaces.
    pub fn with_knowledge(
        mut self,
        knowledge: std::sync::Arc<dyn agent_brain_knowledge::KnowledgeStore>,
    ) -> Self {
        self.knowledge = Some(knowledge);
        self
    }

    // =========================================================================
    // Credential resolution
    // =========================================================================

    // ========================================================================
    // Tool Definitions
    // ========================================================================

    fn search_web_def() -> ToolDefinition {
        ToolDefinition {
            name: "search_web".to_string(),
            description:
                "Search the web. The first engine is picked by rotation across the free \
                 allowances (each engine paced through its day or month), with a failover \
                 ladder behind it so one \
                 exhausted quota does not fail the call. Leave `engine` unset unless the task \
                 needs one engine's particular strength: naming one skips the rotation. \
                 QUERY SYNTAX — Google operators work on EVERY engine (translated for the ones that \
                 do not read them): \"exact phrase\", -word, -\"phrase\", site:example.com, \
                 -site:example.com, filetype:pdf, after:2026-09-01, before:2026-10-01, a OR b. \
                 Use them: site:/-site: to target or exclude outlets, quotes for names and titles, \
                 -word to drop a wrong sense. \
                 FOR CURRENT EVENTS set recency (day/week/month/year) and news:true instead of \
                 writing a date or 'today' into the query — engines rank by relevance, so 'today' \
                 alone returns months-old pages. Phrase it as 'latest …'. \
                 \"google_news\" is SerpApi's Google News (250/month shared quota), only \
                 when named. \
                 Pass source_list to restrict results to an approved domain list stored in the graph."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "The search query"
                    },
                    "engine": engine_property(),
                    "count": {
                        "type": "integer",
                        "description": "Number of results to return (default: 5, max: 20)"
                    },
                    "recency": {
                        "type": "string",
                        "enum": ["day", "week", "month", "year"],
                        "description": "Only results from this window, applied through each engine's \
                                        own date filter. Use it for anything current instead of \
                                        putting a date or 'today' in the query."
                    },
                    "news": {
                        "type": "boolean",
                        "description": "News results only (news vertical on engines that have one)."
                    },
                    "source_list": {
                        "type": "string",
                        "description": "Name of a SourceList node in Neo4j (e.g. 'news'). \
                                        When set, restricts results to approved domains only — \
                                        adds site: operators to the query and post-filters results."
                    },
                    "prior_knowledge": {
                        "type": "boolean",
                        "description": "Prefix results with matching notes, APIs and media the graph \
                                        already holds (default: true). Set false when you need \
                                        EXTERNAL evidence only — e.g. verifying a claim, where our \
                                        own notes restating it are not corroboration."
                    }
                },
                "required": ["query"]
            }),
        }
    }

    fn get_search_usage_def() -> ToolDefinition {
        ToolDefinition {
            name: "get_search_usage".to_string(),
            description:
                "THE answer to any question about our own web searches: which search engines \
                 (exa, tavily, serper, tinyfish, google_news, …) answered, how many \
                 searches each, how many failed and why, who searched (by_caller), each \
                 engine's quality hints (dated, in-window and known-source shares, and the share \
                 of its results later used as claim evidence), where each budgeted engine \
                 stands in the rotation, and a per-day breakdown — read from the telemetry \
                 ledger, exact. Use it for \"which engines did we use\", \"how many searches\", \
                 quota burn against a free-tier cap. Notes do not record this; never infer it \
                 from them. (LLM models and providers are a different question: get_llm_budget.)"
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "window_hours": {
                        "type": "integer",
                        "description": "Restrict to the last N hours (default 720, i.e. ~30 days, \
                                        which matches a monthly quota window). Omit for all time."
                    }
                }
            }),
        }
    }

    /// The approved domains of a SourceList node (empty when it does not
    /// resolve, which degrades to an unrestricted search). They become `site:`
    /// for the Google family and include-domain parameters for Exa/Tavily, and
    /// the results are post-filtered against them either way.
    async fn source_list_domains(&self, source_list_name: &str) -> Vec<String> {
        let Some(ref neo4j) = self.neo4j else {
            return vec![];
        };
        neo4j
            .get_source_list(source_list_name)
            .await
            .unwrap_or_default()
    }

    /// Return true if `url` matches any approved domain (host ends with domain or equals it).
    fn url_matches_any(url: &str, domains: &[String]) -> bool {
        if domains.is_empty() {
            return true;
        }
        // Extract host from URL cheaply — everything between "://" and the next "/".
        let host = url
            .split_once("://")
            .map(|x| x.1)
            .unwrap_or(url)
            .split('/')
            .next()
            .unwrap_or(url)
            .split(':')
            .next()
            .unwrap_or(url)
            .to_lowercase();
        domains
            .iter()
            .any(|d| host == *d || host.ends_with(&format!(".{d}")))
    }

    // ========================================================================
    // Engine failover
    // ========================================================================

    /// Ordered list of engines to try, most-preferred first.
    ///
    /// Base order comes from `SEARCH_ENGINE_ORDER` (comma-separated), defaulting
    /// to the engines' declared `rank` (`search-engines/*.yaml`; the rationale
    /// is in the dev note "Web Search: engine failover ladder").
    ///
    /// An explicitly requested `engine` is promoted to the head but does **not**
    /// truncate the ladder: callers asking for a specific engine still want an
    /// answer more than they want that engine.
    ///
    /// Engines that hit `quota_exhausted` in the last `QUOTA_COOLDOWN_HOURS` are
    /// moved to the back rather than dropped — a quota that reset overnight
    /// should recover on its own, but should not cost a round-trip on all eight
    /// searches of the daily news chain until it does.
    async fn engine_ladder(
        &self,
        requested: Option<&str>,
        spec: &QuerySpec,
        today: chrono::NaiveDate,
    ) -> (Vec<String>, Head) {
        let configured = std::env::var("SEARCH_ENGINE_ORDER").unwrap_or_default();
        let exhausted = self
            .telemetry
            .as_ref()
            .and_then(|t| {
                t.search_engines_with_recent_errors("quota_exhausted", QUOTA_COOLDOWN_HOURS)
                    .ok()
            })
            .unwrap_or_default();
        // Engines on one account (`quota_group`: serpapi and google_news) share
        // its quota. A quota error is recorded against whichever was called,
        // but exhausting the account exhausts both, so the siblings are demoted
        // too, or the ladder keeps hammering a spent account under another name.
        let engines = search_engines::engines();
        let exhausted = search_engines::quota_siblings(engines, &exhausted);
        let ladder = order_engines(
            &configured,
            &search_engines::default_order(engines),
            requested,
            &exhausted,
        );

        // Drop engines with no credentials configured. A missing key produces
        // `not_configured` on every call — the 2026-09-08 GDELT session put
        // `engine: google` on each search, and with no GOOGLE_API_KEY every one
        // wasted its first rung (and log line) before falling through to
        // the next engine, helping exhaust the 8-round tool budget twice. Unlike a spent
        // quota this never recovers at runtime, so remove rather than demote.
        // An engine no file declares is kept: its call reports "unsupported".
        let keep: Vec<String> = ladder
            .iter()
            .filter(|e| search_engines::get(e).is_none_or(search_engines::configured))
            .cloned()
            .collect();
        let ladder = retain_configured(ladder, &keep);
        if !rotation_enabled() {
            return (ladder, Head::Ladder);
        }
        let rungs: Vec<Rung> = ladder
            .iter()
            .map(|name| self.rung(name, &exhausted, spec, today))
            .collect();
        let pinned = requested
            .map(|r| r.trim().to_lowercase())
            .filter(|r| !r.is_empty());
        if pinned.is_none()
            && let Some(tool) = caller().1
        {
            let leads: Vec<String> = engines
                .iter()
                .filter(|e| e.lead_for.contains(&tool))
                .map(|e| e.name.clone())
                .collect();
            if let Some(head) = search_rotation::lead(&rungs, &leads) {
                return (search_rotation::order(&rungs, Some(head)).0, Head::Lead);
            }
        }
        let (order, rotated) = search_rotation::order(&rungs, pinned.as_deref());
        (
            order,
            if rotated {
                Head::Rotation
            } else {
                Head::Ladder
            },
        )
    }

    /// One ladder engine as the rotation sees it. Outside the rotation
    /// (`standing: None`): no budget, or it cannot serve this query. A
    /// recently spent quota is `Spent` whatever the ledger's arithmetic says
    /// (a vendor that refills on the signup anniversary).
    fn rung(
        &self,
        name: &str,
        exhausted: &[String],
        spec: &QuerySpec,
        today: chrono::NaiveDate,
    ) -> Rung {
        let declared = search_engines::get(name);
        let budget = declared.and_then(|e| e.budget.as_ref());
        let serves =
            declared.is_some_and(|e| search_engines::unsupported(e, spec, today).is_none());
        let mut rung = Rung {
            name: name.to_string(),
            standing: None,
            day_calls: 0,
            weight: budget.and_then(|b| b.weight).unwrap_or(1.0),
        };
        let (Some(b), true) = (budget, serves) else {
            return rung;
        };
        if exhausted.iter().any(|e| e == name) {
            rung.standing = Some(Standing::Spent);
            return rung;
        }
        let now = chrono::Utc::now();
        let Some(t) = &self.telemetry else {
            // No ledger to pace against: every budgeted engine is eligible.
            rung.standing = Some(Standing::OnPace);
            return rung;
        };
        match t.engine_use(name, search_rotation::period_start(b.per, now)) {
            Ok(u) => {
                rung.day_calls = u.day_calls;
                rung.standing = Some(search_rotation::standing(b, &u, now));
            }
            Err(e) => warn!(engine = %name, error = %e, "search rotation: ledger read failed"),
        }
        rung
    }

    /// Write one row to the search usage ledger. Never fails the search.
    fn record_usage(&self, attempt: &SearchAttempt) {
        if let Some(ref t) = self.telemetry
            && let Err(e) = t.record_search_attempt(attempt)
        {
            warn!(engine = %attempt.engine, error = %e, "Failed to record search usage");
        }
    }

    /// Plain text of a `ToolCallResult`'s first content block.
    fn result_text(result: &ToolCallResult) -> String {
        result
            .content
            .first()
            .and_then(|c| {
                if let agent_brain_protocol::Content::Text { text } = c {
                    Some(text.clone())
                } else {
                    None
                }
            })
            .unwrap_or_default()
    }

    /// Remove results that are a search engine's own results page
    /// (`ecosia.org/search?q=…`). Returns the result unchanged when nothing is
    /// dropped or the text is not a result array.
    fn drop_search_pages(result: ToolCallResult) -> (ToolCallResult, usize) {
        if result.is_error == Some(true) {
            return (result, 0);
        }
        let Ok(items) = serde_json::from_str::<Vec<Value>>(&Self::result_text(&result)) else {
            return (result, 0);
        };
        let before = items.len();
        let kept: Vec<Value> = items
            .into_iter()
            .filter(|item| {
                let link = item
                    .get("link")
                    .or_else(|| item.get("url"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                !is_search_page(link)
            })
            .collect();
        let dropped = before - kept.len();
        if dropped == 0 {
            return (result, 0);
        }
        (ToolCallResult::success_json(kept), dropped)
    }

    /// Number of results in a successful engine response.
    /// The results of a successful engine response; unparseable output is
    /// none, so it falls through like an empty answer.
    fn result_items(result: &ToolCallResult) -> Vec<Value> {
        serde_json::from_str::<Vec<Value>>(&Self::result_text(result)).unwrap_or_default()
    }

    // ========================================================================
    // Tool Handlers
    // ========================================================================

    /// What the graph ALREADY holds on this query, rendered as a block that
    /// leads every `search_web` result.
    ///
    /// The brain's default reflex was to reach outward. Asked to produce an
    /// OpenAPI doc for Home Assistant, a worker went web-searching for a spec —
    /// while `(:ApiContext {name:'home_assistant'})` sat in the graph with 20
    /// endpoints already ingested, listed in that same conversation two turns
    /// earlier. The same shape costs far more on the research and media paths,
    /// where re-searching a topic the brain studied last week is invisible
    /// waste: no error, no duplicate-detection, just a second set of notes
    /// saying what the first set said.
    ///
    /// Consolidation cleans duplicates up AFTERWARDS. This is the half that
    /// stops them being created — the "wait, do I already know this?" question,
    /// asked automatically so no model has to remember to ask it.
    ///
    /// Deliberately UNCONDITIONAL and in-band. A prompt rule telling an agent to
    /// check the graph first is the weak half: it was ignored in three separate
    /// incidents this month. Putting the answer at position zero of the result
    /// the agent is already reading requires nobody to remember anything.
    ///
    /// Fails open in every direction — a graph error, a missing index, or no
    /// Neo4j at all yields `None` and the search proceeds untouched. This must
    /// never be able to break a search.
    async fn prior_knowledge(&self, query: &str) -> Option<String> {
        let neo4j = self.neo4j.as_ref()?;
        let mut lines: Vec<String> = Vec::new();

        // ── Notes: hybrid (vector + BM25) when a knowledge store is wired,
        // BM25 alone otherwise.
        //
        // Hybrid matters here more than almost anywhere: the stored note
        // rarely repeats the searcher's wording, and a keyword-only probe
        // reports "we know nothing about this" for a paraphrase. It is safe
        // because `search_notes_readonly` degrades to BM25 internally on an
        // embed error, and READONLY because a housekeeping probe must not bump
        // access counts and quietly reshape the spaced-repetition schedule.
        //
        // Activity logs are excluded either way. `chat_session` and
        // `scheduler_dispatch` record that something HAPPENED, not what is
        // KNOWN — and because they quote user questions verbatim they score
        // extremely well on the very query being asked, crowding out real
        // knowledge with transcripts of someone asking about it.
        const LOG_CONTEXTS: [&str; 2] = ["chat_session", "scheduler_dispatch"];
        let mut note_hits: Vec<(String, String, String)> = Vec::new();
        let mut note_total: i64 = 0;

        if let Some(k) = &self.knowledge
            && let Ok(rows) = k.search_notes_readonly(query, 12, 0, None, None).await
            && !rows.is_empty()
        {
            // The hybrid path returns only {id, content}, so date and
            // source_context come from one follow-up lookup rather than by
            // widening a return shape a dozen other callers depend on.
            let ids: Vec<String> = rows
                .iter()
                .filter_map(|r| r.get("id").and_then(Value::as_str).map(str::to_string))
                .collect();
            if let Ok(meta) = neo4j
                .execute(
                    neo4rs::query(
                        "MATCH (n:Note) WHERE n.id IN $ids \
                           AND NOT coalesce(n.source_context, '') IN $logs \
                         RETURN n.id AS id, \
                                substring(toString(n.created_at), 0, 10) AS d, \
                                coalesce(n.source_context, '') AS sc, \
                                left(n.content, 160) AS t",
                    )
                    .param("ids", ids)
                    .param("logs", LOG_CONTEXTS.to_vec()),
                )
                .await
            {
                for row in &meta {
                    note_hits.push((
                        row.get::<String>("d").unwrap_or_default(),
                        row.get::<String>("sc").unwrap_or_default(),
                        row.get::<String>("t").unwrap_or_default(),
                    ));
                }
            }
            note_total = note_hits.len() as i64;
        }

        if note_hits.is_empty() {
            // No store wired, or hybrid found nothing — fall back to the
            // fulltext index directly so the probe still works standalone.
            let lucene = agent_brain_knowledge::knowledge::sanitize_lucene_query(query);
            if let Ok(rows) = neo4j
                .execute(
                    neo4rs::query(
                        "CALL db.index.fulltext.queryNodes('note_content_fulltext', $q, \
                         {limit: 60}) \
                         YIELD node AS n, score \
                         WHERE NOT (n)-[:PART_OF]->() \
                           AND NOT coalesce(n.source_context, '') IN $logs \
                         RETURN count(n) AS total, \
                                collect({ \
                                  t: left(n.content, 100), \
                                  d: substring(toString(n.created_at), 0, 10), \
                                  s: coalesce(n.source_context, '') \
                                })[0..3] AS top",
                    )
                    .param("q", lucene.as_str())
                    .param("logs", LOG_CONTEXTS.to_vec()),
                )
                .await
                && let Some(row) = rows.first()
            {
                note_total = row.get::<i64>("total").unwrap_or(0);
                if let Ok(top) = row.get::<Vec<Value>>("top") {
                    for item in top.iter() {
                        note_hits.push((
                            item.get("d")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string(),
                            item.get("s")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string(),
                            item.get("t")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string(),
                        ));
                    }
                }
            }
        }

        if note_total > 0 {
            lines.push(format!(
                "· {note_total} stored note(s) already match this query. Most relevant:"
            ));
            for (d, sc, t) in note_hits.iter().take(3) {
                let sc = if sc.is_empty() {
                    String::new()
                } else {
                    format!(" [{sc}]")
                };
                // Retrieval labels ("[CLAIM · unverified …]", "[SOURCE RECORD …]")
                // are baked into stored content and would eat the whole preview
                // window, showing the label instead of the note.
                let body = t.replace('\n', " ");
                let body = match (body.find('['), body.find(']')) {
                    (Some(0), Some(end)) => body[end + 1..].trim().to_string(),
                    _ => body.trim().to_string(),
                };
                lines.push(format!(
                    "    ({d}){sc} {}",
                    body.chars().take(100).collect::<String>()
                ));
            }
        }

        // ── APIs: an ingested endpoint graph beats any spec found on the web.
        if let Ok(rows) = neo4j
            .execute(
                neo4rs::query(
                    "MATCH (c:ApiContext) \
                     WHERE any(w IN $words WHERE toLower(c.name) CONTAINS w) \
                     OPTIONAL MATCH (c)-[:EXPOSES]->(e:ApiEndpoint) \
                     WITH c, count(e) AS n WHERE n > 0 \
                     RETURN c.name AS name, n ORDER BY n DESC LIMIT 3",
                )
                .param("words", Self::query_words(query)),
            )
            .await
        {
            for row in &rows {
                lines.push(format!(
                    "· API ALREADY INGESTED: `{}` with {} endpoints — use list_endpoints(\"{}\"), \
                     do NOT search the web for its spec.",
                    row.get::<String>("name").unwrap_or_default(),
                    row.get::<i64>("n").unwrap_or(0),
                    row.get::<String>("name").unwrap_or_default(),
                ));
            }
        }

        // ── Media: "have we watched this already?"
        //
        // TWO matching words minimum. A single word is not evidence: the first
        // build matched "Home Assistant REST API" against three videos on the
        // strength of "home" alone — a SmartThings pricing rant, an Arizona
        // sober-living exposé, and a 9/11 archive piece. A block full of
        // confident false positives is worse than no block, because it trains
        // the reader to skip it.
        if let Ok(rows) = neo4j
            .execute(
                neo4rs::query(
                    "MATCH (m:Media) \
                     WITH m, [w IN $words WHERE toLower(coalesce(m.title, '')) CONTAINS w] AS hits \
                     WHERE size(hits) >= 2 \
                     RETURN m.title AS title, m.url AS url, \
                            substring(toString(m.ingested_at), 0, 10) AS d \
                     ORDER BY m.ingested_at DESC LIMIT 3",
                )
                .param("words", Self::query_words(query)),
            )
            .await
        {
            for row in &rows {
                lines.push(format!(
                    "· ALREADY WATCHED ({}): {} — {}",
                    row.get::<String>("d").unwrap_or_default(),
                    row.get::<String>("title").unwrap_or_default(),
                    row.get::<String>("url").unwrap_or_default(),
                ));
            }
        }

        if lines.is_empty() {
            return None;
        }
        Some(format!(
            "═════ WHAT WE ALREADY KNOW (checked before searching) ═════\n{}\n\
             Read this FIRST. If it answers the question, say so and cite the stored \
             note — do not re-derive it from the web results below, and do not store a \
             second copy of something we already have. Use the web results for what is \
             genuinely NEW or needs updating.\n\
             ═══════════════════════════════════════════════════════════\n",
            lines.join("\n")
        ))
    }

    /// Lowercased, de-noised words from a query, for CONTAINS matching against
    /// names and titles. Short words are dropped — matching on "the" or "a"
    /// would make every API and every video look relevant.
    fn query_words(query: &str) -> Vec<String> {
        // Generic protocol/doc vocabulary is filtered because it appears in
        // almost every technical query — an unfiltered "rest" or "endpoint"
        // makes every ingested API look like a match for every search, which
        // turns the prior-knowledge block into noise nobody reads.
        const STOP: &[&str] = &[
            "the",
            "and",
            "for",
            "with",
            "from",
            "how",
            "what",
            "does",
            "api",
            "apis",
            "spec",
            "specs",
            "docs",
            "documentation",
            "openapi",
            "swagger",
            "about",
            "into",
            "that",
            "this",
            "your",
            "our",
            "rest",
            "http",
            "https",
            "json",
            "yaml",
            "endpoint",
            "endpoints",
            "reference",
            "guide",
            "using",
            "write",
            "search",
            "find",
            "look",
        ];
        let mut out: Vec<String> = query
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .map(|w| w.to_lowercase())
            .filter(|w| w.len() > 3 && !STOP.contains(&w.as_str()))
            .collect();
        out.sort();
        out.dedup();
        out.truncate(6);
        out
    }

    /// Prepend [`prior_knowledge`] to a successful search result.
    async fn with_prior_knowledge(&self, query: &str, result: ToolCallResult) -> ToolCallResult {
        if result.is_error == Some(true) {
            return result;
        }
        let Some(block) = self.prior_knowledge(query).await else {
            return result;
        };
        let body = Self::result_text(&result);
        ToolCallResult {
            content: vec![agent_brain_protocol::Content::text(format!(
                "{block}\n{body}"
            ))],
            is_error: result.is_error,
            bulk: result.bulk,
        }
    }

    async fn handle_search_web(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: SearchInput = match parse_args(arguments) {
            Ok(input) => input,
            Err(e) => return e,
        };

        let count = input.count.unwrap_or(5).clamp(1, 20);

        // Parse Google syntax once; each engine renders what it understands.
        let mut spec = QuerySpec::parse(&input.query);
        spec.recency = input.recency.as_deref().and_then(Recency::parse);
        spec.news = input.news;

        // Resolve source list restriction if requested.
        let allowed_domains = if let Some(ref list_name) = input.source_list {
            let domains = self.source_list_domains(list_name).await;
            spec.add_sites(&domains);
            domains
        } else {
            vec![]
        };
        let effective_query = spec.display();

        // Build the failover ladder. A single dead engine must not take the
        // whole chain with it: an exhausted SerpApi free tier previously killed
        // 39 jobs and 38 tasks because `search_web` hard-failed on one engine.
        // Its head is the rotation's pick (`search_rotation.rs`).
        let today = chrono::Utc::now().date_naive();
        let (ladder, head) = self
            .engine_ladder(input.engine.as_deref(), &spec, today)
            .await;

        info!(
            query = %effective_query,
            ladder = ?ladder,
            head = head.as_str(),
            requested_engine = ?input.engine,
            source_list = ?input.source_list,
            "Searching web"
        );

        // Every attempt of this call shares one ledger record shape.
        let (purpose, tool, task_id) = caller();
        let base = SearchAttempt {
            call_id: uuid::Uuid::new_v4().to_string(),
            query: effective_query.clone(),
            requested: input.engine.as_deref().map(str::to_lowercase),
            purpose: Some(purpose),
            tool,
            task_id,
            recency: input.recency.clone(),
            news: input.news,
            ..Default::default()
        };
        let known = self.known_domains().await;
        let mut attempts_made = 0usize;
        let mut picked = || {
            attempts_made += 1;
            match attempts_made {
                1 if base.requested.is_some() => "pinned",
                1 => head.as_str(),
                _ => "fallback",
            }
            .to_string()
        };

        let mut failures: Vec<String> = Vec::new();
        let mut result: Option<ToolCallResult> = None;
        // An engine that answers `200` with zero results is not an answer — it
        // is a silent outage. SearXNG (since removed) lost DNS on 2026-08-18 and every upstream
        // reported "HTTP connection error", so it returned a well-formed empty
        // result set for three days; because that is not an error the ladder
        // stopped at the first rung and never tried Google/SerpApi/Brave, and
        // every daily news brief since came out empty with nothing logged.
        //
        // Empty now falls through to the next engine. If every engine comes
        // back empty we return that empty result rather than an error: "nobody
        // has anything on this query" is a legitimate outcome, and erroring
        // would burn the job's retries and fail the owning Task through
        // chain-death attribution.
        let mut empty_success: Option<ToolCallResult> = None;

        for engine in &ladder {
            let declared = search_engines::get(engine);
            if let Some(why) = declared.and_then(|e| search_engines::unsupported(e, &spec, today)) {
                info!(engine = %engine, reason = %why, "Search engine skipped — cannot serve this query");
                continue;
            }
            let started = Instant::now();
            let mut units: Option<f64> = None;
            let attempt = match declared {
                Some(e) => match search_engines::run(&self.client, e, &spec, count).await {
                    Ok(answer) => {
                        units = Some(answer.units);
                        ToolCallResult::success_json(answer.items)
                    }
                    Err(text) => ToolCallResult::error(text),
                },
                None => ToolCallResult::error(format!("Unsupported search engine: {engine}")),
            };
            let duration_ms = started.elapsed().as_millis() as i64;
            let mut row = SearchAttempt {
                engine: engine.clone(),
                duration_ms: Some(duration_ms),
                picked: picked(),
                ..base.clone()
            };

            if attempt.is_error == Some(true) {
                let error_text = Self::result_text(&attempt);
                let kind = classify_search_error(&error_text);
                warn!(
                    engine = %engine,
                    error_kind = kind,
                    error = %error_text,
                    "Search engine failed — trying next in ladder"
                );
                row.success = false;
                row.error_kind = Some(kind.to_string());
                self.record_usage(&row);
                failures.push(format!("{engine}: {error_text}"));
                continue;
            }

            // Before counting: an engine whose only "hits" are other engines'
            // results pages has not answered, and must fall through like an
            // empty one rather than end the ladder.
            let (attempt, search_pages) = Self::drop_search_pages(attempt);
            if search_pages > 0 {
                info!(
                    engine = %engine,
                    dropped = search_pages,
                    "Dropped search-results pages from engine answer"
                );
            }

            let items = Self::result_items(&attempt);
            let n = items.len();
            let quality = search_quality::assess(&items, &spec, &known, chrono::Utc::now());
            row.success = true;
            row.result_count = Some(n as i64);
            row.units = units;
            row.dated = Some(quality.dated);
            row.in_window = quality.in_window;
            row.known_source = Some(quality.known_source);
            row.domains = Some(quality.domains);
            row.links = quality.links;
            self.record_usage(&row);

            if n == 0 {
                warn!(
                    engine = %engine,
                    query = %effective_query,
                    "Search engine returned zero results — trying next in ladder"
                );
                failures.push(format!("{engine}: returned 0 results"));
                // Keep the first one so an all-empty ladder still returns `[]`
                // in the engine's own shape rather than an error.
                empty_success.get_or_insert(attempt);
                continue;
            }

            result = Some(attempt);
            break;
        }

        let result = match result.or(empty_success) {
            Some(r) => r,
            None => {
                // Every engine is down. Surface all of them — knowing that SerpApi
                // is out of quota AND Google CSE is disabled is the difference
                // between a one-line fix and a day of guessing.
                return ToolCallResult::error(format!(
                    "All search engines failed ({} tried).\n{}",
                    ladder.len(),
                    failures.join("\n")
                ));
            }
        };

        if !failures.is_empty() {
            warn!(
                query = %effective_query,
                attempts = ?failures,
                "Search ladder fell through at least one engine"
            );
        }

        // Post-filter: drop any result whose URL is not from an approved domain.
        if allowed_domains.is_empty() {
            return self.finish_search(&input, result).await;
        }
        let content_text = result
            .content
            .first()
            .and_then(|c| {
                if let agent_brain_protocol::Content::Text { text } = c {
                    Some(text.as_str())
                } else {
                    None
                }
            })
            .unwrap_or("[]");
        let Ok(items) = serde_json::from_str::<Vec<Value>>(content_text) else {
            return self.finish_search(&input, result).await;
        };
        let filtered: Vec<Value> = items
            .into_iter()
            .filter(|item| {
                let url = item
                    .get("link")
                    .or_else(|| item.get("url"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                Self::url_matches_any(url, &allowed_domains)
            })
            .collect();
        // Source-list path returns here; it must carry the prior-knowledge
        // block too, or a domain-restricted search is the one route that
        // silently skips the "do we already know this?" check.
        self.finish_search(&input, ToolCallResult::success_json(filtered))
            .await
    }

    /// Every return path of `search_web` ends here, so the `prior_knowledge`
    /// opt-out cannot be honoured on one path and forgotten on another.
    async fn finish_search(&self, input: &SearchInput, result: ToolCallResult) -> ToolCallResult {
        if !input.prior_knowledge {
            return result;
        }
        self.with_prior_knowledge(&input.query, result).await
    }

    async fn handle_get_search_usage(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: SearchUsageInput = match parse_args(arguments) {
            Ok(v) => v,
            Err(e) => return e,
        };
        let Some(ref t) = self.telemetry else {
            return ToolCallResult::error(
                "get_search_usage is not available: telemetry is not configured \
                 (set TELEMETRY_DB_PATH)",
            );
        };
        // Default to a 30-day window: monthly caps are the ones that bite.
        let window = input.window_hours.or(Some(720));
        let mut stats = match t.get_search_stats(window) {
            Ok(stats) => stats,
            Err(e) => return ToolCallResult::error(format!("Failed to read search usage: {e}")),
        };
        stats["rotation"] = self.rotation_state(t);
        // read_page's readers ride the same pacing; their ledger is read_usage.
        stats["page_reads"] = crate::read::ReadSkill::new(Some(t.clone())).report(window);
        // Links are kept 90 days; an all-time window reads what is there.
        stats["used"] = self.used_results(t, window.unwrap_or(90 * 24)).await;
        ToolCallResult::success_json(stats)
    }

    /// Each budgeted engine against its allowance now, as the next call's
    /// rotation would see it.
    fn rotation_state(&self, t: &TelemetryClient) -> Value {
        let now = chrono::Utc::now();
        let engines: Vec<Value> = search_engines::engines()
            .iter()
            .filter(|e| search_engines::configured(e))
            .filter_map(|e| {
                let b = e.budget.as_ref()?;
                let u = t
                    .engine_use(&e.name, search_rotation::period_start(b.per, now))
                    .ok()?;
                let round = |x: f64| (x * 1000.0).round() / 1000.0;
                Some(json!({
                    "engine": e.name,
                    "unit": format!("{:?}", b.unit).to_lowercase(),
                    "per": format!("{:?}", b.per).to_lowercase(),
                    "free": b.free,
                    "used": round(search_rotation::used(b, &u)),
                    "allowed_so_far": round(search_rotation::allowance_so_far(b, now)),
                    "standing": format!("{:?}", search_rotation::standing(b, &u, now)),
                    "calls_today": u.day_calls,
                    "weight": b.weight.unwrap_or(1.0),
                }))
            })
            .collect();
        json!({
            "enabled": rotation_enabled(),
            "engines": engines,
            "note": "The next search_web call goes first to the OnPace engine with the fewest \
                     calls today per unit of weight; the rest of the ladder is the fallback.",
        })
    }

    /// Per engine: how many of its logged result links were later used,
    /// cited as claim evidence (`CORROBORATED_BY`/`CONTRADICTED_BY`
    /// `sources`) or ingested as a note's `source_url`.
    async fn used_results(&self, t: &TelemetryClient, window_hours: i64) -> Value {
        let Some(neo4j) = &self.neo4j else {
            return json!({"note": "no graph: used results cannot be matched"});
        };
        let links = match t.search_links_since(window_hours) {
            Ok(l) => l,
            Err(e) => return json!({"error": format!("reading result links failed: {e}")}),
        };
        let since = (chrono::Utc::now() - chrono::Duration::hours(window_hours)).to_rfc3339();
        let cited: std::collections::HashSet<String> = match neo4j
            .execute(
                neo4rs::query(
                    "CALL () { \
                       MATCH ()-[r:CORROBORATED_BY|CONTRADICTED_BY]->() \
                       WHERE r.found_at >= datetime($since) \
                       UNWIND coalesce(r.sources, []) AS u RETURN u \
                       UNION \
                       MATCH (n:Note) WHERE n.created_at >= datetime($since) \
                         AND n.source_url IS NOT NULL RETURN n.source_url AS u } \
                     RETURN collect(DISTINCT u) AS links",
                )
                .param("since", since),
            )
            .await
        {
            Ok(rows) => rows
                .first()
                .and_then(|r| r.get::<Vec<String>>("links").ok())
                .unwrap_or_default()
                .iter()
                .filter_map(|l| search_quality::link_key(l))
                .collect(),
            Err(e) => return json!({"error": format!("reading cited links failed: {e}")}),
        };
        let mut per: std::collections::BTreeMap<String, (i64, i64)> = Default::default();
        for (engine, link) in &links {
            let e = per.entry(engine.clone()).or_default();
            e.0 += 1;
            if search_quality::link_key(link).is_some_and(|k| cited.contains(&k)) {
                e.1 += 1;
            }
        }
        let engines: Vec<Value> = per
            .into_iter()
            .map(|(engine, (results, used))| {
                json!({
                    "engine": engine,
                    "results_logged": results,
                    "used": used,
                    "used_share": if results > 0 {
                        json!(((used as f64 / results as f64) * 1000.0).round() / 1000.0)
                    } else { Value::Null },
                })
            })
            .collect();
        json!({
            "engines": engines,
            "note": "A result is used when its link was later cited as claim evidence or \
                     ingested as a note's source. Links are logged from 2026-10-16 on.",
        })
    }
}

#[async_trait]
impl Skill for SearchSkill {
    fn name(&self) -> &str {
        "Web Search"
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        vec![Self::search_web_def(), Self::get_search_usage_def()]
    }

    async fn execute(&self, tool_name: &str, arguments: Option<Value>) -> Option<ToolCallResult> {
        match tool_name {
            "search_web" => Some(self.handle_search_web(arguments).await),
            "get_search_usage" => Some(self.handle_get_search_usage(arguments).await),
            _ => None,
        }
    }
}

#[derive(Debug, Deserialize)]
struct SearchUsageInput {
    #[serde(default)]
    window_hours: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct SearchInput {
    query: String,
    // Optional filters: `""` means "not specified", not "the engine named ''"
    // or "the source list named ''". Normalized at the boundary so an empty
    // value degrades to the default ladder / unrestricted search rather than a
    // filter that matches nothing. See serde_flex::empty_string_as_none.
    #[serde(default, deserialize_with = "crate::serde_flex::empty_string_as_none")]
    engine: Option<String>,
    /// Accepts both integer and string values (e.g. `10` or `"10"`) so that
    /// Workflow step definitions stored with quoted counts still work.
    #[serde(default, deserialize_with = "deserialize_optional_count")]
    count: Option<u8>,
    /// `day` / `week` / `month` / `year`: only results from that window, mapped
    /// to each engine's own date filter.
    #[serde(default, deserialize_with = "crate::serde_flex::empty_string_as_none")]
    recency: Option<String>,
    /// News results only (Exa `category`, Tavily `topic`, the Serper
    /// news verticals).
    #[serde(default)]
    news: bool,
    /// Name of a SourceList node in Neo4j. When set, restricts results to approved domains.
    #[serde(default, deserialize_with = "crate::serde_flex::empty_string_as_none")]
    source_list: Option<String>,
    /// Prefix the result with what the graph already holds (default on).
    /// A claim verifier MUST turn this off: the block quotes our own notes —
    /// often the very claim being verified — and a judge reading it answers
    /// "supports" with the claim as its own source. Observed 2026-09-12..24:
    /// the corroboration rate went 41% → 84%, 169 edges citing a graph note.
    #[serde(default = "default_true")]
    prior_knowledge: bool,
}

fn default_true() -> bool {
    true
}

fn deserialize_optional_count<'de, D>(deserializer: D) -> Result<Option<u8>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error as _;
    match Option::<serde_json::Value>::deserialize(deserializer)? {
        None => Ok(None),
        Some(serde_json::Value::Number(n)) => Ok(Some(
            n.as_u64()
                .and_then(|v| u8::try_from(v).ok())
                .ok_or_else(|| D::Error::custom("count must be in range 0-255"))?,
        )),
        Some(serde_json::Value::String(s)) => s.parse::<u8>().map(Some).map_err(D::Error::custom),
        Some(other) => Err(D::Error::custom(format!(
            "invalid type for count: expected integer, got {other}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- prior knowledge ------------------------------------------------------

    /// The word filter decides what counts as "do we already know this".
    /// Too loose and every API matches every query; too tight and nothing does.
    #[test]
    fn query_words_drop_noise_and_keep_subjects() {
        let w = SearchSkill::query_words("Home Assistant REST API OpenAPI spec");
        assert!(w.contains(&"home".to_string()) || w.contains(&"assistant".to_string()));
        // Generic doc words would match every ingested API, not this one.
        for noise in ["api", "spec", "openapi", "the", "rest"] {
            assert!(
                !w.contains(&noise.to_string()),
                "{noise} should be filtered"
            );
        }
    }

    #[test]
    fn query_words_are_deduped_and_bounded() {
        let w = SearchSkill::query_words("alpha alpha beta gamma delta epsilon zeta eta theta");
        assert!(w.len() <= 6, "unbounded word list: {w:?}");
        let mut sorted = w.clone();
        sorted.dedup();
        assert_eq!(w.len(), sorted.len(), "duplicates survived");
    }

    /// Short tokens carry no signal and would match almost any title.
    #[test]
    fn query_words_ignore_short_tokens() {
        let w = SearchSkill::query_words("who is he in AI ml");
        assert!(w.is_empty(), "expected nothing usable, got {w:?}");
    }

    /// The graph-first block is on unless a caller opts out — and a verifier's
    /// opt-out must actually parse, or it silently gets the block anyway.
    #[test]
    fn prior_knowledge_defaults_on_and_can_be_turned_off() {
        let on: SearchInput = serde_json::from_value(json!({"query": "q"})).unwrap();
        assert!(on.prior_knowledge);
        let off: SearchInput =
            serde_json::from_value(json!({"query": "q", "prior_knowledge": false})).unwrap();
        assert!(!off.prior_knowledge);
    }

    /// The check must never be able to break a search: no graph, no block, and
    /// the original result passes through untouched.
    #[tokio::test]
    async fn prior_knowledge_fails_open_without_neo4j() {
        let skill = SearchSkill::new(None, None);
        let original = ToolCallResult::success_json(serde_json::json!([{"link": "x"}]));
        let out = skill
            .with_prior_knowledge("anything", original.clone())
            .await;
        assert_eq!(
            SearchSkill::result_text(&out),
            SearchSkill::result_text(&original)
        );
    }

    /// An errored search must not get a "what we already know" preamble — the
    /// caller needs the failure, undecorated.
    #[tokio::test]
    async fn prior_knowledge_is_not_attached_to_an_error() {
        let skill = SearchSkill::new(None, None);
        let err = ToolCallResult::error("all engines failed");
        let out = skill.with_prior_knowledge("anything", err).await;
        assert_eq!(out.is_error, Some(true));
        assert!(SearchSkill::result_text(&out).contains("all engines failed"));
    }

    /// The bundled engines' rank order (`search_engines` tests check the files
    /// still give it).
    fn test_default() -> Vec<String> {
        v(&["exa", "tavily", "serper", "serpapi", "google", "brave"])
    }

    fn order_with_default(c: &str, r: Option<&str>, e: &[String]) -> Vec<String> {
        order_engines(c, &test_default(), r, e)
    }

    /// Without engine files (this test runs outside the repo root) the property
    /// stays valid: no `enum`, which would otherwise reject every value.
    #[test]
    fn engine_property_never_emits_an_empty_enum() {
        let p = engine_property();
        assert_eq!(p["type"], "string");
        if let Some(e) = p.get("enum") {
            assert!(!e.as_array().unwrap().is_empty());
        }
    }

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn default_ladder_follows_the_engine_ranks() {
        assert_eq!(
            order_with_default("", None, &[]),
            v(&["exa", "tavily", "serper", "serpapi", "google", "brave"])
        );
    }

    #[test]
    fn spent_keyed_engines_fall_behind_the_live_ones() {
        let ladder = order_with_default("", None, &v(&["exa", "tavily"]));
        let pos = |e: &str| ladder.iter().position(|x| x == e).unwrap();
        assert!(pos("serper") < pos("exa"));
        assert!(pos("brave") < pos("tavily"));
    }

    #[test]
    fn unconfigured_engines_are_dropped_from_the_ladder() {
        // Only exa + serpapi have keys: google and brave leave the ladder
        // entirely rather than wasting a rung on `not_configured` each call.
        let ladder = order_with_default("", None, &[]);
        assert_eq!(
            retain_configured(ladder, &v(&["exa", "serpapi"])),
            v(&["exa", "serpapi"])
        );
    }

    #[test]
    fn retain_configured_never_empties_the_ladder() {
        // Nothing configured — keep the original so the caller still gets a real
        // per-engine error instead of silently searching nothing.
        let ladder = order_with_default("", None, &[]);
        assert_eq!(retain_configured(ladder.clone(), &[]), ladder);
    }

    #[test]
    fn retain_configured_preserves_order() {
        let ladder = v(&["tavily", "google", "serpapi", "brave"]);
        assert_eq!(
            retain_configured(ladder, &v(&["brave", "tavily"])),
            v(&["tavily", "brave"])
        );
    }

    #[test]
    fn configured_order_overrides_the_default() {
        assert_eq!(
            order_with_default("google, serpapi ,brave", None, &[]),
            v(&["google", "serpapi", "brave"])
        );
    }

    #[test]
    fn blank_configuration_falls_back_to_the_default() {
        assert_eq!(order_with_default("  , ,", None, &[]), test_default());
    }

    #[test]
    fn requested_engine_is_promoted_without_dropping_the_rest() {
        // The whole point of the ladder: asking for serpapi must not mean
        // "fail if serpapi is down".
        let ladder = order_with_default("", Some("serpapi"), &[]);
        assert_eq!(ladder[0], "serpapi");
        assert_eq!(ladder.len(), test_default().len());
        assert!(ladder.contains(&"brave".to_string()));
    }

    #[test]
    fn requested_engine_is_matched_case_insensitively() {
        let ladder = order_with_default("", Some("  SerpApi "), &[]);
        assert_eq!(ladder[0], "serpapi");
        // Promotion must not leave a duplicate behind.
        assert_eq!(ladder.iter().filter(|e| *e == "serpapi").count(), 1);
    }

    #[test]
    fn exhausted_engines_are_demoted_not_removed() {
        let ladder = order_with_default("", None, &v(&["tavily"]));
        assert_eq!(*ladder.last().unwrap(), "tavily");
        assert_eq!(ladder.len(), test_default().len());
    }

    #[test]
    fn exhaustion_outranks_an_explicit_request() {
        // Observed 2026-08-10: SerpApi's free tier was spent, so an explicit
        // engine:"serpapi" step should still be served by a live engine first.
        let ladder = order_with_default("", Some("serpapi"), &v(&["serpapi"]));
        assert_eq!(ladder[0], "exa");
        assert_eq!(*ladder.last().unwrap(), "serpapi");
    }

    #[test]
    fn all_engines_exhausted_preserves_order_rather_than_emptying() {
        let ladder = order_with_default("", None, &test_default());
        assert_eq!(ladder, test_default());
    }

    #[test]
    fn serpapi_exhaustion_is_classified_as_quota() {
        let err = "SerpApi failed: 429 Too Many Requests - {\n  \"error\": \
                   \"Your account has run out of searches.\"\n}";
        assert_eq!(classify_search_error(err), "quota_exhausted");
    }

    #[test]
    fn serpapi_200_quota_error_body_still_classifies_as_quota() {
        // SerpApi reports quota exhaustion as HTTP 200 with an `error` body, not
        // a 429. search_serpapi now surfaces that as `SerpApi error: <msg>`; the
        // ladder must still route it to the quota cooldown, not treat it as a
        // generic api_error (which would keep hammering the exhausted engine).
        let err = "SerpApi error: You've run out of searches for this month.";
        assert_eq!(classify_search_error(err), "quota_exhausted");
    }

    #[test]
    fn serpapi_benign_no_results_is_not_a_quota() {
        // The other 200+error case: a genuinely empty query. It must fall through
        // WITHOUT tripping the 6h quota cooldown — otherwise one obscure query
        // benches SerpApi for everyone.
        let err = "SerpApi error: Google hasn't returned any results for this query.";
        assert_eq!(classify_search_error(err), "api_error");
    }

    #[test]
    fn disabled_google_api_is_not_mistaken_for_a_quota() {
        // A disabled API needs a console click, not a wait for quota reset —
        // conflating the two sends the operator to the wrong place.
        let err = "Google Search failed: 403 - Custom Search API has not been \
                   used in project 125908057711 before or it is disabled.";
        assert_eq!(classify_search_error(err), "api_disabled");
    }

    #[test]
    fn missing_credentials_are_classified_separately() {
        assert_eq!(
            classify_search_error("Brave API key not configured (set BRAVE_API_KEY)"),
            "not_configured"
        );
    }

    #[test]
    fn network_failures_are_classified_separately() {
        assert_eq!(
            classify_search_error("Request failed: error sending request"),
            "network_error"
        );
    }

    #[test]
    fn a_zero_result_answer_does_not_count_as_an_answer() {
        // The signature of the 2026-08-18 SearXNG DNS outage: HTTP 200, well-formed body,
        // no results. `is_error` is false, so only the count distinguishes it
        // from a working engine — and it must not end the ladder.
        let empty = ToolCallResult::success_text("[]");
        assert_eq!(SearchSkill::result_items(&empty).len(), 0);
        assert_ne!(empty.is_error, Some(true));

        let one = ToolCallResult::success_text(
            r#"[{"title":"t","link":"https://example.com","snippet":"s"}]"#,
        );
        assert_eq!(SearchSkill::result_items(&one).len(), 1);
    }

    #[test]
    fn unparseable_output_counts_as_zero_so_the_ladder_moves_on() {
        // A rung that answers with something that is not a result array is no
        // more useful than one that answers with none.
        let junk = ToolCallResult::success_text("<html>rate limited</html>");
        assert_eq!(SearchSkill::result_items(&junk).len(), 0);
    }

    #[test]
    fn serpapi_quota_demotes_google_news_too_and_vice_versa() {
        // The two engines share one SerpApi account, so exhausting either must
        // move both to the back of the ladder (the sibling expansion happens in
        // engine_ladder; order_engines then demotes both).
        let ladder = order_with_default(
            "exa,google,serpapi,brave",
            Some("google_news"),
            &v(&["serpapi", "google_news"]),
        );
        // Both siblings sink below the unmetered engine even though google_news
        // was explicitly requested.
        assert_eq!(ladder[0], "exa");
        assert!(
            ladder.iter().position(|e| e == "serpapi") > ladder.iter().position(|e| e == "brave")
        );
        assert!(
            ladder.iter().position(|e| e == "google_news")
                > ladder.iter().position(|e| e == "brave")
        );
    }
}

#[cfg(test)]
mod keyed_engine_tests {
    use super::*;

    /// Each vendor signals a spent allowance differently; all must demote.
    #[test]
    fn search_pages_are_dropped_and_an_all_echo_answer_counts_as_empty() {
        let mixed = ToolCallResult::success_json(vec![
            json!({"title": "Search X with Ecosia", "link": "https://www.ecosia.org/search?q=x", "snippet": "x"}),
            json!({"title": "Real", "link": "https://www.reuters.com/a", "snippet": "y"}),
        ]);
        let (kept, dropped) = SearchSkill::drop_search_pages(mixed);
        assert_eq!(dropped, 1);
        assert_eq!(SearchSkill::result_items(&kept).len(), 1);

        let echo_only = ToolCallResult::success_json(vec![json!({
            "title": "t", "link": "https://us.search.yahoo.com/search?p=x", "snippet": "x"
        })]);
        let (kept, dropped) = SearchSkill::drop_search_pages(echo_only);
        assert_eq!(dropped, 1);
        assert_eq!(
            SearchSkill::result_items(&kept).len(),
            0,
            "falls through like an empty answer"
        );

        let error = ToolCallResult::error("boom");
        assert_eq!(SearchSkill::drop_search_pages(error).1, 0);
    }

    #[test]
    fn spent_allowances_classify_as_quota_exhausted() {
        for msg in [
            "Exa search failed: quota exhausted — HTTP 402 Payment Required - {}",
            "Tavily search failed: quota exhausted — HTTP 432 <unknown status code> - {}",
            "Serper search failed: quota exhausted — HTTP 400 Bad Request - Not enough credits",
        ] {
            assert_eq!(classify_search_error(msg), "quota_exhausted", "{msg}");
        }
        assert_eq!(
            classify_search_error("Tavily API key not configured (set TAVILY_API_KEY)"),
            "not_configured"
        );
    }
}
