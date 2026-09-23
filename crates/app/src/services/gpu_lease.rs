//! GPU lease — Phase 5 of `project-docs/PROVIDER_ROUTING_PLAN.md`.
//!
//! A fine-tuning run needs the whole 8 GB card. The GPU hosts the llama.cpp
//! sidecar, host Ollama's models, and bge-m3, which embeds every stored note and
//! search query. While a lease is held the brain keeps running on cloud
//! providers and stops touching the GPU:
//!
//! - **Routing** refuses every `billing: local` provider (`RoutingPolicy`), so
//!   capability steps, chat pins, ladders, sub-agents and fallback rungs all
//!   move to permitted cloud models or fail with the reason.
//! - **`LlmClient`** refuses local generation and all embeddings, a backstop for
//!   callers that build a client directly (the distiller, the context builder).
//!   A single `search_notes` would otherwise reload bge-m3 mid-training.
//! - **The queue defers local-pinned jobs** (`provider_hint: ollama` with no
//!   capability routing) instead of failing them or pushing a day of local work
//!   onto Ollama Cloud's single free slot. They keep their place and attempts.
//! - **Notes stored meanwhile** are kept without a vector and marked
//!   `embedding_pending`. Search degrades to BM25.
//! - **On acquire** the brain unloads host Ollama's models itself. The llama.cpp
//!   and whisper sidecars are separate containers it cannot stop; the holder
//!   stops them from the host (`workspace/finetune/scripts/gpu_lease.sh`).
//!
//! On release, on expiry, and at a startup with no lease held, the release hook
//! embeds and links the pending notes and extracts their entities.
//!
//! A lease always expires (default 12 h, at most 72 h; the holder renews by
//! acquiring again), so a forgotten one cannot disable local models for good.
//! It is persisted as `(:GpuLease {id:'current'})` and reloaded at startup, so a
//! brain restarted mid-training does not reclaim the GPU.

use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration as StdDuration;

use chrono::{DateTime, Duration, Utc};
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::repository::Neo4jClient;

pub const DEFAULT_TTL_HOURS: i64 = 12;
pub const MAX_TTL_HOURS: i64 = 72;

/// One lease on the GPU.
#[derive(Debug, Clone, PartialEq)]
pub struct Lease {
    pub holder: String,
    pub reason: String,
    pub acquired_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

impl Lease {
    /// The reason given wherever local work is refused.
    pub fn refusal(&self) -> String {
        format!(
            "the GPU is leased to {} ({}) until {} UTC, so local models are unavailable",
            self.holder,
            self.reason,
            self.expires_at.format("%Y-%m-%d %H:%M")
        )
    }

    fn to_json(&self) -> Value {
        json!({
            "holder": self.holder,
            "reason": self.reason,
            "acquired_at": self.acquired_at.to_rfc3339(),
            "expires_at": self.expires_at.to_rfc3339(),
        })
    }
}

/// The lease rules, independent of persistence. Every method takes `now`.
#[derive(Debug, Default)]
pub struct LeaseBook {
    lease: Option<Lease>,
}

impl LeaseBook {
    /// The live lease, if any. An expired lease is not held.
    pub fn current(&self, now: DateTime<Utc>) -> Option<&Lease> {
        self.lease.as_ref().filter(|l| now < l.expires_at)
    }

    /// Take the lease, or renew it for the holder that already has it (the
    /// start time is kept, the expiry moves). Another holder is refused.
    pub fn acquire(
        &mut self,
        holder: &str,
        reason: &str,
        ttl_hours: Option<i64>,
        now: DateTime<Utc>,
    ) -> Result<Lease, String> {
        let holder = holder.trim();
        if holder.is_empty() {
            return Err("gpu_lease acquire needs a `holder` (who is using the GPU)".to_string());
        }
        let ttl = ttl_hours
            .unwrap_or(DEFAULT_TTL_HOURS)
            .clamp(1, MAX_TTL_HOURS);
        let current = self.current(now).cloned();
        if let Some(l) = &current
            && !l.holder.eq_ignore_ascii_case(holder)
        {
            return Err(format!(
                "the GPU is already leased to {} until {} UTC",
                l.holder,
                l.expires_at.format("%Y-%m-%d %H:%M")
            ));
        }
        let reason = reason.trim();
        let lease = Lease {
            holder: holder.to_string(),
            reason: if reason.is_empty() {
                "unspecified".to_string()
            } else {
                reason.to_string()
            },
            acquired_at: current.map(|l| l.acquired_at).unwrap_or(now),
            expires_at: now + Duration::hours(ttl),
        };
        self.lease = Some(lease.clone());
        Ok(lease)
    }

