//! GDELT 2.0 Events: ETL from the 15-minute export files, and queries.
//!
//! Extract: `lastupdate.txt` names the newest export file; every file is named
//! by its 15-minute UTC stamp, so the pending ones are computed from the last
//! file recorded, not listed. Transform + load: DuckDB reads the unzipped TSV
//! in SQL (`DatasetStore::load_gdelt_file`). A slot GDELT skipped (404 while a
//! newer file exists) is recorded as missing so the resume point moves on.

use std::collections::BTreeMap;
use std::io::Read;

use anyhow::{Context, Result, anyhow};
use chrono::{Duration, NaiveDate, NaiveDateTime, Timelike};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{info, warn};

use super::{SourceClasses, text};
use crate::repository::DatasetStore;

#[derive(Debug, Clone, Deserialize)]
pub struct GdeltConfig {
    pub lastupdate_url: String,
    pub file_url_template: String,
    pub retention_days: i64,
    pub initial_backfill_days: i64,
    pub max_files_per_run: usize,
    pub default_min_sources: i64,
    #[serde(default)]
    pub quad_classes: BTreeMap<u8, String>,
    #[serde(default)]
    pub cameo_root_codes: BTreeMap<String, String>,
}

const STAMP_FMT: &str = "%Y%m%d%H%M%S";

/// The 14-digit stamp in an export file URL.
pub fn stamp_in(url: &str) -> Option<String> {
    let name = url.rsplit('/').next()?;
    let stamp = name.split('.').next()?;
    (stamp.len() == 14 && stamp.bytes().all(|b| b.is_ascii_digit())).then(|| stamp.to_string())
}

fn parse_stamp(s: &str) -> Option<NaiveDateTime> {
    NaiveDateTime::parse_from_str(s, STAMP_FMT).ok()
}

/// Round down to a 15-minute slot.
fn floor_slot(t: NaiveDateTime) -> NaiveDateTime {
    let t = t
        .with_second(0)
        .unwrap_or(t)
        .with_nanosecond(0)
        .unwrap_or(t);
    t - Duration::minutes((t.minute() % 15) as i64)
}

/// Stamps to load, oldest first, at most `cap`: every slot after `after` up
/// to and including `latest`; with no `after`, from `backfill_days` before
/// `latest`.
pub fn pending_stamps(
    after: Option<&str>,
    latest: &str,
    backfill_days: i64,
    cap: usize,
) -> Vec<String> {
    let Some(latest_t) = parse_stamp(latest) else {
        return vec![];
    };
    let mut t = match after.and_then(parse_stamp) {
        Some(a) => floor_slot(a) + Duration::minutes(15),
        None => floor_slot(latest_t - Duration::days(backfill_days)),
    };
    let mut out = Vec::new();
    while t <= latest_t && out.len() < cap {
        out.push(t.format(STAMP_FMT).to_string());
        t += Duration::minutes(15);
    }
    out
}

/// Slots between `after` and `latest` still to load after this run.
fn backlog(after: &str, latest: &str) -> i64 {
    match (parse_stamp(after), parse_stamp(latest)) {
        (Some(a), Some(l)) if l > a => (l - a).num_minutes() / 15,
        _ => 0,
    }
}

/// Removes a scratch directory when dropped, on every exit path.
struct TempDir(std::path::PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The single CSV inside an export zip.
fn unzip_single(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).context("bad zip")?;
    let mut file = archive.by_index(0).context("empty zip")?;
    let mut out = Vec::with_capacity(file.size() as usize);
    file.read_to_end(&mut out)?;
    Ok(out)
}

