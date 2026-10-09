//! System: the brain as a machine, for the HBI's System workspace.
//!
//! - `GET /api/system/health` — one call. Every dependency in
//!   `architecture/components.yaml` (`ARCHITECTURE_DIR`) probed in parallel,
//!   plus the scheduler, the queue, the GPU lease, provider budgets and the
//!   running version, and `issues`: what is wrong, worst first. A component
//!   whose URL env var is unset is "not configured", never "down".
//! - `GET /api/system/registry` — what the brain is made of: skills and their
//!   tools, dynamic tools, agents (context profiles), chains, MCP servers,
//!   providers, schedules.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{Extension, Json, response::IntoResponse};
use serde::Serialize;
use serde_json::{Value, json};

use super::rest::{RestState, unavailable};
pub use crate::services::architecture::{Component, architecture_dir, components};

const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Up,
    NotConfigured,
    Degraded,
    Down,
}

#[derive(Debug, Serialize)]
pub struct Probed {
    pub name: String,
    pub label: String,
    pub kind: String,
    pub required: bool,
    pub purpose: String,
    pub status: Status,
    pub latency_ms: Option<u64>,
    /// Scheme, host and port only — never a path with credentials.
    pub endpoint: Option<String>,
    pub detail: String,
}

/// `http://user:pw@host:8080/v1` → `http://host:8080`.
pub fn endpoint_of(url: &str) -> String {
    match reqwest::Url::parse(url) {
        Ok(u) => {
            let host = u.host_str().unwrap_or("?");
            match u.port() {
                Some(p) => format!("{}://{host}:{p}", u.scheme()),
                None => format!("{}://{host}", u.scheme()),
            }
        }
        Err(_) => "(unparseable url)".into(),
    }
}

/// The URL to GET for an http component, or `None` when its env var is unset.
pub fn probe_url(c: &Component, env: impl Fn(&str) -> Option<String>) -> Option<String> {
    let base = env(c.url_env.as_deref()?)?
        .trim()
        .trim_end_matches('/')
        .to_string();
    if base.is_empty() {
        return None;
    }
    let base = match &c.strip_suffix {
        Some(s) => base
            .strip_suffix(s.as_str())
            .unwrap_or(&base)
            .trim_end_matches('/')
            .to_string(),
        None => base,
    };
    Some(format!("{base}{}", c.path.as_deref().unwrap_or("")))
}

async fn probe(c: &Component, state: &RestState, http: &reqwest::Client) -> Probed {
    let started = Instant::now();
    let mut out = Probed {
        name: c.name.clone(),
        label: c.label.clone(),
        kind: c.kind.clone(),
        required: c.required,
        purpose: c.purpose.clone(),
        status: Status::Down,
        latency_ms: None,
        endpoint: None,
        detail: String::new(),
    };
    let ms = |s: Instant| Some(s.elapsed().as_millis() as u64);
    match c.probe.as_str() {
        "neo4j" => match &state.neo4j {
            None => {
                out.status = Status::NotConfigured;
                out.detail = "this brain was started without Neo4j".into();
            }
            Some(n) => match tokio::time::timeout(
                PROBE_TIMEOUT,
                n.execute(neo4rs::query("RETURN 1 AS ok")),
            )
            .await
            {
                Ok(Ok(_)) => {
                    out.status = Status::Up;
                    out.latency_ms = ms(started);
                }
                Ok(Err(e)) => out.detail = format!("query failed: {e}"),
                Err(_) => out.detail = format!("no answer in {}s", PROBE_TIMEOUT.as_secs()),
            },
        },
        "duckdb" => match state.telemetry.clone() {
            None => {
                out.status = Status::NotConfigured;
                out.detail =
                    "TELEMETRY_DB_PATH unset: no telemetry, budgets or model registry".into();
            }
            Some(t) => {
                let r = tokio::time::timeout(
                    PROBE_TIMEOUT,
                    tokio::task::spawn_blocking(move || t.query_raw("SELECT 1 AS ok", 1)),
                )
                .await;
                match r {
                    Ok(Ok(Ok(_))) => {
                        out.status = Status::Up;
                        out.latency_ms = ms(started);
                    }
                    Ok(Ok(Err(e))) => out.detail = format!("query failed: {e}"),
                    Ok(Err(e)) => out.detail = format!("probe panicked: {e}"),
                    Err(_) => {
                        out.detail = format!(
                            "no answer in {}s (database locked?)",
                            PROBE_TIMEOUT.as_secs()
                        )
                    }
                }
            }
        },
        "http" => match probe_url(c, |k| std::env::var(k).ok()) {
            None => {
                out.status = Status::NotConfigured;
                out.detail = format!(
                    "{} is unset",
                    c.url_env.as_deref().unwrap_or("its url env var")
                );
            }
            Some(url) => {
                out.endpoint = Some(endpoint_of(&url));
                match http.get(&url).timeout(PROBE_TIMEOUT).send().await {
                    Ok(r) if r.status().is_success() => {
                        out.status = Status::Up;
                        out.latency_ms = ms(started);
                    }
                    Ok(r) => {
                        // Answering but not ready (llama.cpp says 503 while loading a model).
                        out.status = Status::Degraded;
                        out.latency_ms = ms(started);
                        let code = r.status();
                        let body: String = r
                            .text()
                            .await
                            .unwrap_or_default()
                            .chars()
                            .take(160)
                            .collect();
                        out.detail = format!("HTTP {code} {}", body.trim());
                    }
                    Err(e) if e.is_timeout() => {
                        out.detail = format!("no answer in {}s", PROBE_TIMEOUT.as_secs())
                    }
                    Err(e) if e.is_connect() => {
                        out.detail = "connection refused or host unreachable".into()
                    }
                    Err(e) => out.detail = e.to_string(),
                }
            }
        },
        other => {
            out.status = Status::Degraded;
            out.detail = format!("unknown probe '{other}' in components.yaml");
        }
    }
    out
}

