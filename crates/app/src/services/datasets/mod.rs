//! External datasets loaded by ETL into their own DuckDB file
//! (`agent_brain_repository::DatasetStore`): GDELT events and ReliefWeb
//! reports. Configuration is YAML in `DATASETS_DIR` (`datasets/*.yaml`), read
//! per call so an edit needs no rebuild.
//!
//! Each dataset has the same three parts: `sync` (extract → transform → load,
//! resumable, logged to `etl_runs`), `run_query` (aggregates + a sample, with
//! the caveats a reader needs), and a review view in `dataset_status`.

pub mod gdelt;
pub mod reliefweb;

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::repository::Neo4jClient;
use crate::services::web_result::registrable_domain;

/// `DATASETS_DIR`, default `datasets`.
pub fn datasets_dir() -> PathBuf {
    PathBuf::from(std::env::var("DATASETS_DIR").unwrap_or_else(|_| "datasets".into()))
}

/// Read `{DATASETS_DIR}/{name}.yaml`.
pub fn load_config<T: DeserializeOwned>(name: &str) -> Result<T> {
    let path = datasets_dir().join(format!("{name}.yaml"));
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("cannot read dataset config {}", path.display()))?;
    serde_yaml::from_str(&text).with_context(|| format!("bad dataset config {}", path.display()))
}

/// HTTP client for dataset downloads: explicit agent, generous timeout.
pub fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(concat!("agent-brain/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .unwrap_or_default()
}

/// Outlets classed by the graph's SourceLists, so a dataset's answers say
/// WHO they come from: `platform` / `state_media` (role lists), the name of an
/// approved list (`osint`, `news`, …), or `unlisted`. A description, not a
/// filter — `unlisted` is "not on our lists", not "unreliable".
#[derive(Debug, Clone, Default)]
pub struct SourceClasses {
    /// (class, domains), role lists first so they win over approved ones.
    lists: Vec<(String, Vec<String>)>,
}

impl SourceClasses {
    pub fn new(mut lists: Vec<(String, Option<String>, Vec<String>)>) -> Self {
        lists.sort_by_key(|(_, role, _)| role.is_none());
        Self {
            lists: lists
                .into_iter()
                .map(|(name, role, domains)| (role.unwrap_or(name), domains))
                .collect(),
        }
    }

    /// Fails open to "everything unlisted" without Neo4j.
    pub async fn load(neo4j: Option<&Neo4jClient>) -> Self {
        let Some(neo4j) = neo4j else {
            return Self::default();
        };
        let rows = neo4j
            .execute(neo4rs::query(
                "MATCH (s:SourceList) RETURN s.name AS name, COALESCE(s.role, '') AS role, \
                 COALESCE(s.domains, []) AS domains",
            ))
            .await
            .unwrap_or_default();
        Self::new(
            rows.iter()
                .filter_map(|r| {
                    let name = r.get::<String>("name").ok()?;
                    let role = r.get::<String>("role").ok().filter(|s| !s.is_empty());
                    let domains = r.get::<Vec<String>>("domains").unwrap_or_default();
                    Some((name, role, domains))
                })
                .collect(),
        )
    }

    pub fn classify(&self, host: &str) -> String {
        let host = host.trim().trim_start_matches("www.").to_lowercase();
        let reg = registrable_domain(&host).unwrap_or_else(|| host.clone());
        for (class, domains) in &self.lists {
            if domains.iter().any(|d| {
                let d = d.to_lowercase();
                host == d || host.ends_with(&format!(".{d}")) || reg == d
            }) {
                return class.clone();
            }
        }
        "unlisted".to_string()
    }

    /// Fold `{key: host, events, articles}` rows into per-class totals plus
    /// the top hosts with their class.
    pub fn mix(&self, host_rows: &[Value], top: usize) -> Value {
        let mut by_class: std::collections::BTreeMap<String, (i64, i64)> = Default::default();
        let mut tops = Vec::new();
        for r in host_rows {
            let host = r["key"].as_str().unwrap_or("");
            let class = self.classify(host);
            let events = r["events"].as_i64().unwrap_or(0);
            let articles = r["articles"].as_i64().unwrap_or(0);
            let e = by_class.entry(class.clone()).or_default();
            e.0 += events;
            e.1 += articles;
            if tops.len() < top {
                tops.push(
                    json!({"host": host, "class": class, "events": events, "articles": articles}),
                );
            }
        }
        let mut classes: Vec<Value> = by_class
            .into_iter()
            .map(|(c, (e, a))| json!({"class": c, "events": e, "articles": a}))
            .collect();
        classes.sort_by_key(|v| -v["events"].as_i64().unwrap_or(0));
        json!({"by_class": classes, "top_hosts": tops})
    }
}

/// The text of a `query_json` scalar, or "".
pub(crate) fn text(v: &Value) -> String {
    v.as_str().map(String::from).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classes() -> SourceClasses {
        SourceClasses::new(vec![
            (
                "news".into(),
                None,
                vec!["bbc.co.uk".into(), "reuters.com".into()],
            ),
            (
                "osint".into(),
                None,
                vec!["bellingcat.com".into(), "otx.alienvault.com".into()],
            ),
            (
                "state-media".into(),
                Some("state_media".into()),
                vec!["trtworld.com".into()],
            ),
            (
                "platforms".into(),
                Some("platform".into()),
                vec!["facebook.com".into()],
            ),
        ])
    }

    #[test]
    fn hosts_are_classed_by_list_with_role_lists_first() {
        let c = classes();
        assert_eq!(c.classify("www.bbc.co.uk"), "news");
        assert_eq!(c.classify("edition.reuters.com"), "news");
        assert_eq!(c.classify("bellingcat.com"), "osint");
        assert_eq!(c.classify("otx.alienvault.com"), "osint");
        assert_eq!(c.classify("trtworld.com"), "state_media");
        assert_eq!(c.classify("m.facebook.com"), "platform");
        assert_eq!(c.classify("radaronline.com"), "unlisted");
    }

    #[test]
    fn mix_totals_by_class_and_keeps_the_top_hosts() {
        let rows = vec![
            json!({"key": "dailymail.com", "events": 67, "articles": 20}),
            json!({"key": "bbc.co.uk", "events": 26, "articles": 9}),
            json!({"key": "radaronline.com", "events": 10, "articles": 2}),
        ];
        let m = classes().mix(&rows, 2);
        assert_eq!(
            m["by_class"][0],
            json!({"class": "unlisted", "events": 77, "articles": 22})
        );
        assert_eq!(m["by_class"][1]["class"], "news");
        assert_eq!(m["top_hosts"].as_array().unwrap().len(), 2);
    }
}
