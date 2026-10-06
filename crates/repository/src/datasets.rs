//! DuckDB store for external datasets the brain loads by ETL (GDELT events,
//! ReliefWeb reports).
//!
//! Its own file (`DATASETS_DB_PATH`), never the telemetry database: bulk loads
//! of ~86k GDELT rows a day must not be able to take telemetry down with them
//! when a killed container leaves a WAL tail that fails replay.
//!
//! Every load is one transaction that also records the file or batch it came
//! from, so a run that dies midway leaves nothing half-loaded and the next run
//! resumes from the last committed file.

use anyhow::{Context, Result};
use duckdb::{Connection, ToSql, params};
use serde_json::Value;
use std::path::Path;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct DatasetStore {
    conn: Arc<Mutex<Connection>>,
}

/// One ReliefWeb report, flattened for storage. Multi-valued fields are
/// joined with `"; "`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReliefwebRow {
    pub id: i64,
    pub title: String,
    pub url: String,
    pub date_created: String,
    pub date_original: Option<String>,
    pub sources: String,
    pub source_types: String,
    pub primary_country: String,
    pub countries: String,
    pub disasters: String,
    pub disaster_types: String,
    pub themes: String,
    pub formats: String,
    pub languages: String,
    pub body_excerpt: String,
}

/// GDELT 2.0 export columns are tab-separated and positional (61 of them).
/// Read every column as text and transform in SQL; `TRY_*` keeps a malformed
/// value from failing the whole file.
fn gdelt_columns() -> String {
    (0..61)
        .map(|i| format!("'c{i}': 'VARCHAR'"))
        .collect::<Vec<_>>()
        .join(", ")
}