    /// Give the lease back. Only its holder, or `force`, may release a live
    /// lease; releasing when nothing is held is a no-op that returns `None`.
    pub fn release(
        &mut self,
        holder: Option<&str>,
        force: bool,
        now: DateTime<Utc>,
    ) -> Result<Option<Lease>, String> {
        let Some(live) = self.current(now).cloned() else {
            self.lease = None;
            return Ok(None);
        };
        let is_holder = holder.is_some_and(|h| h.trim().eq_ignore_ascii_case(&live.holder));
        if !is_holder && !force {
            return Err(format!(
                "the GPU is leased to {}; release it as that holder, or pass force: true",
                live.holder
            ));
        }
        self.lease = None;
        Ok(Some(live))
    }

    /// Drop a lease that has run out, returning it so the release hook runs.
    fn take_expired(&mut self, now: DateTime<Utc>) -> Option<Lease> {
        if self.lease.as_ref().is_some_and(|l| now >= l.expires_at) {
            self.lease.take()
        } else {
            None
        }
    }
}

// ── Process-wide lease ──────────────────────────────────────────────────────

type ReleaseHook = Arc<dyn Fn() + Send + Sync>;

#[derive(Default)]
struct Global {
    book: LeaseBook,
    neo4j: Option<Neo4jClient>,
    on_release: Option<ReleaseHook>,
}

fn global() -> MutexGuard<'static, Global> {
    static G: OnceLock<Mutex<Global>> = OnceLock::new();
    G.get_or_init(|| Mutex::new(Global::default()))
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

/// Clear the persisted lease and run the release hook. Never called with the
/// global lock held: the hook may re-enter this module.
fn after_release(neo4j: Option<Neo4jClient>, hook: Option<ReleaseHook>) {
    if let (Some(neo4j), Ok(rt)) = (neo4j, tokio::runtime::Handle::try_current()) {
        rt.spawn(async move {
            if let Err(e) = neo4j
                .run(neo4rs::query("MATCH (l:GpuLease {id: 'current'}) DELETE l"))
                .await
            {
                warn!(error = %e, "could not delete the released GpuLease node");
            }
        });
    }
    if let Some(hook) = hook {
        hook();
    }
}

/// The live lease, if any. The first check after a lease runs out clears it
/// and runs the release hook.
pub fn held() -> Option<Lease> {
    let now = Utc::now();
    let (live, expired, neo4j, hook) = {
        let mut g = global();
        let expired = g.book.take_expired(now);
        (
            g.book.current(now).cloned(),
            expired,
            g.neo4j.clone(),
            g.on_release.clone(),
        )
    };
    if let Some(l) = expired {
        info!(holder = %l.holder, "GPU lease expired — local models are available again");
        after_release(neo4j, hook);
    }
    live
}

/// Why local models are unavailable right now, or `None` when they are not.
pub fn refusal() -> Option<String> {
    held().map(|l| l.refusal())
}

/// Reload a lease that survived a restart and register the release hook. With
/// no live lease the hook runs once, so notes left pending by a lease that
/// ended while the brain was down are embedded now.
pub async fn init(neo4j: Neo4jClient, on_release: ReleaseHook) {
    let loaded = load(&neo4j).await;
    let now = Utc::now();
    let live = loaded.filter(|l| now < l.expires_at);
    {
        let mut g = global();
        g.neo4j = Some(neo4j.clone());
        g.on_release = Some(Arc::clone(&on_release));
        g.book.lease = live.clone();
    }
    match live {
        Some(l) => warn!(
            holder = %l.holder,
            reason = %l.reason,
            expires_at = %l.expires_at,
            "GPU lease in force at startup — local models stay unavailable until it is released or expires"
        ),
        None => after_release(Some(neo4j), Some(on_release)),
    }
}

async fn load(neo4j: &Neo4jClient) -> Option<Lease> {
    let rows = neo4j
        .execute(neo4rs::query(
            "MATCH (l:GpuLease {id: 'current'}) \
             RETURN l.holder AS holder, l.reason AS reason, \
                    toString(l.acquired_at) AS acquired_at, toString(l.expires_at) AS expires_at",
        ))
        .await
        .map_err(|e| warn!(error = %e, "could not read the GpuLease node — assuming no lease"))
        .ok()?;
    let row = rows.first()?;
    let at = |key: &str| {
        row.get::<String>(key)
            .ok()
            .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
            .map(|t| t.with_timezone(&Utc))
    };
    Some(Lease {
        holder: row.get::<String>("holder").ok()?,
        reason: row.get::<String>("reason").unwrap_or_default(),
        acquired_at: at("acquired_at")?,
        expires_at: at("expires_at")?,
    })
}

async fn persist(neo4j: &Neo4jClient, lease: &Lease) -> bool {
    let q = neo4rs::query(
        "MERGE (l:GpuLease {id: 'current'}) \
         SET l.holder = $holder, l.reason = $reason, \
             l.acquired_at = datetime($acquired_at), l.expires_at = datetime($expires_at)",
    )
    .param("holder", lease.holder.as_str())
    .param("reason", lease.reason.as_str())
    .param("acquired_at", lease.acquired_at.to_rfc3339())
    .param("expires_at", lease.expires_at.to_rfc3339());
    match neo4j.run(q).await {
        Ok(_) => true,
        Err(e) => {
            warn!(error = %e, "could not persist the GPU lease — it holds until restart");
            false
        }
    }
}

/// Ask host Ollama to unload every model it holds (`keep_alive: 0` unloads
/// generation and embedding models alike, verified against bge-m3), then report
/// what is still loaded. Best effort: an unreachable Ollama holds nothing.
async fn unload_ollama_models() -> Value {
    let base = std::env::var("OLLAMA_LOCAL_URL")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "http://localhost:11434".to_string());
    let client = match reqwest::Client::builder()
        .timeout(StdDuration::from_secs(30))
        .build()
    {
        Ok(c) => c,
        Err(e) => return json!({ "error": e.to_string() }),
    };
    let loaded = |client: reqwest::Client, base: String| async move {
        let v: Value = client
            .get(format!("{base}/api/ps"))
            .send()
            .await
            .ok()?
            .json()
            .await
            .ok()?;
        Some(
            v["models"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|m| m["name"].as_str().map(str::to_string))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default(),
        )
    };
    let Some(before) = loaded(client.clone(), base.clone()).await else {
        return json!({ "error": format!("could not list models at {base}/api/ps") });
    };
    for model in &before {
        let _ = client
            .post(format!("{base}/api/generate"))
            .json(&json!({ "model": model, "keep_alive": 0 }))
            .send()
            .await;
    }
    let still = loaded(client, base).await.unwrap_or_default();
    json!({ "unloaded": before, "still_loaded": still })
}

