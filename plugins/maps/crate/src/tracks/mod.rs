//! NIA tracks (MAP_PLAN Phase 4a): ship and aircraft positions over the
//! watch areas (`areas/*.yaml`), collected in the background into a DuckDB
//! store of their own, with the coverage that says whether "nothing there"
//! means quiet or unseen.
//!
//! - `ais.rs`: one aisstream.io WebSocket for every ship area; the key goes
//!   in the subscription message, so the stream is this code's, never the
//!   `ws_*` tools' (the key would pass through a model's context).
//! - `opensky.rs`: OpenSky's REST `states/all` per aircraft area, OAuth2
//!   client credentials.
//! - `tool.rs`: `area_traffic`, what workflows ask.
//!
//! The collectors run on the deployed brain only (`background_allowed`) and
//! say what they are doing (`Status`): off and why, running, waiting.

pub mod ais;
pub mod areas;
pub mod opensky;
pub mod store;
pub mod tool;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use agent_brain_plugin_api::PluginContext;
use serde::Serialize;
use serde_json::{Value, json};

pub use areas::Area;
pub use store::Store;

/// Where the store lives unless `TRACKS_DB_PATH` says.
pub const DEFAULT_DB: &str = "/home/agent/telemetry/tracks.duckdb";

/// What one collector is doing.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SourceStatus {
    /// `starting`, `off`, `connecting`, `running`, `waiting`, `error`.
    pub state: String,
    /// Why it is off, waiting or failing.
    pub detail: Option<String>,
    /// Unix seconds the state began.
    pub since: Option<i64>,
    pub last_message_at: Option<i64>,
    pub messages: u64,
    pub stored: u64,
    pub errors: u64,
    /// The areas it watches.
    pub areas: Vec<String>,
}

#[derive(Clone, Default)]
pub struct Status(Arc<Mutex<BTreeMap<&'static str, SourceStatus>>>);

impl Status {
    pub fn set(&self, source: &'static str, state: &str, detail: Option<String>) {
        if let Ok(mut m) = self.0.lock() {
            let s = m.entry(source).or_default();
            if s.state != state {
                s.since = Some(now());
            }
            s.state = state.to_string();
            s.detail = detail;
            if state == "error" {
                s.errors += 1;
            }
        }
    }

    pub fn update(&self, source: &'static str, f: impl FnOnce(&mut SourceStatus)) {
        if let Ok(mut m) = self.0.lock() {
            f(m.entry(source).or_insert_with(|| SourceStatus {
                state: "starting".into(),
                since: Some(now()),
                ..Default::default()
            }));
        }
    }

    pub fn snapshot(&self) -> Value {
        match self.0.lock() {
            Ok(m) => json!(&*m),
            Err(_) => json!({}),
        }
    }
}

/// The tracks: the store (if it opened) and the collectors' status.
#[derive(Clone)]
pub struct Tracks {
    pub store: Result<Store, String>,
    pub status: Status,
    ctx: PluginContext,
}

impl Tracks {
    pub fn open(ctx: &PluginContext) -> Self {
        let path = ctx
            .env("TRACKS_DB_PATH")
            .unwrap_or_else(|| DEFAULT_DB.to_string());
        let store = Store::open(&path).map_err(|e| format!("the tracks store did not open: {e:#}"));
        if let Err(e) = &store {
            tracing::warn!(error = %e, "maps: tracks store");
        }
        Self {
            store,
            status: Status::default(),
            ctx: ctx.clone(),
        }
    }

    /// Every loaded bundle's areas, and the files that did not load.
    pub fn areas(&self) -> (Vec<Area>, Vec<String>) {
        areas::load_all(self.ctx.files("areas"))
    }

    /// Start the collectors (deployed brain only).
    pub fn spawn(&self) {
        if !self.ctx.background_allowed() {
            return;
        }
        let Ok(store) = self.store.clone() else {
            self.status
                .set("ais", "off", Some("the tracks store did not open".into()));
            self.status
                .set("adsb", "off", Some("the tracks store did not open".into()));
            return;
        };
        ais::spawn(self.clone(), store.clone());
        opensky::spawn(self.clone(), store.clone());
        // Retention: once at start, then every 6 hours.
        tokio::spawn(async move {
            loop {
                match store.blocking(|s| s.prune(now())).await {
                    Ok(n) if n > 0 => tracing::info!(pruned = n, "maps: old positions dropped"),
                    Ok(_) => {}
                    Err(e) => tracing::warn!(error = %e, "maps: prune failed"),
                }
                tokio::time::sleep(std::time::Duration::from_secs(6 * 3600)).await;
            }
        });
    }

    pub fn env(&self, key: &str) -> Option<String> {
        self.ctx.env(key).filter(|v| !v.trim().is_empty())
    }
}

pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// The start of the hour `t` falls in.
pub fn hour_of(t: i64) -> i64 {
    t - t.rem_euclid(3600)
}