/// One extract → transform → load run. Never panics on upstream trouble:
/// the error is returned in the report and logged to `etl_runs`.
pub async fn sync(store: &DatasetStore, cfg: &GdeltConfig, http: &reqwest::Client) -> Value {
    let started = chrono::Utc::now();
    let mut calls = 0i64;
    let mut loaded = 0i64;
    let mut missing = 0i64;
    let mut rows = 0i64;

    let result: Result<(String, Option<String>)> = async {
        calls += 1;
        let listing = http
            .get(&cfg.lastupdate_url)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        let latest = listing
            .lines()
            .find_map(|l| {
                l.split_whitespace()
                    .nth(2)
                    .filter(|u| u.contains(".export."))
            })
            .and_then(stamp_in)
            .ok_or_else(|| anyhow!("lastupdate.txt named no export file: {listing:.200}"))?;
        let after = store.last_file("gdelt")?;
        let stamps = pending_stamps(
            after.as_deref(),
            &latest,
            cfg.initial_backfill_days,
            cfg.max_files_per_run,
        );
        let tmp = std::env::temp_dir().join(format!("gdelt-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp)?;
        let _cleanup = TempDir(tmp.clone());
        let mut last_done = after.clone();
        for stamp in stamps {
            let url = cfg.file_url_template.replace("{stamp}", &stamp);
            calls += 1;
            let resp = http.get(&url).send().await?;
            if resp.status() == reqwest::StatusCode::NOT_FOUND {
                if stamp < latest {
                    // GDELT skipped this slot; a newer file exists.
                    store.mark_missing("gdelt", &stamp)?;
                    missing += 1;
                    last_done = Some(stamp);
                    continue;
                }
                break; // the newest slot is not published yet
            }
            let bytes = resp.error_for_status()?.bytes().await?;
            let csv = unzip_single(&bytes).with_context(|| format!("export {stamp}"))?;
            let path = tmp.join(format!("{stamp}.csv"));
            std::fs::write(&path, csv)?;
            rows += store.load_gdelt_file(&stamp, &path)? as i64;
            std::fs::remove_file(&path).ok();
            loaded += 1;
            last_done = Some(stamp);
        }
        Ok((latest, last_done))
    }
    .await;

    let pruned = store.prune_gdelt(cfg.retention_days).unwrap_or_else(|e| {
        warn!(error = %e, "gdelt: prune failed");
        0
    }) as i64;
    if let Err(e) = store.checkpoint() {
        warn!(error = %e, "gdelt: checkpoint failed");
    }
    let error = result.as_ref().err().map(|e| format!("{e:#}"));
    let _ = store.record_run(
        "gdelt",
        &started.format("%Y-%m-%d %H:%M:%S").to_string(),
        calls,
        loaded,
        rows,
        pruned,
        error.as_deref(),
    );
    info!(loaded, missing, rows, pruned, error = ?error, "gdelt sync");

    let (latest, behind) = match &result {
        Ok((latest, Some(done))) => (Some(latest.clone()), backlog(done, latest)),
        Ok((latest, None)) => (Some(latest.clone()), 0),
        Err(_) => (None, 0),
    };
    json!({
        "dataset": "gdelt",
        "files_loaded": loaded,
        "files_missing_upstream": missing,
        "rows_loaded": rows,
        "rows_pruned": pruned,
        "latest_published": latest,
        "files_still_behind": behind,
        "error": error,
    })
}

/// One events query. Dates default to the last 7 days.
#[derive(Debug, Clone, Default)]
pub struct GdeltQuery {
    pub countries: Vec<String>,
    /// CAMEO root codes or their names ("19", "fight").
    pub event_roots: Vec<String>,
    pub quad_class: Option<u8>,
    pub actor: Option<String>,
    pub since: Option<NaiveDate>,
    pub until: Option<NaiveDate>,
    pub min_sources: Option<i64>,
    pub root_only: bool,
    pub sample: usize,
}

impl GdeltQuery {
    pub fn window(&self, today: NaiveDate) -> (NaiveDate, NaiveDate) {
        let until = self.until.unwrap_or(today);
        (self.since.unwrap_or(until - Duration::days(7)), until)
    }

    /// Root codes, resolving names and unpadded digits; unknown values are
    /// returned separately so the answer can name them.
    pub fn root_codes(&self, cfg: &GdeltConfig) -> (Vec<String>, Vec<String>) {
        let mut codes = Vec::new();
        let mut unknown = Vec::new();
        for raw in &self.event_roots {
            let v = raw.trim().to_lowercase();
            let padded = if v.len() == 1 && v.bytes().all(|b| b.is_ascii_digit()) {
                format!("0{v}")
            } else {
                v.clone()
            };
            if cfg.cameo_root_codes.contains_key(&padded) {
                codes.push(padded);
            } else if let Some((code, _)) = cfg
                .cameo_root_codes
                .iter()
                .find(|(_, n)| n.to_lowercase() == v)
            {
                codes.push(code.clone());
            } else {
                unknown.push(raw.clone());
            }
        }
        (codes, unknown)
    }

    /// `WHERE` clause and its text parameters.
    pub fn filter(&self, cfg: &GdeltConfig, today: NaiveDate) -> (String, Vec<String>) {
        let (since, until) = self.window(today);
        let mut sql = String::from(
            "event_date BETWEEN CAST(? AS DATE) AND CAST(? AS DATE) \
             AND num_sources >= CAST(? AS INTEGER)",
        );
        let mut args = vec![
            since.to_string(),
            until.to_string(),
            self.min_sources
                .unwrap_or(cfg.default_min_sources)
                .max(1)
                .to_string(),
        ];
        let countries: Vec<&String> = self
            .countries
            .iter()
            .filter(|c| !c.trim().is_empty())
            .collect();
        if !countries.is_empty() {
            sql.push_str(&format!(
                " AND lower(country) IN ({})",
                vec!["lower(?)"; countries.len()].join(", ")
            ));
            args.extend(countries.iter().map(|c| c.trim().to_string()));
        }
        let (codes, _) = self.root_codes(cfg);
        if !codes.is_empty() {
            sql.push_str(&format!(
                " AND event_root_code IN ({})",
                vec!["?"; codes.len()].join(", ")
            ));
            args.extend(codes);
        }
        if let Some(q) = self.quad_class {
            sql.push_str(" AND quad_class = CAST(? AS TINYINT)");
            args.push(q.to_string());
        }
        if let Some(a) = self
            .actor
            .as_deref()
            .map(str::trim)
            .filter(|a| !a.is_empty())
        {
            sql.push_str(" AND (actor1_name ILIKE ? OR actor2_name ILIKE ?)");
            let pat = format!("%{a}%");
            args.push(pat.clone());
            args.push(pat);
        }
        if self.root_only {
            sql.push_str(" AND is_root");
        }
        (sql, args)
    }
}

fn grouped(
    store: &DatasetStore,
    key: &str,
    filter: &str,
    args: &[String],
    limit: usize,
) -> Result<Vec<Value>> {
    store.query_json(
        &format!(
            "SELECT {key} AS key, count(*) AS events, count(DISTINCT source_url) AS articles \
             FROM gdelt_events WHERE {filter} AND {key} IS NOT NULL \
             GROUP BY 1 ORDER BY events DESC, key LIMIT {limit}"
        ),
        args,
    )
}

/// Aggregates, source mix, a most-corroborated-first sample, and the caveats.
pub fn run_query(
    store: &DatasetStore,
    cfg: &GdeltConfig,
    classes: &SourceClasses,
    q: &GdeltQuery,
    today: NaiveDate,
) -> Result<Value> {
    let (filter, args) = q.filter(cfg, today);
    let (since, until) = q.window(today);
    let (_, unknown_roots) = q.root_codes(cfg);

    let totals = store.query_json(
        &format!(
            "SELECT count(*) AS events, count(DISTINCT source_url) AS articles, \
             round(avg(goldstein), 2) AS avg_goldstein, round(avg(avg_tone), 2) AS avg_tone \
             FROM gdelt_events WHERE {filter}"
        ),
        &args,
    )?;
    let totals = totals.first().cloned().unwrap_or(json!({}));
    let events = totals["events"].as_i64().unwrap_or(0);

    let name_rows = |rows: Vec<Value>, names: &dyn Fn(&str) -> Option<String>| -> Vec<Value> {
        rows.into_iter()
            .map(|mut r| {
                if let Some(n) = names(r["key"].as_str().unwrap_or("")) {
                    r["name"] = json!(n);
                }
                r
            })
            .collect()
    };
    let by_event = name_rows(
        grouped(store, "event_root_code", &filter, &args, 20)?,
        &|k| cfg.cameo_root_codes.get(k).cloned(),
    );
    let by_quad = name_rows(
        grouped(store, "CAST(quad_class AS VARCHAR)", &filter, &args, 4)?,
        &|k| {
            k.parse::<u8>()
                .ok()
                .and_then(|n| cfg.quad_classes.get(&n).cloned())
        },
    );
    let by_country = grouped(store, "country", &filter, &args, 15)?;
    let by_day = store.query_json(
        &format!(
            "SELECT CAST(event_date AS VARCHAR) AS day, count(*) AS events \
             FROM gdelt_events WHERE {filter} GROUP BY 1 ORDER BY 1"
        ),
        &args,
    )?;
    let mut actor_args = args.clone();
    actor_args.extend(args.iter().cloned());
    let top_actors = store.query_json(
        &format!(
            "SELECT a AS key, count(*) AS events FROM ( \
               SELECT actor1_name AS a FROM gdelt_events WHERE {filter} \
               UNION ALL SELECT actor2_name AS a FROM gdelt_events WHERE {filter}) \
             WHERE a IS NOT NULL GROUP BY 1 ORDER BY 2 DESC, 1 LIMIT 12"
        ),
        &actor_args,
    )?;
    let hosts = grouped(store, "source_host", &filter, &args, 500)?;
    let source_mix = classes.mix(&hosts, 15);

    let mut sample_args = args.clone();
    sample_args.push(q.sample.to_string());
    let sample: Vec<Value> = store
        .query_json(
            &format!(
                "SELECT CAST(event_date AS VARCHAR) AS date, actor1_name, actor2_name, \
                   event_code, event_root_code, quad_class, goldstein, num_sources, num_articles, \
                   geo_fullname AS place, country, source_url, source_host \
                 FROM gdelt_events WHERE {filter} \
                 QUALIFY row_number() OVER (PARTITION BY source_url ORDER BY num_sources DESC, event_id) = 1 \
                 ORDER BY num_sources DESC, event_date DESC LIMIT CAST(? AS INTEGER)"
            ),
            &sample_args,
        )?
        .into_iter()
        .map(|mut r| {
            let root = text(&r["event_root_code"]);
            if let Some(n) = cfg.cameo_root_codes.get(&root) {
                r["event"] = json!(n);
            }
            r["source_class"] = json!(classes.classify(r["source_host"].as_str().unwrap_or("")));
            r
        })
        .collect();

    let coverage = store
        .query_json(
            "SELECT min(file) AS first_file, max(file) AS last_file, count(*) AS files \
             FROM etl_files WHERE dataset = 'gdelt' AND status = 'loaded'",
            &[],
        )?
        .first()
        .cloned()
        .unwrap_or(json!({}));

    let mut notes: Vec<String> = vec![
        "GDELT is machine-coded from news text: counts measure news COVERAGE of an event \
         type, not how often it happened. Single-source events are often miscoded; places \
         can be mis-geocoded (Gaza is often coded as Israel). Check a sample source before \
         relying on a number."
            .into(),
    ];
    let min_sources = q.min_sources.unwrap_or(cfg.default_min_sources).max(1);
    if min_sources > 1 {
        notes.push(format!(
            "Only events reported by >= {min_sources} outlets are counted (min_sources)."
        ));
    }
    if let Some(first) = coverage["first_file"].as_str()
        && let Some(first_day) = parse_stamp(first).map(|t| t.date())
        && since < first_day
    {
        notes.push(format!(
            "Loaded coverage starts {first_day}; the window asked from {since}, so earlier days are missing, not quiet."
        ));
    }
    if coverage["files"].as_i64().unwrap_or(0) == 0 {
        notes.push("No GDELT files are loaded yet: run dataset_sync(dataset: \"gdelt\").".into());
    }
    if !unknown_roots.is_empty() {
        notes.push(format!(
            "Unrecognised event_root values {unknown_roots:?} were ignored. Use a code 01-20 or a name: {}.",
            cfg.cameo_root_codes.values().cloned().collect::<Vec<_>>().join(", ")
        ));
    }
    if events == 0 && !q.countries.is_empty() {
        let mut close = Vec::new();
        for c in &q.countries {
            for r in store.query_json(
                "SELECT DISTINCT country FROM gdelt_events WHERE country ILIKE ? LIMIT 5",
                &[format!("%{}%", c.trim())],
            )? {
                close.push(text(&r["country"]));
            }
        }
        notes.push(if close.is_empty() {
            "No events matched. GDELT country names are English (\"Russia\", \"United States\"); no loaded country resembles the one asked for.".into()
        } else {
            format!("No events matched. Loaded country names resembling the request: {close:?}.")
        });
    }

    Ok(json!({
        "window": {"since": since.to_string(), "until": until.to_string()},
        "totals": totals,
        "by_quad_class": by_quad,
        "by_event_type": by_event,
        "by_country": by_country,
        "by_day": by_day,
        "top_actors": top_actors,
        "source_mix": source_mix,
        "sample": sample,
        "coverage": coverage,
        "notes": notes,
        "attribution": "Data: The GDELT Project (gdeltproject.org)",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> GdeltConfig {
        serde_yaml::from_str(
            r#"
lastupdate_url: x
file_url_template: "https://d/{stamp}.export.CSV.zip"
retention_days: 90
initial_backfill_days: 7
max_files_per_run: 48
default_min_sources: 2
quad_classes: {1: verbal cooperation, 4: material conflict}
cameo_root_codes: {"14": protest, "18": assault, "19": fight}
"#,
        )
        .unwrap()
    }

    #[test]
    fn the_stamp_comes_from_the_export_url() {
        assert_eq!(
            stamp_in("http://data.gdeltproject.org/gdeltv2/20261006210000.export.CSV.zip")
                .as_deref(),
            Some("20261006210000")
        );
        assert_eq!(stamp_in("http://x/lastupdate.txt"), None);
    }

    #[test]
    fn pending_stamps_resume_after_the_last_file_and_respect_the_cap() {
        let p = pending_stamps(Some("20261006200000"), "20261006210000", 7, 48);
        assert_eq!(
            p,
            vec![
                "20261006201500",
                "20261006203000",
                "20261006204500",
                "20261006210000"
            ]
        );
        assert_eq!(
            pending_stamps(Some("20261006200000"), "20261006210000", 7, 2).len(),
            2
        );
        assert!(pending_stamps(Some("20261006210000"), "20261006210000", 7, 48).is_empty());
    }

    #[test]
    fn a_first_run_backfills_from_the_configured_days() {
        let p = pending_stamps(None, "20261006210000", 1, 1000);
        assert_eq!(p.first().map(String::as_str), Some("20261005210000"));
        assert_eq!(p.len(), 97, "24h of 15-minute slots, both ends included");
    }

    #[test]
    fn backlog_counts_remaining_slots() {
        assert_eq!(backlog("20261006200000", "20261006210000"), 4);
        assert_eq!(backlog("20261006210000", "20261006210000"), 0);
    }

    #[test]
    fn root_codes_accept_names_and_unpadded_digits_and_report_unknowns() {
        let q = GdeltQuery {
            event_roots: vec!["Fight".into(), "14".into(), "9".into(), "riot".into()],
            ..Default::default()
        };
        let (codes, unknown) = q.root_codes(&cfg());
        assert_eq!(codes, vec!["19", "14"]);
        assert_eq!(unknown, vec!["9", "riot"], "09 is not in this test config");
    }

    #[test]
    fn the_filter_binds_every_value_and_defaults_min_sources() {
        let q = GdeltQuery {
            countries: vec!["Ukraine".into(), " ".into()],
            event_roots: vec!["fight".into()],
            actor: Some("Wagner".into()),
            quad_class: Some(4),
            root_only: true,
            ..Default::default()
        };
        let today = NaiveDate::from_ymd_opt(2026, 10, 6).unwrap();
        let (sql, args) = q.filter(&cfg(), today);
        assert_eq!(sql.matches('?').count(), args.len());
        assert_eq!(
            args[..3],
            ["2026-09-29".to_string(), "2026-10-06".into(), "2".into()]
        );
        assert!(args.contains(&"Ukraine".to_string()) && args.contains(&"19".to_string()));
        assert!(args.contains(&"%Wagner%".to_string()));
        assert!(sql.contains("is_root"));
    }

    #[test]
    fn a_zip_with_one_file_unzips() {
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut w = zip::ZipWriter::new(&mut buf);
            w.start_file::<_, ()>("x.CSV", zip::write::FileOptions::default())
                .unwrap();
            std::io::Write::write_all(&mut w, b"a\tb\n").unwrap();
            w.finish().unwrap();
        }
        assert_eq!(unzip_single(buf.get_ref()).unwrap(), b"a\tb\n");
        assert!(unzip_single(b"not a zip").is_err());
    }
}