/// Take (or renew) the lease.
pub async fn acquire(holder: &str, reason: &str, ttl_hours: Option<i64>) -> Result<Value, String> {
    let (lease, neo4j) = {
        let mut g = global();
        let lease = g.book.acquire(holder, reason, ttl_hours, Utc::now())?;
        (lease, g.neo4j.clone())
    };
    let persisted = match &neo4j {
        Some(n) => persist(n, &lease).await,
        None => false,
    };
    let ollama = unload_ollama_models().await;
    info!(
        holder = %lease.holder,
        reason = %lease.reason,
        expires_at = %lease.expires_at,
        "GPU leased — local models refused, local-pinned jobs deferred, embeddings paused"
    );
    Ok(json!({
        "held": true,
        "lease": lease.to_json(),
        "persisted": persisted,
        "ollama": ollama,
        "next": "Stop the llama.cpp and whisper sidecars from the host (docker compose stop \
                 llamacpp whisper) once /api/llm/budget shows no local request in use. \
                 Acquire again as the same holder to extend the lease.",
    }))
}

/// Give the lease back and start the backfill.
pub async fn release(holder: Option<&str>, force: bool) -> Result<Value, String> {
    let (released, neo4j, hook) = {
        let mut g = global();
        let released = g.book.release(holder, force, Utc::now())?;
        (released, g.neo4j.clone(), g.on_release.clone())
    };
    if let Some(l) = &released {
        info!(holder = %l.holder, "GPU lease released — local models are available again");
    }
    after_release(neo4j, hook);
    Ok(json!({
        "held": false,
        "released": released.map(|l| l.to_json()),
        "next": "Start the sidecars (docker compose start llamacpp whisper). Notes stored \
                 during the lease are being embedded and linked in the background.",
    }))
}

/// The lease as JSON.
pub fn status_json() -> Value {
    match held() {
        Some(l) => json!({ "held": true, "lease": l.to_json() }),
        None => json!({ "held": false }),
    }
}