#[derive(Debug, Serialize, PartialEq)]
pub struct Issue {
    /// `critical` (the brain can't do its job) · `warn` · `info`.
    pub severity: &'static str,
    pub area: String,
    pub message: String,
}

/// What is wrong, from the probes and the status blocks. Pure, so tested.
pub fn issues(
    probed: &[Probed],
    scheduler: &Value,
    queue: &Value,
    lease: &Value,
    budget: &Value,
) -> Vec<Issue> {
    let mut out = Vec::new();
    for p in probed {
        match p.status {
            Status::Down => out.push(Issue {
                severity: if p.required { "critical" } else { "warn" },
                area: p.name.clone(),
                message: format!("{} is down: {}", p.label, p.detail),
            }),
            Status::Degraded => out.push(Issue {
                severity: "warn",
                area: p.name.clone(),
                message: format!("{} answers but is not healthy: {}", p.label, p.detail),
            }),
            _ => {}
        }
    }
    if scheduler.pointer("/config/enabled") == Some(&Value::Bool(false)) {
        out.push(Issue {
            severity: "warn",
            area: "scheduler".into(),
            message: "The scheduler is off: no pending task or schedule will run.".into(),
        });
    }
    let errs = scheduler
        .pointer("/state/consecutive_errors")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let budget_n = scheduler
        .pointer("/config/error_budget")
        .and_then(Value::as_i64)
        .unwrap_or(i64::MAX);
    if errs > 0 {
        out.push(Issue {
            severity: if errs >= budget_n { "critical" } else { "warn" },
            area: "scheduler".into(),
            message: format!(
                "{errs} consecutive scheduler errors (it pauses itself at {budget_n}). Last: {}",
                scheduler
                    .pointer("/state/last_error")
                    .and_then(Value::as_str)
                    .unwrap_or("—")
            ),
        });
    }
    if queue.pointer("/coordinator/alive") == Some(&Value::Bool(false)) {
        out.push(Issue {
            severity: "critical",
            area: "queue".into(),
            message: "The job coordinator is not running: queued jobs won't start.".into(),
        });
    }
    if queue.get("enabled") == Some(&Value::Bool(false)) {
        out.push(Issue {
            severity: "warn",
            area: "queue".into(),
            message: "The job queue is disabled.".into(),
        });
    }
    if lease.get("held") == Some(&Value::Bool(true)) {
        let deferred = queue
            .get("deferred_for_gpu_lease")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        out.push(Issue {
            severity: "info",
            area: "gpu".into(),
            message: format!(
                "The GPU is leased to {} until {}; local work waits ({deferred} jobs deferred).",
                lease
                    .get("holder")
                    .and_then(Value::as_str)
                    .unwrap_or("someone"),
                lease
                    .get("expires_at")
                    .and_then(Value::as_str)
                    .unwrap_or("?"),
            ),
        });
    }
    for p in budget
        .get("providers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let name = p
            .get("name")
            .or_else(|| p.get("kind"))
            .and_then(Value::as_str)
            .unwrap_or("?");
        if p.get("enabled") == Some(&Value::Bool(true))
            && p.get("key_configured") == Some(&Value::Bool(false))
        {
            out.push(Issue {
                severity: "warn",
                area: format!("provider:{name}"),
                message: format!(
                    "Provider {name} is enabled but its key ({}) is not set.",
                    p.get("api_key_env").and_then(Value::as_str).unwrap_or("?")
                ),
            });
        }
        for m in p
            .get("models")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if m.get("cooling_down") == Some(&Value::Bool(true)) {
                out.push(Issue {
                    severity: "warn",
                    area: format!("provider:{name}"),
                    message: format!(
                        "{name}/{} is cooling down until {} ({}).",
                        m.get("model").and_then(Value::as_str).unwrap_or("?"),
                        m.get("cooldown_until")
                            .and_then(Value::as_str)
                            .unwrap_or("?"),
                        m.get("cooldown_reason")
                            .and_then(Value::as_str)
                            .unwrap_or("rate limited"),
                    ),
                });
            }
        }
    }
    let rank = |s: &str| match s {
        "critical" => 0,
        "warn" => 1,
        _ => 2,
    };
    out.sort_by_key(|i| rank(i.severity));
    out
}