impl DatasetStore {
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self> {
        let conn = Connection::open(path).context("Failed to open datasets DuckDB file")?;
        // Containers are killed, not stopped; checkpoint on open so a WAL from
        // a previous process never accumulates (same rule as telemetry).
        conn.execute_batch("CHECKPOINT;")?;
        let store = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        store.init_schema()?;
        Ok(store)
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|_| anyhow::anyhow!("datasets DuckDB lock poisoned"))
    }

    fn init_schema(&self) -> Result<()> {
        self.lock()?.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS etl_files (
                dataset   TEXT NOT NULL,
                file      TEXT NOT NULL,
                status    TEXT NOT NULL,          -- loaded | missing
                rows      INTEGER,
                loaded_at TIMESTAMPTZ DEFAULT current_timestamp,
                PRIMARY KEY (dataset, file)
            );

            CREATE TABLE IF NOT EXISTS etl_runs (
                id          TEXT PRIMARY KEY,
                dataset     TEXT NOT NULL,
                started_at  TIMESTAMP,              -- UTC, set from Rust (no ICU)
                finished_at TIMESTAMP,
                api_calls   INTEGER,
                files       INTEGER,
                rows        INTEGER,
                pruned      INTEGER,
                error       TEXT
            );

            CREATE TABLE IF NOT EXISTS gdelt_events (
                event_id         BIGINT,
                event_date       DATE,
                added_at         TIMESTAMP,
                file             TEXT,
                actor1_code      TEXT,
                actor1_name      TEXT,
                actor1_country   TEXT,
                actor2_code      TEXT,
                actor2_name      TEXT,
                actor2_country   TEXT,
                is_root          BOOLEAN,
                event_code       TEXT,
                event_base_code  TEXT,
                event_root_code  TEXT,
                quad_class       TINYINT,
                goldstein        DOUBLE,
                num_mentions     INTEGER,
                num_sources      INTEGER,
                num_articles     INTEGER,
                avg_tone         DOUBLE,
                geo_type         TINYINT,
                geo_fullname     TEXT,
                geo_country_fips TEXT,
                geo_adm1         TEXT,
                geo_lat          DOUBLE,
                geo_lon          DOUBLE,
                country          TEXT,
                source_url       TEXT,
                source_host      TEXT
            );

            CREATE TABLE IF NOT EXISTS reliefweb_reports (
                id              BIGINT PRIMARY KEY,
                title           TEXT,
                url             TEXT,
                date_created    TIMESTAMP,          -- UTC; the bundled DuckDB has no ICU,
                date_original   TIMESTAMP,          -- so TIMESTAMPTZ cannot even cast to DATE
                sources         TEXT,
                source_types    TEXT,
                primary_country TEXT,
                countries       TEXT,
                disasters       TEXT,
                disaster_types  TEXT,
                themes          TEXT,
                formats         TEXT,
                languages       TEXT,
                body_excerpt    TEXT,
                loaded_at       TIMESTAMPTZ DEFAULT current_timestamp
            );
            ",
        )?;
        Ok(())
    }

    /// The newest file recorded for a dataset (loaded or missing): the resume
    /// point for the next run.
    pub fn last_file(&self, dataset: &str) -> Result<Option<String>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare("SELECT max(file) FROM etl_files WHERE dataset = ?")?;
        let v: Option<String> = stmt.query_row(params![dataset], |r| r.get(0))?;
        Ok(v)
    }

    /// Record a file that will never load (absent upstream), so the resume
    /// point moves past it.
    pub fn mark_missing(&self, dataset: &str, file: &str) -> Result<()> {
        self.lock()?.execute(
            "INSERT OR REPLACE INTO etl_files (dataset, file, status, rows) VALUES (?, ?, 'missing', 0)",
            params![dataset, file],
        )?;
        Ok(())
    }

    /// Load one unzipped GDELT 2.0 export file and record it, atomically.
    /// Returns the rows inserted.
    pub fn load_gdelt_file(&self, stamp: &str, csv_path: &Path) -> Result<usize> {
        let path = csv_path.to_string_lossy().replace('\'', "''");
        let stamp_sql = stamp.replace('\'', "''");
        let sql = format!(
            "INSERT INTO gdelt_events SELECT
                TRY_CAST(c0 AS BIGINT),
                CAST(try_strptime(c1, '%Y%m%d') AS DATE),
                try_strptime(c59, '%Y%m%d%H%M%S'),
                '{stamp_sql}',
                NULLIF(c5, ''), NULLIF(c6, ''), NULLIF(c7, ''),
                NULLIF(c15, ''), NULLIF(c16, ''), NULLIF(c17, ''),
                c25 = '1',
                NULLIF(c26, ''), NULLIF(c27, ''), NULLIF(c28, ''),
                TRY_CAST(c29 AS TINYINT), TRY_CAST(c30 AS DOUBLE),
                TRY_CAST(c31 AS INTEGER), TRY_CAST(c32 AS INTEGER), TRY_CAST(c33 AS INTEGER),
                TRY_CAST(c34 AS DOUBLE),
                TRY_CAST(c51 AS TINYINT), NULLIF(c52, ''), NULLIF(c53, ''), NULLIF(c54, ''),
                TRY_CAST(c56 AS DOUBLE), TRY_CAST(c57 AS DOUBLE),
                NULLIF(trim(list_last(string_split(c52, ','))), ''),
                NULLIF(c60, ''),
                NULLIF(lower(regexp_extract(c60, '^[a-zA-Z]+://(?:www\\.)?([^/:?#]+)', 1)), '')
             FROM read_csv('{path}', delim = '\t', header = false, quote = '', escape = '',
                           auto_detect = false, columns = {{{cols}}}, ignore_errors = true)",
            cols = gdelt_columns()
        );
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        let rows = tx.execute(&sql, [])?;
        tx.execute(
            "INSERT OR REPLACE INTO etl_files (dataset, file, status, rows) VALUES ('gdelt', ?, 'loaded', ?)",
            params![stamp, rows as i64],
        )?;
        tx.commit()?;
        Ok(rows)
    }

    /// Insert or replace ReliefWeb reports, atomically. Returns rows written.
    pub fn upsert_reliefweb(&self, rows: &[ReliefwebRow]) -> Result<usize> {
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT OR REPLACE INTO reliefweb_reports
                 (id, title, url, date_created, date_original, sources, source_types,
                  primary_country, countries, disasters, disaster_types, themes, formats,
                  languages, body_excerpt)
                 VALUES (?, ?, ?, CAST(? AS TIMESTAMP), CAST(? AS TIMESTAMP), ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )?;
            for r in rows {
                stmt.execute(params![
                    r.id,
                    r.title,
                    r.url,
                    r.date_created,
                    r.date_original,
                    r.sources,
                    r.source_types,
                    r.primary_country,
                    r.countries,
                    r.disasters,
                    r.disaster_types,
                    r.themes,
                    r.formats,
                    r.languages,
                    r.body_excerpt,
                ])?;
            }
        }
        tx.commit()?;
        Ok(rows.len())
    }

    /// Newest ReliefWeb `date_created` held, as ISO 8601 text.
    pub fn reliefweb_newest(&self) -> Result<Option<String>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT strftime(max(date_created), '%Y-%m-%dT%H:%M:%S+00:00') FROM reliefweb_reports",
        )?;
        let v: Option<String> = stmt.query_row([], |r| r.get(0))?;
        Ok(v)
    }

    /// API calls a dataset made since 00:00 UTC — the daily-quota meter.
    pub fn api_calls_today(&self, dataset: &str) -> Result<i64> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT COALESCE(sum(api_calls), 0) FROM etl_runs
             WHERE dataset = ? AND finished_at >= CAST(? AS TIMESTAMP)",
        )?;
        let midnight = chrono::Utc::now().format("%Y-%m-%d 00:00:00").to_string();
        let v: i64 = stmt.query_row(params![dataset, midnight], |r| r.get(0))?;
        Ok(v)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_run(
        &self,
        dataset: &str,
        started_at: &str,
        api_calls: i64,
        files: i64,
        rows: i64,
        pruned: i64,
        error: Option<&str>,
    ) -> Result<()> {
        self.lock()?.execute(
            "INSERT INTO etl_runs (id, dataset, started_at, finished_at, api_calls, files, rows, pruned, error)
             VALUES (?, ?, CAST(? AS TIMESTAMP), CAST(? AS TIMESTAMP), ?, ?, ?, ?, ?)",
            params![
                uuid::Uuid::new_v4().to_string(),
                dataset,
                started_at,
                chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string(),
                api_calls,
                files,
                rows,
                pruned,
                error
            ],
        )?;
        Ok(())
    }

    /// Delete GDELT events older than `days` and the file records behind
    /// them (keeping the newest file record, the resume point). Returns the
    /// event rows deleted.
    pub fn prune_gdelt(&self, days: i64) -> Result<usize> {
        let conn = self.lock()?;
        let n = conn.execute(
            "DELETE FROM gdelt_events WHERE event_date < current_date - CAST(? AS INTEGER)",
            params![days],
        )?;
        conn.execute(
            "DELETE FROM etl_files WHERE dataset = 'gdelt' AND file < ?
               AND file <> (SELECT max(file) FROM etl_files WHERE dataset = 'gdelt')",
            params![
                (chrono::Utc::now() - chrono::Duration::days(days))
                    .format("%Y%m%d%H%M%S")
                    .to_string()
            ],
        )?;
        Ok(n)
    }

    /// Delete ReliefWeb reports created more than `days` ago. The cutoff is
    /// computed here, not with `now()`, which is TIMESTAMPTZ and needs ICU.
    pub fn prune_reliefweb(&self, days: i64) -> Result<usize> {
        let cutoff = (chrono::Utc::now() - chrono::Duration::days(days))
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        let n = self.lock()?.execute(
            "DELETE FROM reliefweb_reports WHERE date_created < CAST(? AS TIMESTAMP)",
            params![cutoff],
        )?;
        Ok(n)
    }

    /// Bound the WAL after a bulk load.
    pub fn checkpoint(&self) -> Result<()> {
        self.lock()?.execute_batch("CHECKPOINT;")?;
        Ok(())
    }

    /// Run a read query with text parameters and return rows as JSON objects.
    ///
    /// For queries the app builds itself (never caller SQL). Select dates as
    /// text: the row conversion below handles numbers, text, booleans and NULL.
    pub fn query_json(&self, sql: &str, args: &[String]) -> Result<Vec<Value>> {
        use duckdb::types::ValueRef;
        let conn = self.lock()?;
        let mut stmt = conn.prepare(sql)?;
        let bound: Vec<&dyn ToSql> = args.iter().map(|a| a as &dyn ToSql).collect();
        // duckdb 1.4: column names exist only after execution (see
        // TelemetryClient::query_raw), so collect values first, names after.
        let raw: Vec<Vec<Value>> = stmt
            .query_map(bound.as_slice(), |row| {
                let mut vals = Vec::new();
                while let Ok(v) = row.get_ref(vals.len()) {
                    vals.push(match v {
                        ValueRef::Null => Value::Null,
                        ValueRef::Boolean(b) => Value::Bool(b),
                        ValueRef::TinyInt(n) => n.into(),
                        ValueRef::SmallInt(n) => n.into(),
                        ValueRef::Int(n) => n.into(),
                        ValueRef::BigInt(n) => n.into(),
                        ValueRef::HugeInt(n) => Value::String(n.to_string()),
                        ValueRef::UTinyInt(n) => n.into(),
                        ValueRef::USmallInt(n) => n.into(),
                        ValueRef::UInt(n) => n.into(),
                        ValueRef::UBigInt(n) => n.into(),
                        ValueRef::Float(f) => serde_json::json!(f),
                        ValueRef::Double(f) => serde_json::json!(f),
                        ValueRef::Decimal(d) => Value::String(d.to_string()),
                        ValueRef::Text(t) => Value::String(String::from_utf8_lossy(t).into_owned()),
                        _ => Value::String("(unsupported type — cast it to text)".into()),
                    });
                }
                Ok(vals)
            })?
            .collect::<std::result::Result<_, _>>()?;
        let names: Vec<String> = stmt.column_names();
        Ok(raw
            .into_iter()
            .map(|vals| {
                Value::Object(
                    names
                        .iter()
                        .cloned()
                        .zip(vals)
                        .collect::<serde_json::Map<String, Value>>(),
                )
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, DatasetStore) {
        let dir = tempfile::tempdir().unwrap();
        let s = DatasetStore::new(dir.path().join("d.duckdb")).unwrap();
        (dir, s)
    }

    /// A GDELT export row with the columns the load reads filled in.
    fn gdelt_line(
        id: i64,
        date: &str,
        fullname: &str,
        root: &str,
        sources: i32,
        url: &str,
    ) -> String {
        let mut c = vec![String::new(); 61];
        c[0] = id.to_string();
        c[1] = date.replace('-', "");
        c[6] = "RUSSIA".into();
        c[16] = "UKRAINE".into();
        c[25] = "1".into();
        c[26] = format!("{root}0");
        c[27] = format!("{root}0");
        c[28] = root.into();
        c[29] = "4".into();
        c[30] = "-10.0".into();
        c[31] = "5".into();
        c[32] = sources.to_string();
        c[33] = "5".into();
        c[34] = "-3.2".into();
        c[51] = "4".into();
        c[52] = fullname.into();
        c[53] = "UP".into();
        c[56] = "50.45".into();
        c[57] = "30.52".into();
        c[59] = format!("{}120000", date.replace('-', ""));
        c[60] = url.into();
        c.join("\t")
    }

    #[test]
    fn a_gdelt_file_loads_with_derived_country_and_host_and_records_itself() {
        let (dir, s) = store();
        let today = chrono::Utc::now().date_naive().to_string();
        let csv = dir.path().join("x.CSV");
        std::fs::write(
            &csv,
            [
                gdelt_line(
                    1,
                    &today,
                    "Kyiv, Kyyiv, Misto, Ukraine",
                    "19",
                    3,
                    "https://www.bbc.co.uk/news/a",
                ),
                gdelt_line(
                    2,
                    &today,
                    "Ukraine",
                    "18",
                    1,
                    "http://radaronline.com/x?y=1",
                ),
                "garbage line without tabs".to_string(),
            ]
            .join("\n"),
        )
        .unwrap();
        let n = s.load_gdelt_file("20261006120000", &csv).unwrap();
        assert_eq!(
            n, 2,
            "the malformed row is skipped rather than failing the file"
        );
        assert_eq!(
            s.last_file("gdelt").unwrap().as_deref(),
            Some("20261006120000")
        );
        let rows = s
            .query_json(
                "SELECT event_id, CAST(event_date AS VARCHAR) AS d, country, source_host, event_root_code, num_sources, is_root
                 FROM gdelt_events WHERE event_id IS NOT NULL ORDER BY event_id",
                &[],
            )
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["country"], "Ukraine");
        assert_eq!(rows[0]["source_host"], "bbc.co.uk");
        assert_eq!(rows[0]["d"], today);
        assert_eq!(rows[0]["is_root"], true);
        assert_eq!(
            rows[1]["country"], "Ukraine",
            "a country-level place is its own country"
        );
        assert_eq!(rows[1]["source_host"], "radaronline.com");
    }

    #[test]
    fn missing_files_advance_the_resume_point() {
        let (_d, s) = store();
        assert_eq!(s.last_file("gdelt").unwrap(), None);
        s.mark_missing("gdelt", "20261006121500").unwrap();
        assert_eq!(
            s.last_file("gdelt").unwrap().as_deref(),
            Some("20261006121500")
        );
    }

    #[test]
    fn reliefweb_upserts_replace_by_id_and_report_the_newest_date() {
        let (_d, s) = store();
        let mut r = ReliefwebRow {
            id: 42,
            title: "Sudan: Flash Update".into(),
            date_created: "2026-10-05T08:00:00+00:00".into(),
            primary_country: "Sudan".into(),
            ..Default::default()
        };
        s.upsert_reliefweb(std::slice::from_ref(&r)).unwrap();
        r.title = "Sudan: Flash Update No. 2".into();
        r.date_created = "2026-10-06T09:30:00+00:00".into();
        s.upsert_reliefweb(&[r]).unwrap();
        let rows = s
            .query_json(
                "SELECT count(*) AS n, max(title) AS t FROM reliefweb_reports",
                &[],
            )
            .unwrap();
        assert_eq!(rows[0]["n"], 1);
        assert_eq!(rows[0]["t"], "Sudan: Flash Update No. 2");
        assert_eq!(
            s.reliefweb_newest().unwrap().as_deref(),
            Some("2026-10-06T09:30:00+00:00")
        );
    }

    #[test]
    fn runs_are_logged_and_count_toward_the_daily_quota() {
        let (_d, s) = store();
        let now = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
        s.record_run("reliefweb", &now, 3, 0, 120, 0, None).unwrap();
        s.record_run("reliefweb", &now, 2, 0, 0, 0, Some("HTTP 500"))
            .unwrap();
        s.record_run("gdelt", &now, 9, 9, 8000, 0, None).unwrap();
        assert_eq!(s.api_calls_today("reliefweb").unwrap(), 5);
    }

    #[test]
    fn query_parameters_bind_as_text() {
        let (_d, s) = store();
        let rows = s
            .query_json("SELECT ? || '-' || ? AS joined", &["a".into(), "b".into()])
            .unwrap();
        assert_eq!(rows[0]["joined"], "a-b");
    }
}