/// The `gpu_lease` tool and `POST /api/gpu-lease`:
/// `{action: status|acquire|release, holder, reason, ttl_hours, force}`.
pub async fn handle(args: Value) -> Result<Value, String> {
    let holder = args["holder"].as_str().unwrap_or("");
    match args["action"].as_str().unwrap_or("status") {
        "status" => Ok(status_json()),
        "acquire" => {
            acquire(
                holder,
                args["reason"].as_str().unwrap_or(""),
                args["ttl_hours"].as_i64(),
            )
            .await
        }
        "release" => {
            release(
                (!holder.trim().is_empty()).then_some(holder),
                args["force"].as_bool().unwrap_or(false),
            )
            .await
        }
        other => Err(format!(
            "unknown gpu_lease action '{other}': use status, acquire or release"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(h: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 23, h, 0, 0).unwrap()
    }

    #[test]
    fn a_lease_blocks_other_holders_until_it_expires() {
        let mut b = LeaseBook::default();
        let l = b.acquire("finetune", "exp 2", Some(4), at(10)).unwrap();
        assert_eq!(l.expires_at, at(14));
        let err = b.acquire("someone-else", "", None, at(11)).unwrap_err();
        assert!(err.contains("already leased to finetune"), "{err}");
        assert!(b.current(at(13)).is_some());
        assert!(b.current(at(14)).is_none(), "expired leases are not held");
        assert!(b.acquire("someone-else", "", None, at(15)).is_ok());
    }

    #[test]
    fn the_holder_renews_and_keeps_the_start_time() {
        let mut b = LeaseBook::default();
        b.acquire("finetune", "exp", Some(2), at(10)).unwrap();
        let renewed = b.acquire("FineTune", "exp", Some(6), at(11)).unwrap();
        assert_eq!(renewed.acquired_at, at(10));
        assert_eq!(renewed.expires_at, at(17));
    }

    #[test]
    fn the_ttl_is_clamped_and_defaults_to_twelve_hours() {
        let mut b = LeaseBook::default();
        assert_eq!(b.acquire("a", "", None, at(0)).unwrap().expires_at, at(12));
        let mut b = LeaseBook::default();
        let l = b.acquire("a", "", Some(10_000), at(0)).unwrap();
        assert_eq!(l.expires_at - l.acquired_at, Duration::hours(MAX_TTL_HOURS));
        let mut b = LeaseBook::default();
        let l = b.acquire("a", "", Some(0), at(0)).unwrap();
        assert_eq!(l.expires_at - l.acquired_at, Duration::hours(1));
    }

    #[test]
    fn a_holder_is_required_and_a_blank_reason_is_named() {
        let mut b = LeaseBook::default();
        assert!(b.acquire("  ", "x", None, at(0)).is_err());
        assert_eq!(
            b.acquire("a", " ", None, at(0)).unwrap().reason,
            "unspecified"
        );
    }

    #[test]
    fn only_the_holder_or_force_releases_a_live_lease() {
        let mut b = LeaseBook::default();
        b.acquire("finetune", "", None, at(10)).unwrap();
        assert!(b.release(Some("other"), false, at(11)).is_err());
        assert!(b.release(None, false, at(11)).is_err());
        assert!(
            b.current(at(11)).is_some(),
            "a refused release changes nothing"
        );
        assert!(b.release(None, true, at(11)).unwrap().is_some());
        assert!(b.current(at(11)).is_none());
    }

    #[test]
    fn releasing_nothing_is_a_no_op() {
        let mut b = LeaseBook::default();
        assert_eq!(b.release(Some("anyone"), false, at(1)).unwrap(), None);
        b.acquire("finetune", "", Some(1), at(1)).unwrap();
        // Expired: nothing live to release, and the stale lease is cleared.
        assert_eq!(b.release(None, false, at(5)).unwrap(), None);
        assert!(b.lease.is_none());
    }

    #[test]
    fn an_expired_lease_is_taken_once_for_the_release_hook() {
        let mut b = LeaseBook::default();
        b.acquire("finetune", "", Some(1), at(1)).unwrap();
        assert!(b.take_expired(at(1)).is_none());
        assert!(b.take_expired(at(2)).is_some());
        assert!(
            b.take_expired(at(3)).is_none(),
            "fires once, not on every check"
        );
    }

    #[test]
    fn the_refusal_names_holder_reason_and_expiry() {
        let mut b = LeaseBook::default();
        let l = b
            .acquire("finetune", "experiment 2", Some(3), at(9))
            .unwrap();
        let r = l.refusal();
        assert!(
            r.contains("finetune") && r.contains("experiment 2") && r.contains("12:00"),
            "{r}"
        );
    }

    #[tokio::test]
    async fn handle_rejects_an_unknown_action() {
        let err = handle(json!({"action": "steal"})).await.unwrap_err();
        assert!(err.contains("unknown gpu_lease action"), "{err}");
    }
}