pub async fn handle_health(Extension(state): Extension<Arc<RestState>>) -> impl IntoResponse {
    let http = reqwest::Client::new();
    let comps = components();
    let probed: Vec<Probed> =
        futures_util::future::join_all(comps.iter().map(|c| probe(c, &state, &http))).await;

    let (scheduler, queue) = match &state.scheduler {
        Some(h) => match h.read().await.as_ref() {
            Some(svc) => (svc.status().await, svc.queue_stats().await),
            None => (
                json!({ "error": "not yet initialised" }),
                json!({ "error": "not yet initialised" }),
            ),
        },
        None => (
            json!({ "error": "not available" }),
            json!({ "error": "not available" }),
        ),
    };
    let lease = crate::services::gpu_lease::status_json();
    let budget = crate::services::budget::snapshot_json();
    let issues = issues(&probed, &scheduler, &queue, &lease, &budget);

    let version = match &state.neo4j {
        Some(n) => n
            .execute(neo4rs::query(
                "MATCH (b:BrainVersion {id: 'current'}) \
                 RETURN b.sha AS sha, b.branch AS branch, b.dirty AS dirty, \
                        b.subject AS subject, toString(b.deployed_at) AS deployed_at",
            ))
            .await
            .ok()
            .and_then(|r| {
                r.first().map(|r| {
                    json!({
                        "sha": r.get::<String>("sha").ok(),
                        "branch": r.get::<String>("branch").ok(),
                        "dirty": r.get::<bool>("dirty").ok(),
                        "subject": r.get::<String>("subject").ok(),
                        "deployed_at": r.get::<String>("deployed_at").ok(),
                    })
                })
            }),
        None => None,
    };

    let overall = if issues.iter().any(|i| i.severity == "critical") {
        "down"
    } else if issues.iter().any(|i| i.severity == "warn") {
        "degraded"
    } else {
        "ok"
    };
    Json(json!({
        "overall": overall,
        "checked_at": chrono::Utc::now().to_rfc3339(),
        "components": probed,
        "components_declared": comps.len(),
        "issues": issues,
        "scheduler": scheduler,
        "queue": queue,
        "gpu_lease": lease,
        "budget": budget,
        "version": version,
    }))
    .into_response()
}

pub async fn handle_registry(Extension(state): Extension<Arc<RestState>>) -> impl IntoResponse {
    let Some(neo4j) = state.neo4j.as_ref() else {
        return unavailable("Neo4j not available");
    };
    let skills: Vec<Value> = match &state.tool_registry {
        Some(r) => r
            .read()
            .await
            .skills_with_tools()
            .into_iter()
            .map(|(skill, tools)| {
                json!({
                    "name": skill,
                    "tools": tools.iter().map(|t| json!({ "name": t.name, "description": t.description })).collect::<Vec<_>>(),
                })
            })
            .collect(),
        None => Vec::new(),
    };
    let tool_count: usize = skills
        .iter()
        .map(|s| s["tools"].as_array().map_or(0, Vec::len))
        .sum();

    let agents: Vec<Value> = match &state.context_builder {
        Some(h) => match h.read().await.as_ref() {
            Some(cb) => cb
                .list_profiles()
                .await
                .iter()
                .map(|p| {
                    json!({
                        "name": p.name,
                        "description": p.description,
                        "tool_count": p.tools.len(),
                        "model_preference": p.model_preference,
                        "provider_hint": p.provider_hint,
                        "spawnable": p.spawnable,
                        "specialty": p.specialty,
                        "data_class": p.data_class,
                    })
                })
                .collect(),
            None => Vec::new(),
        },
        None => Vec::new(),
    };

    let rows = |cypher: &'static str| async move {
        neo4j
            .execute(neo4rs::query(cypher))
            .await
            .unwrap_or_default()
    };
    let chains: Vec<Value> = rows(
        "MATCH (c:SchedulerChain) RETURN c.id AS id, c.name AS name, c.pattern AS pattern, \
         c.description AS description, c.steps AS steps, coalesce(c.no_evaluator, false) AS no_evaluator \
         ORDER BY c.priority, c.name",
    )
    .await
    .iter()
    .map(|r| {
        let steps = serde_json::from_str::<Value>(&r.get::<String>("steps").unwrap_or_default())
            .ok()
            .and_then(|v| v.as_array().map(Vec::len))
            .unwrap_or(0);
        json!({
            "id": r.get::<String>("id").ok(),
            "name": r.get::<String>("name").ok(),
            "pattern": r.get::<String>("pattern").ok(),
            "description": r.get::<String>("description").ok(),
            "steps": steps,
            "no_evaluator": r.get::<bool>("no_evaluator").unwrap_or(false),
        })
    })
    .collect();
    let mcp_servers: Vec<Value> = rows(
        "MATCH (m:McpServer) RETURN m.name AS name, m.url AS url, m.description AS description, \
         coalesce(m.enabled, true) AS enabled, m.auth_scheme AS auth_scheme, m.auth_env_var AS auth_env_var \
         ORDER BY m.name",
    )
    .await
    .iter()
    .map(|r| {
        let env = r.get::<String>("auth_env_var").ok().filter(|s| !s.is_empty());
        json!({
            "name": r.get::<String>("name").ok(),
            "endpoint": r.get::<String>("url").ok().map(|u| endpoint_of(&u)),
            "description": r.get::<String>("description").ok(),
            "enabled": r.get::<bool>("enabled").unwrap_or(true),
            "auth_scheme": r.get::<String>("auth_scheme").ok(),
            // Whether its secret is present — never the secret.
            "auth_configured": env.as_ref().map(|e| std::env::var(e).map(|v| !v.is_empty()).unwrap_or(false)),
            "auth_env_var": env,
        })
    })
    .collect();
    let dynamic_tools: Vec<Value> = rows("MATCH (d:DynamicTool) RETURN d.name AS name, d.description AS description ORDER BY d.name")
        .await
        .iter()
        .map(|r| json!({ "name": r.get::<String>("name").ok(), "description": r.get::<String>("description").ok() }))
        .collect();
    let schedules = rows(
        "MATCH (s:ScheduledTask) RETURN count(s) AS total, \
         sum(CASE WHEN coalesce(s.enabled, true) THEN 1 ELSE 0 END) AS enabled",
    )
    .await;
    let (sched_total, sched_enabled) = schedules
        .first()
        .map(|r| {
            (
                r.get::<i64>("total").unwrap_or(0),
                r.get::<i64>("enabled").unwrap_or(0),
            )
        })
        .unwrap_or((0, 0));

    Json(json!({
        "counts": {
            "skills": skills.len(),
            "tools": tool_count,
            "dynamic_tools": dynamic_tools.len(),
            "agents": agents.len(),
            "chains": chains.len(),
            "mcp_servers": mcp_servers.len(),
            "schedules": sched_total,
            "schedules_enabled": sched_enabled,
        },
        "skills": skills,
        "dynamic_tools": dynamic_tools,
        "agents": agents,
        "chains": chains,
        "mcp_servers": mcp_servers,
        "providers": crate::services::provider_registry::snapshot_json(),
        "routing_policy": crate::services::routing::RoutingPolicy::from_env().describe(),
        "components": components().iter().map(|c| json!({
            "name": c.name, "label": c.label, "kind": c.kind, "required": c.required, "purpose": c.purpose,
        })).collect::<Vec<_>>(),
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comp(url_env: &str, strip: Option<&str>, path: &str) -> Component {
        Component {
            name: "x".into(),
            label: "X".into(),
            kind: "sidecar".into(),
            probe: "http".into(),
            url_env: Some(url_env.into()),
            strip_suffix: strip.map(Into::into),
            path: Some(path.into()),
            required: false,
            purpose: String::new(),
        }
    }

    #[test]
    fn probe_urls_strip_suffixes_and_need_their_env() {
        let env = |k: &str| match k {
            "W" => Some("http://whisper:8000/v1/".to_string()),
            "S" => Some("http://searxng:8080".to_string()),
            "E" => Some("  ".to_string()),
            _ => None,
        };
        assert_eq!(
            probe_url(&comp("W", Some("/v1"), "/health"), env).as_deref(),
            Some("http://whisper:8000/health")
        );
        assert_eq!(
            probe_url(&comp("S", None, "/healthz"), env).as_deref(),
            Some("http://searxng:8080/healthz")
        );
        assert_eq!(probe_url(&comp("MISSING", None, "/h"), env), None);
        assert_eq!(
            probe_url(&comp("E", None, "/h"), env),
            None,
            "blank is unset"
        );
    }

    #[test]
    fn endpoints_never_carry_credentials_or_paths() {
        assert_eq!(
            endpoint_of("http://u:pw@host:8080/v1?key=s"),
            "http://host:8080"
        );
        assert_eq!(endpoint_of("https://mcp.exa.ai/mcp"), "https://mcp.exa.ai");
    }

    fn probed(name: &str, status: Status, required: bool) -> Probed {
        Probed {
            name: name.into(),
            label: name.into(),
            kind: String::new(),
            required,
            purpose: String::new(),
            status,
            latency_ms: None,
            endpoint: None,
            detail: "refused".into(),
        }
    }

    #[test]
    fn issues_rank_required_failures_first() {
        let p = vec![
            probed("whisper", Status::Down, false),
            probed("ollama", Status::Down, true),
            probed("sandbox", Status::NotConfigured, false),
            probed("neo4j", Status::Up, true),
        ];
        let sched = json!({ "config": { "enabled": false, "error_budget": 5 }, "state": { "consecutive_errors": 0 } });
        let queue = json!({ "coordinator": { "alive": true }, "enabled": true });
        let lease =
            json!({ "held": true, "holder": "finetune", "expires_at": "2026-10-09T01:00:00Z" });
        let budget = json!({ "providers": [
            { "name": "gemini", "enabled": true, "key_configured": false, "api_key_env": "GEMINI_API_KEY", "models": [] },
            { "name": "anthropic", "enabled": true, "key_configured": true, "models": [
                { "model": "claude-sonnet-5", "cooling_down": true, "cooldown_until": "soon", "cooldown_reason": "429" } ] },
        ] });
        let i = issues(&p, &sched, &queue, &lease, &budget);
        assert_eq!(i[0].severity, "critical");
        assert_eq!(i[0].area, "ollama");
        assert!(
            i.iter()
                .any(|x| x.area == "whisper" && x.severity == "warn")
        );
        assert!(
            !i.iter().any(|x| x.area == "sandbox"),
            "not configured is not a problem"
        );
        assert!(
            i.iter()
                .any(|x| x.area == "scheduler" && x.message.contains("off"))
        );
        assert!(i.iter().any(|x| x.area == "gpu" && x.severity == "info"));
        assert!(
            i.iter()
                .any(|x| x.area == "provider:gemini" && x.message.contains("GEMINI_API_KEY"))
        );
        assert!(
            i.iter()
                .any(|x| x.area == "provider:anthropic" && x.message.contains("cooling down"))
        );
        assert_eq!(i.last().unwrap().severity, "info");
    }

    #[test]
    fn scheduler_errors_turn_critical_at_the_budget() {
        let sched = |n: i64| json!({ "config": { "enabled": true, "error_budget": 5 }, "state": { "consecutive_errors": n, "last_error": "boom" } });
        let none = json!({});
        assert!(issues(&[], &sched(0), &none, &none, &none).is_empty());
        assert_eq!(
            issues(&[], &sched(2), &none, &none, &none)[0].severity,
            "warn"
        );
        assert_eq!(
            issues(&[], &sched(5), &none, &none, &none)[0].severity,
            "critical"
        );
    }

    #[test]
    fn the_shipped_component_list_parses() {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../architecture/components.yaml");
        let f: crate::services::architecture::ComponentsFile =
            serde_yaml::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
        assert!(f.components.iter().any(|c| c.name == "neo4j" && c.required));
        assert!(
            f.components
                .iter()
                .all(|c| c.probe != "http" || c.url_env.is_some())
        );
    }
}
