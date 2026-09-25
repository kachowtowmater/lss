//! SQLite storage: samples, incidents, alerts, probes and a small key/value table for the
//! engine + tracker state.

use lss_core::incidents::Incident;
use lss_core::model::{AlertRow, Sample};
use lss_core::probe::ProbeRecord;
use lss_core::rules::AlertEvent;
use lss_core::gatelog::LogDelta;
use lss_core::hist::{decode_bounds, encode_bounds, ClosedWindow, HistAccum, HIST_METRICS};
use lss_core::bench::Scorecard;
use lss_core::loadout::{LoadoutAcc, LoadoutIdentity};
use lss_core::series::{Agg, RollupBatch};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use std::cell::RefCell;
use std::collections::HashMap;

pub struct Db {
    conn: Connection,
    metric_ids: RefCell<HashMap<String, i64>>,
}

/// What each tier keeps, in seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retention {
    pub raw: i64,
    pub m1: i64,
    pub m10: i64,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS samples  (ts INTEGER PRIMARY KEY, json TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS incidents(id INTEGER PRIMARY KEY AUTOINCREMENT, start INTEGER NOT NULL, end INTEGER,
                                     kind TEXT NOT NULL, detail TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS incidents_start ON incidents(start);
CREATE TABLE IF NOT EXISTS alerts   (id INTEGER PRIMARY KEY AUTOINCREMENT, ts INTEGER NOT NULL, rule TEXT NOT NULL,
                                     severity TEXT NOT NULL, message TEXT NOT NULL, recovered INTEGER NOT NULL DEFAULT 0,
                                     delivered INTEGER NOT NULL DEFAULT 0);
CREATE INDEX IF NOT EXISTS alerts_ts ON alerts(ts);
CREATE TABLE IF NOT EXISTS probes   (id INTEGER PRIMARY KEY AUTOINCREMENT, ts INTEGER NOT NULL, status TEXT NOT NULL,
                                     ttft_ms REAL, decode_tok_s REAL, tokens INTEGER, detail TEXT NOT NULL DEFAULT '');
CREATE INDEX IF NOT EXISTS probes_ts ON probes(ts);
CREATE TABLE IF NOT EXISTS kv       (k TEXT PRIMARY KEY, v TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS metric_names(id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE);
CREATE TABLE IF NOT EXISTS rollup   (res INTEGER NOT NULL, metric INTEGER NOT NULL, ts INTEGER NOT NULL,
                                     avg REAL NOT NULL, min REAL NOT NULL, max REAL NOT NULL, n REAL NOT NULL,
                                     PRIMARY KEY(res, metric, ts)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS hist_bounds(id INTEGER PRIMARY KEY, le TEXT NOT NULL UNIQUE);
CREATE TABLE IF NOT EXISTS hist     (res INTEGER NOT NULL, metric TEXT NOT NULL, ts INTEGER NOT NULL, bounds INTEGER NOT NULL,
                                     counts TEXT NOT NULL, sum REAL NOT NULL, PRIMARY KEY(res, metric, ts)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS gatelog  (ts INTEGER PRIMARY KEY, json TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS loadout  (id TEXT PRIMARY KEY, identity TEXT NOT NULL, acc TEXT NOT NULL,
                                     first_seen INTEGER NOT NULL, last_seen INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS bench_runs(id INTEGER PRIMARY KEY AUTOINCREMENT, loadout_id TEXT NOT NULL, profile TEXT NOT NULL,
                                     started_at INTEGER NOT NULL, ended_at INTEGER, status TEXT NOT NULL, scorecard TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS bench_runs_loadout ON bench_runs(loadout_id, started_at);
";

type R<T> = rusqlite::Result<T>;

impl Db {
    pub fn open(path: &str) -> R<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(SCHEMA)?;
        Self::migrate(&conn)?;
        Ok(Self { conn, metric_ids: RefCell::default() })
    }

    /// A second, read-only connection for the HTTP threads: WAL lets them read while the poll
    /// loop writes, so a slow `/series` client never holds the collector up.
    pub fn open_readonly(path: &str) -> R<Self> {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
        conn.busy_timeout(std::time::Duration::from_secs(2))?;
        Ok(Self { conn, metric_ids: RefCell::default() })
    }

    /// Additive migrations for databases created by an older collector.
    fn migrate(conn: &Connection) -> R<()> {
        let has_http_status = conn.prepare("SELECT 1 FROM pragma_table_info('probes') WHERE name = 'http_status'")?.exists([])?;
        if !has_http_status {
            conn.execute_batch("ALTER TABLE probes ADD COLUMN http_status INTEGER;")?;
        }
        let has_valid = conn.prepare("SELECT 1 FROM pragma_table_info('probes') WHERE name = 'valid'")?.exists([])?;
        if !has_valid {
            conn.execute_batch("ALTER TABLE probes ADD COLUMN valid INTEGER NOT NULL DEFAULT 1; ALTER TABLE probes ADD COLUMN invalid_reason TEXT;")?;
        }
        // D1, 2026-09-20: a valid-but-not-proved-alone reading (no gateway, an engine that does
        // not report its own load) used to lose the caveat the moment it round-tripped the
        // database: an older row (or one from before this column) is proved, never unproved.
        let has_unverified = conn.prepare("SELECT 1 FROM pragma_table_info('probes') WHERE name = 'unverified'")?.exists([])?;
        if !has_unverified {
            conn.execute_batch("ALTER TABLE probes ADD COLUMN unverified INTEGER NOT NULL DEFAULT 0;")?;
        }
        let has_key = conn.prepare("SELECT 1 FROM pragma_table_info('incidents') WHERE name = 'dedupe_key'")?.exists([])?;
        if !has_key {
            conn.execute_batch("ALTER TABLE incidents ADD COLUMN dedupe_key TEXT;")?;
        }
        let merged = Self::merge_duplicate_xids(conn)?;
        if merged > 0 {
            eprintln!("incidents: merged {merged} duplicate Xid incident(s)");
        }
        // An interim build kept one `loadouts` row per container START; a loadout is now one row
        // per CONFIGURATION (table `loadout`). Those rows were built from the stored samples and
        // are rebuilt the same way, so nothing measured is lost by dropping the old table.
        conn.execute_batch("DROP TABLE IF EXISTS loadouts;")?;
        // One-off: accumulators written before reading speed stopped counting cache hits are
        // rebuilt (the collector folds the stored samples in again the moment it sees the serve).
        let rebuilt = conn.prepare("SELECT 1 FROM kv WHERE k = 'loadout_acc_v2'")?.exists([])?;
        if !rebuilt {
            conn.execute_batch("DELETE FROM loadout WHERE id NOT IN (SELECT DISTINCT loadout_id FROM bench_runs); INSERT OR REPLACE INTO kv(k, v) VALUES ('loadout_acc_v2', '1');")?;
        }
        // from here on the database itself refuses a second booking of the same event
        conn.execute_batch("CREATE UNIQUE INDEX IF NOT EXISTS incidents_dedupe ON incidents(dedupe_key) WHERE dedupe_key IS NOT NULL;")?;
        Ok(())
    }

    /// Judges rows written before probe validation existed by what they DO record: a probe
    /// whose first token took longer than the limit was not alone, and a probe that never
    /// completed is not a reading. (Contention that left no trace in the TTFT cannot be seen
    /// after the fact.) Idempotent; returns how many rows changed.
    pub fn invalidate_legacy_probes(&self, max_ttft_ms: f64) -> R<usize> {
        let mut n = self.conn.execute(
            "UPDATE probes SET valid = 0, invalid_reason = 'slow_ttft' WHERE valid = 1 AND status = 'ok' AND ttft_ms > ?1",
            [max_ttft_ms],
        )?;
        n += self.conn.execute("UPDATE probes SET valid = 0, invalid_reason = 'busy_before' WHERE valid = 1 AND status = 'skipped_busy'", [])?;
        n += self.conn.execute("UPDATE probes SET valid = 0, invalid_reason = status WHERE valid = 1 AND status NOT IN ('ok', 'skipped_busy')", [])?;
        Ok(n)
    }

    /// The newest valid completed probe, however old: the C1 reading that stays on screen and
    /// in /metrics through any run of skipped or contaminated probes.
    pub fn last_valid_probe(&self) -> R<Option<ProbeRecord>> {
        self.conn.query_row(&format!("SELECT {PROBE_COLS} FROM probes WHERE valid = 1 AND status = 'ok' ORDER BY ts DESC, id DESC LIMIT 1"), [], probe_row).optional()
    }

    /// Decode rates of the `n` most recent valid probes, OLDEST first.
    /// card #261: TTFT of the `n` most recent valid probes (any order - a median is taken).
    pub fn recent_valid_ttft_ms(&self, n: usize) -> R<Vec<f64>> {
        let mut st = self.conn.prepare("SELECT ttft_ms FROM probes WHERE valid = 1 AND status = 'ok' AND ttft_ms IS NOT NULL ORDER BY ts DESC, id DESC LIMIT ?1")?;
        let v: Vec<f64> = st.query_map([n as i64], |r| r.get(0))?.filter_map(Result::ok).collect();
        Ok(v)
    }

    /// card #261: is the probe stored at `ts` (still) a valid reading?
    pub fn probe_is_valid(&self, ts: i64) -> R<bool> {
        self.conn.query_row("SELECT EXISTS(SELECT 1 FROM probes WHERE ts = ?1 AND valid = 1 AND status = 'ok')", [ts], |r| r.get(0))
    }

    pub fn recent_valid_tok_s(&self, n: usize) -> R<Vec<f64>> {
        let mut st = self.conn.prepare("SELECT decode_tok_s FROM probes WHERE valid = 1 AND status = 'ok' AND decode_tok_s IS NOT NULL ORDER BY ts DESC, id DESC LIMIT ?1")?;
        let mut v: Vec<f64> = st.query_map([n as i64], |r| r.get(0))?.filter_map(Result::ok).collect();
        v.reverse();
        Ok(v)
    }

    #[cfg(test)]
    pub fn memory() -> Self {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        Self::migrate(&conn).unwrap();
        Self { conn, metric_ids: RefCell::default() }
    }

    pub fn insert_sample(&self, s: &Sample) -> R<()> {
        let json = serde_json::to_string(s).unwrap_or_else(|_| "{}".into());
        self.conn.execute("INSERT OR REPLACE INTO samples(ts, json) VALUES (?1, ?2)", params![s.ts, json])?;
        Ok(())
    }

    pub fn samples_since(&self, ts: i64) -> R<Vec<Sample>> {
        let mut st = self.conn.prepare("SELECT json FROM samples WHERE ts >= ?1 ORDER BY ts")?;
        let rows = st.query_map([ts], |r| r.get::<_, String>(0))?;
        Ok(rows.filter_map(Result::ok).filter_map(|j| serde_json::from_str(&j).ok()).collect())
    }

    pub fn open_incident(&self, kind: &str, start: i64, detail: &str) -> R<()> {
        self.conn.execute("INSERT INTO incidents(start, end, kind, detail) VALUES (?1, NULL, ?2, ?3)", params![start, kind, detail])?;
        Ok(())
    }

    /// Closes every open incident of `kind` (there is at most one by construction).
    pub fn close_incident(&self, kind: &str, end: i64) -> R<usize> {
        self.conn.execute("UPDATE incidents SET end = ?1 WHERE kind = ?2 AND end IS NULL", params![end, kind])
    }

    /// Point incident. Idempotent: on `key` when there is one (an Xid is the same Xid however
    /// its description is worded by this or a later version), else on (kind, start, detail). So
    /// the Xid back-fill and an overlapping journal window can never double-book.
    pub fn event_incident(&self, kind: &str, ts: i64, detail: &str, key: Option<&str>) -> R<bool> {
        let exists: Option<i64> = match key {
            Some(k) => self.conn.query_row("SELECT id FROM incidents WHERE dedupe_key = ?1", [k], |r| r.get(0)).optional()?,
            None => self.conn.query_row("SELECT id FROM incidents WHERE kind = ?1 AND start = ?2 AND detail = ?3", params![kind, ts, detail], |r| r.get(0)).optional()?,
        };
        if exists.is_some() {
            return Ok(false);
        }
        self.conn.execute("INSERT INTO incidents(start, end, kind, detail, dedupe_key) VALUES (?1, ?1, ?2, ?3, ?4)", params![ts, kind, detail, key])?;
        Ok(true)
    }

    /// One-off, idempotent: gives every stored Xid incident its dedupe key and merges the rows
    /// that turn out to be the same event booked twice under two wordings (the oldest row stays,
    /// with the newest wording). Returns how many duplicates were removed.
    fn merge_duplicate_xids(conn: &Connection) -> R<usize> {
        let unkeyed: Vec<(i64, i64, String)> = {
            let mut st = conn.prepare("SELECT id, start, detail FROM incidents WHERE kind = 'xid' AND dedupe_key IS NULL ORDER BY id")?;
            let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            rows.filter_map(Result::ok).collect()
        };
        let mut removed = 0;
        for (id, start, detail) in unkeyed {
            let Some(key) = lss_core::incidents::xid_key_from_detail(&detail, start) else { continue };
            let keeper: Option<i64> = conn.query_row("SELECT id FROM incidents WHERE dedupe_key = ?1", [&key], |r| r.get(0)).optional()?;
            match keeper {
                Some(first) => {
                    conn.execute("UPDATE incidents SET detail = ?1 WHERE id = ?2", params![detail, first])?;
                    conn.execute("DELETE FROM incidents WHERE id = ?1", [id])?;
                    removed += 1;
                }
                None => {
                    conn.execute("UPDATE incidents SET dedupe_key = ?1 WHERE id = ?2", params![key, id])?;
                }
            }
        }
        Ok(removed)
    }

    /// Newest first. Includes anything that overlaps the window (still open, or ended inside it).
    pub fn incidents_since(&self, ts: i64) -> R<Vec<Incident>> {
        let mut st = self.conn.prepare(
            "SELECT id, start, end, kind, detail FROM incidents WHERE start >= ?1 OR end IS NULL OR end >= ?1 ORDER BY start DESC, id DESC LIMIT 500",
        )?;
        let rows = st.query_map([ts], |r| Ok(Incident { id: r.get(0)?, start: r.get(1)?, end: r.get(2)?, kind: r.get(3)?, detail: r.get(4)? }))?;
        Ok(rows.filter_map(Result::ok).collect())
    }

    pub fn count_restarts_since(&self, ts: i64, excluding_prefix: &str) -> R<u32> {
        self.conn.query_row(
            "SELECT COUNT(*) FROM incidents WHERE kind = 'container_restart' AND start >= ?1 AND detail NOT LIKE ?2",
            params![ts, format!("{excluding_prefix} %")],
            |r| r.get(0),
        )
    }

    pub fn insert_alert(&self, a: &AlertEvent) -> R<i64> {
        self.conn.execute(
            "INSERT INTO alerts(ts, rule, severity, message, recovered, delivered) VALUES (?1, ?2, ?3, ?4, ?5, 0)",
            params![a.ts, a.rule, a.severity.as_str(), a.message, a.recovered],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn mark_delivered(&self, id: i64) -> R<()> {
        self.conn.execute("UPDATE alerts SET delivered = 1 WHERE id = ?1", [id])?;
        Ok(())
    }

    pub fn recent_alerts(&self, limit: u32) -> R<Vec<AlertRow>> {
        let mut st = self.conn.prepare("SELECT id, ts, rule, severity, message, recovered, delivered FROM alerts ORDER BY ts DESC, id DESC LIMIT ?1")?;
        let rows = st.query_map([limit], |r| {
            Ok(AlertRow { id: r.get(0)?, ts: r.get(1)?, rule: r.get(2)?, severity: r.get(3)?, message: r.get(4)?, recovered: r.get(5)?, delivered: r.get(6)? })
        })?;
        Ok(rows.filter_map(Result::ok).collect())
    }

    pub fn insert_probe(&self, p: &ProbeRecord) -> R<()> {
        self.conn.execute(
            "INSERT INTO probes(ts, status, ttft_ms, decode_tok_s, tokens, detail, http_status, valid, invalid_reason, unverified) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![p.ts, p.status, p.ttft_ms, p.decode_tok_s, p.tokens, p.detail, p.http_status, p.valid, p.invalid_reason, p.unverified],
        )?;
        Ok(())
    }

    pub fn recent_probes(&self, limit: u32) -> R<Vec<ProbeRecord>> {
        let mut st = self.conn.prepare(&format!("SELECT {PROBE_COLS} FROM probes ORDER BY ts DESC, id DESC LIMIT ?1"))?;
        let rows = st.query_map([limit], probe_row)?;
        Ok(rows.filter_map(Result::ok).collect())
    }

    /// How many of the collector's own probes the gate ADMITTED since `ts` (the gate's start):
    /// that many of the gate's `admitted` counter are the monitor, not users.
    pub fn probes_admitted_since(&self, ts: i64) -> R<u64> {
        let mut st = self.conn.prepare(&format!("SELECT {PROBE_COLS} FROM probes WHERE ts >= ?1 AND status != 'skipped_busy'"))?;
        let rows = st.query_map([ts], probe_row)?;
        Ok(rows.filter_map(Result::ok).filter(ProbeRecord::was_admitted).count() as u64)
    }

    pub fn last_probe_attempt(&self) -> R<Option<i64>> {
        self.conn.query_row("SELECT MAX(ts) FROM probes", [], |r| r.get(0))
    }

    pub fn kv_get(&self, k: &str) -> R<Option<String>> {
        self.conn.query_row("SELECT v FROM kv WHERE k = ?1", [k], |r| r.get(0)).optional()
    }

    pub fn kv_del(&self, k: &str) -> R<()> {
        self.conn.execute("DELETE FROM kv WHERE k = ?1", [k])?;
        Ok(())
    }

    pub fn kv_set(&self, k: &str, v: &str) -> R<()> {
        self.conn.execute("INSERT INTO kv(k, v) VALUES (?1, ?2) ON CONFLICT(k) DO UPDATE SET v = excluded.v", params![k, v])?;
        Ok(())
    }

    /// Retention, tier by tier: raw samples, the 1-minute tier (rollups + histograms), and the
    /// 10-minute tier, which also bounds incidents, alerts, probes and the gate-log rollup.
    /// Open incidents are never pruned.
    pub fn prune(&self, now: i64, keep: Retention) -> R<usize> {
        let mut n = self.conn.execute("DELETE FROM samples WHERE ts < ?1", [now - keep.raw])?;
        for (res, secs) in [(60, keep.m1), (600, keep.m10)] {
            n += self.conn.execute("DELETE FROM rollup WHERE res = ?1 AND ts < ?2", params![res, now - secs])?;
            n += self.conn.execute("DELETE FROM hist WHERE res = ?1 AND ts < ?2", params![res, now - secs])?;
        }
        let oldest = now - keep.m10;
        n += self.conn.execute("DELETE FROM gatelog WHERE ts < ?1", [oldest])?;
        n += self.conn.execute("DELETE FROM incidents WHERE end IS NOT NULL AND end < ?1", [oldest])?;
        n += self.conn.execute("DELETE FROM alerts WHERE ts < ?1", [oldest])?;
        n += self.conn.execute("DELETE FROM probes WHERE ts < ?1", [oldest])?;
        // the scorecards keep the last 100 loadouts and the last 500 bench runs, however old
        n += self.conn.execute("DELETE FROM loadout WHERE id NOT IN (SELECT id FROM loadout ORDER BY last_seen DESC LIMIT 100)", [])?;
        n += self.conn.execute("DELETE FROM bench_runs WHERE id NOT IN (SELECT id FROM bench_runs ORDER BY id DESC LIMIT 500)", [])?;
        Ok(n)
    }

    fn metric_id(&self, name: &str, create: bool) -> R<Option<i64>> {
        if let Some(id) = self.metric_ids.borrow().get(name) {
            return Ok(Some(*id));
        }
        if create {
            self.conn.execute("INSERT OR IGNORE INTO metric_names(name) VALUES (?1)", [name])?;
        }
        let id: Option<i64> = self.conn.query_row("SELECT id FROM metric_names WHERE name = ?1", [name], |r| r.get(0)).optional()?;
        if let Some(id) = id {
            self.metric_ids.borrow_mut().insert(name.to_string(), id);
        }
        Ok(id)
    }

    /// Every metric name ever recorded.
    pub fn metric_names(&self) -> R<Vec<String>> {
        let mut st = self.conn.prepare("SELECT name FROM metric_names ORDER BY name")?;
        let rows = st.query_map([], |r| r.get(0))?;
        Ok(rows.filter_map(Result::ok).collect())
    }

    /// One finished bucket. `replace = false` keeps a row that is already there (the back-fill
    /// must never overwrite what the live loop wrote).
    pub fn write_rollup(&self, batch: &RollupBatch, replace: bool) -> R<()> {
        let verb = if replace { "INSERT OR REPLACE" } else { "INSERT OR IGNORE" };
        let mut st = self.conn.prepare_cached(&format!("{verb} INTO rollup(res, metric, ts, avg, min, max, n) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"))?;
        for (name, a) in &batch.rows {
            let Some(id) = self.metric_id(name, true)? else { continue };
            st.execute(params![batch.res, id, batch.ts, a.avg(), a.min, a.max, a.n])?;
        }
        Ok(())
    }

    /// Rows of one metric in `[from, to)`, oldest first.
    pub fn rollup_rows(&self, res: i64, metric: &str, from: i64, to: i64, mut each: impl FnMut(i64, Agg)) -> R<bool> {
        let Some(id) = self.metric_id(metric, false)? else { return Ok(false) };
        let mut st = self.conn.prepare_cached("SELECT ts, avg, min, max, n FROM rollup WHERE res = ?1 AND metric = ?2 AND ts >= ?3 AND ts < ?4 ORDER BY ts")?;
        let mut rows = st.query(params![res, id, from, to])?;
        while let Some(r) = rows.next()? {
            each(r.get(0)?, Agg::from_row(r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?));
        }
        Ok(true)
    }

    /// Streams the stored samples of `[from, to)` one at a time: a range is never held in memory.
    pub fn each_sample(&self, from: i64, to: i64, mut each: impl FnMut(Sample)) -> R<()> {
        let mut st = self.conn.prepare_cached("SELECT json FROM samples WHERE ts >= ?1 AND ts < ?2 ORDER BY ts")?;
        let mut rows = st.query(params![from, to])?;
        while let Some(r) = rows.next()? {
            let json: String = r.get(0)?;
            if let Ok(s) = serde_json::from_str(&json) {
                each(s);
            }
        }
        Ok(())
    }

    pub fn first_sample_at_or_after(&self, ts: i64) -> R<Option<i64>> {
        self.conn.query_row("SELECT MIN(ts) FROM samples WHERE ts >= ?1", [ts], |r| r.get(0))
    }

    pub fn first_sample_ts(&self) -> R<Option<i64>> {
        self.conn.query_row("SELECT MIN(ts) FROM samples", [], |r| r.get(0))
    }

    /// The bucket deltas of one closed latency window (only metrics that saw requests).
    pub fn write_hist_window(&self, w: &ClosedWindow) -> R<()> {
        for (i, (short, _)) in HIST_METRICS.iter().enumerate() {
            let acc = &w.accs[i];
            if acc.is_empty() {
                continue;
            }
            let le = encode_bounds(&acc.le);
            self.conn.execute("INSERT OR IGNORE INTO hist_bounds(le) VALUES (?1)", [&le])?;
            let bounds: i64 = self.conn.query_row("SELECT id FROM hist_bounds WHERE le = ?1", [&le], |r| r.get(0))?;
            self.conn.execute(
                "INSERT OR REPLACE INTO hist(res, metric, ts, bounds, counts, sum) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![w.res, short, w.ts, bounds, acc.encode_counts(), acc.sum],
            )?;
        }
        Ok(())
    }

    /// Stored windows of one latency metric in `[from, to)`, oldest first.
    pub fn hist_rows(&self, res: i64, metric: &str, from: i64, to: i64, mut each: impl FnMut(i64, HistAccum)) -> R<()> {
        let mut bounds_cache: HashMap<i64, Vec<f64>> = HashMap::new();
        let mut st = self.conn.prepare_cached("SELECT ts, bounds, counts, sum FROM hist WHERE res = ?1 AND metric = ?2 AND ts >= ?3 AND ts < ?4 ORDER BY ts")?;
        let mut rows = st.query(params![res, metric, from, to])?;
        while let Some(r) = rows.next()? {
            let (ts, bounds, counts, sum): (i64, i64, String, f64) = (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?);
            if let std::collections::hash_map::Entry::Vacant(slot) = bounds_cache.entry(bounds) {
                let le: Option<String> = self.conn.query_row("SELECT le FROM hist_bounds WHERE id = ?1", [bounds], |r| r.get(0)).optional()?;
                slot.insert(decode_bounds(&le.unwrap_or_default()));
            }
            let le = bounds_cache[&bounds].clone();
            let counts = HistAccum::decode_counts(&counts, le.len() + 1);
            let count = counts.iter().sum();
            each(ts, HistAccum { le, counts, sum, count });
        }
        Ok(())
    }

    /// The merged gate log of one 10-minute bucket.
    pub fn write_gatelog(&self, ts: i64, log: &LogDelta, replace: bool) -> R<()> {
        if log.is_empty() {
            return Ok(());
        }
        let verb = if replace { "INSERT OR REPLACE" } else { "INSERT OR IGNORE" };
        self.conn.execute(&format!("{verb} INTO gatelog(ts, json) VALUES (?1, ?2)"), params![ts, serde_json::to_string(log).unwrap_or_else(|_| "{}".into())])?;
        Ok(())
    }

    /// The stored 10-minute gate-log buckets of `[from, to)`, oldest first.
    pub fn each_gatelog(&self, from: i64, to: i64, mut each: impl FnMut(i64, LogDelta)) -> R<()> {
        let mut st = self.conn.prepare_cached("SELECT ts, json FROM gatelog WHERE ts >= ?1 AND ts < ?2 ORDER BY ts")?;
        let mut rows = st.query(params![from, to])?;
        while let Some(r) = rows.next()? {
            let (ts, json): (i64, String) = (r.get(0)?, r.get(1)?);
            if let Ok(d) = serde_json::from_str::<LogDelta>(&json) {
                each(ts, d);
            }
        }
        Ok(())
    }

    /// Every gate-log bucket in `[from, to)`, merged (the advice windows).
    pub fn gatelog_merged(&self, from: i64, to: i64) -> R<LogDelta> {
        let mut acc = LogDelta::default();
        self.each_gatelog(from, to, |_, d| acc.merge(&d))?;
        Ok(acc)
    }

    /// Probes in `[from, to)`, oldest first (the `c1_*` series and the gateway's probe share).
    pub fn probes_between(&self, from: i64, to: i64) -> R<Vec<ProbeRecord>> {
        let mut st = self.conn.prepare_cached(&format!("SELECT {PROBE_COLS} FROM probes WHERE ts >= ?1 AND ts < ?2 ORDER BY ts, id"))?;
        let rows = st.query_map(params![from, to], probe_row)?;
        Ok(rows.filter_map(Result::ok).collect())
    }

    /// When a metric was first recorded in a tier (None = never). Metrics were added over time, so
    /// two of them rarely cover the same span: a RATIO must only use the span both cover.
    pub fn rollup_first_ts(&self, res: i64, metric: &str) -> R<Option<i64>> {
        let Some(id) = self.metric_id(metric, false)? else { return Ok(None) };
        self.conn.query_row("SELECT MIN(ts) FROM rollup WHERE res = ?1 AND metric = ?2", params![res, id], |r| r.get::<_, Option<i64>>(0))
    }

    /// STARTUP SANITY CHECK + REPAIR for stored C1 probes: a reading more than
    /// `BURST_OVER_BASELINE` times the median of the others is not this server getting faster,
    /// it is an answer that arrived in one piece (2026-09-20: 26,773 and 5,596 tok/s, both right
    /// after a gateway restart, stored as VALID - the MODEL page then said "465 tok/s (best
    /// 26773.8)"). Those rows are marked invalid with a plain reason, and every loadout's C1
    /// figures are rebuilt from the probes that are left. Returns the repaired (ts, tok/s).
    pub fn repair_burst_probes(&self) -> R<Vec<(i64, f64)>> {
        let readings: Vec<(i64, f64)> = {
            let mut st = self.conn.prepare("SELECT ts, decode_tok_s FROM probes WHERE valid = 1 AND status = 'ok' AND decode_tok_s IS NOT NULL ORDER BY ts")?;
            let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.filter_map(Result::ok).collect()
        };
        if readings.len() < 8 {
            return Ok(Vec::new());
        }
        let typical = lss_core::rules::median(&readings.iter().map(|(_, v)| *v).collect::<Vec<_>>());
        let ceiling = typical * lss_core::probe::BURST_OVER_BASELINE;
        let bad: Vec<(i64, f64)> = readings.into_iter().filter(|(_, v)| typical > 0.0 && *v > ceiling).collect();
        for (ts, v) in &bad {
            let why = format!("{v:.0} tok/s is more than {:.0}x the usual {typical:.0} tok/s: the answer was buffered, not streamed", lss_core::probe::BURST_OVER_BASELINE);
            self.conn.execute("UPDATE probes SET valid = 0, invalid_reason = ?1, detail = ?2 WHERE ts = ?3", params![lss_core::probe::INVALID_BURST, why, ts])?;
        }
        if !bad.is_empty() {
            // the loadouts counted those readings: rebuild their C1 figures from what is left
            for (id, mut acc, first_seen) in self.recent_loadouts(50)? {
                let probes = self.probes_between(first_seen, i64::MAX)?;
                acc.rebuild_c1(&probes);
                self.save_loadout(&id, &acc, first_seen)?;
            }
        }
        Ok(bad)
    }

    /// STARTUP SANITY CHECK + REPAIR for probes the OLD idle gate missed (card #45, 2026-09-20):
    /// `running` is a 5 s gauge, so a request that starts and finishes entirely between two of
    /// the poll loop's own scrapes never showed up in it, and a probe whose before/after check
    /// looked clean was stored VALID. The poll loop's own stored samples around the probe's
    /// window still carry the proof: `generation_tokens_total` grew by more than the probe's own
    /// answer. (Live: counter growth of 50-1548 tokens caught across dozens of rows.) A GPU
    /// cold-clock signal was tried alongside this and REVERTED the same day: live, a power-
    /// managed GPU idling down between requests is the routine state right before almost every
    /// genuinely-idle probe, not a rare event, and the clock check could not tell that apart from
    /// an actual ramp corrupting a reading - it took whole hours to zero valid probes on deploy.
    /// The counter check alone had already caught every case the clock check was built for. An
    /// engine with no counter growth signal available is left alone - this can only REMOVE a
    /// false VALID, never invent one from data that is not there. Idempotent. Returns
    /// (ts, reason, detail).
    ///
    /// `poll_secs` is the collector's own poll cadence (`cfg.poll_secs`): the window around each
    /// probe must reach at least one full poll interval past the probe's own wall time on BOTH
    /// sides, or a probe that happens to fire near the middle of the gap between two poll
    /// scrapes can leave fewer than 2 samples inside a narrower window - not a boundary bug, just
    /// not enough room (2026-09-20 residual: 4 known-contaminated rows, 01:13/09:00/13:51/14:06,
    /// were still missed with a fixed few seconds of slack for exactly this reason).
    pub fn repair_probes_the_gauge_missed(&self, poll_secs: i64) -> R<Vec<(i64, &'static str, String)>> {
        let readings: Vec<(i64, f64, u32)> = {
            let mut st = self.conn.prepare("SELECT ts, decode_tok_s, tokens FROM probes WHERE valid = 1 AND status = 'ok' AND decode_tok_s IS NOT NULL AND tokens IS NOT NULL ORDER BY ts")?;
            let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            rows.filter_map(Result::ok).collect()
        };
        // +1 on top of the poll interval: jitter in exactly when a poll fires, and `each_sample`'s
        // range being HALF-OPEN ([from, to)) - a sample landing exactly on `to` is real evidence,
        // not slack to be careless with (18:27:19 was missed by precisely this one second first).
        let slack = poll_secs.max(1) + 1;
        let mut out = Vec::new();
        for (ts, decode_tok_s, tokens) in readings {
            // the probe's own wall time: TTFT is not stored on its own here, so this is generous
            // on purpose (decode time alone, plus a full poll interval of slack either side).
            //
            // #49, 2026-09-21: this window is NECESSARILY coarser than the live before/after check
            // (`validate()`'s own `gen_tokens_before`/`gen_tokens_after`, read by two tight HTTP
            // calls bracketing the probe to sub-second precision) - it can only compare counter
            // values at the samples table's OWN 5 s grid, so its true resolution is never finer
            // than one poll interval. Live case: a probe finished at 17:19:17.8 but the next stored
            // sample was not until 17:19:21 - 3.2 s of blind spot in which the gate's own admitted
            // counters proved separate trusted-lane traffic landed (+3 admissions the very next
            // tick). "Overcounting the window has no cost" is true only when nothing else is
            // actually nearby; when something is, a wider window WILL misattribute it to the
            // probe. That is an accepted, deliberate tradeoff here - repairs only ever ADD
            // invalidations and are meant to be conservative - not a defect, but worth being
            // honest about rather than claiming it is free: undercounting the window risks missing
            // real contamination outright; overcounting it risks flagging a probe that in fact ran
            // alone, whenever real traffic happens to land just outside its own true footprint but
            // still inside this grid-limited window.
            let decode_s = if decode_tok_s > 0.0 { f64::from(tokens) / decode_tok_s } else { 0.0 };
            let (from, to) = (ts - slack, ts + decode_s.ceil() as i64 + slack);
            let (mut gen_first, mut gen_last, mut n) = (None::<f64>, None::<f64>, 0usize);
            self.each_sample(from, to, |s| {
                n += 1;
                if let Some(m) = &s.metrics {
                    gen_first.get_or_insert(m.generation_tokens_total);
                    gen_last = Some(m.generation_tokens_total);
                }
            })?;
            if n < 2 {
                // too little stored around this probe to say anything either way: left alone
                continue;
            }
            let verdict = match (gen_first, gen_last) {
                (Some(first), Some(last)) if last >= first => {
                    let grown = last - first;
                    (grown > f64::from(tokens) + lss_core::probe::COUNTER_MARGIN_TOKENS).then(|| {
                        (
                            lss_core::probe::INVALID_CONTENDED,
                            format!(
                                "generation_tokens_total grew by {grown:.0}, {:.0} more than the probe's own {tokens} tokens: something else generated on this engine during the probe (found on repair from the stored samples, {}s window, {n} samples in it - no finer than the poll cadence)",
                                grown - f64::from(tokens),
                                to - from,
                            ),
                        )
                    })
                }
                _ => None,
            };
            if let Some((reason, detail)) = verdict {
                self.conn.execute("UPDATE probes SET valid = 0, invalid_reason = ?1, detail = ?2 WHERE ts = ?3", params![reason, detail, ts])?;
                out.push((ts, reason, detail));
            }
        }
        if !out.is_empty() {
            // the loadouts counted those readings: rebuild their C1 figures from what is left
            for (id, mut acc, first_seen) in self.recent_loadouts(50)? {
                let probes = self.probes_between(first_seen, i64::MAX)?;
                acc.rebuild_c1(&probes);
                self.save_loadout(&id, &acc, first_seen)?;
            }
        }
        Ok(out)
    }

    /// ONE-TIME CORRECTION (card #45, 2026-09-20): the GPU cold-clock check shipped for part of
    /// one day, then was shown wrong and removed the same day (see `validate()`'s own doc
    /// comment - live, it invalidated the routine idle-power state of this GPU, not a rare
    /// event). A repair only ever marks MORE rows invalid, never un-invalidates, so every row it
    /// flagged before removal would stay wrongly invalid forever unless undone explicitly. This
    /// reverts every row still carrying `invalid_reason = 'cold_clock'` - a reason no code path
    /// writes any more - back to valid, and rebuilds affected loadouts' C1 figures. Idempotent:
    /// once none are left, every future call is a no-op. Returns the timestamps restored.
    pub fn undo_cold_clock_false_invalidations(&self) -> R<Vec<i64>> {
        let restored: Vec<i64> = {
            let mut st = self.conn.prepare("SELECT ts FROM probes WHERE invalid_reason = 'cold_clock'")?;
            let rows = st.query_map([], |r| r.get(0))?;
            rows.filter_map(Result::ok).collect()
        };
        if restored.is_empty() {
            return Ok(Vec::new());
        }
        self.conn.execute("UPDATE probes SET valid = 1, invalid_reason = NULL, detail = '' WHERE invalid_reason = 'cold_clock'", [])?;
        for (id, mut acc, first_seen) in self.recent_loadouts(50)? {
            let probes = self.probes_between(first_seen, i64::MAX)?;
            acc.rebuild_c1(&probes);
            self.save_loadout(&id, &acc, first_seen)?;
        }
        Ok(restored)
    }

    /// STARTUP SANITY CHECK + REPAIR for the token history. A bucket that says more prompt tokens
    /// came from the cache than any hardware could serve (> `MAX_CACHED_TOK_S` sustained over the
    /// whole bucket) AND more than twice the prompt tokens of the same bucket is not traffic: it
    /// is a counter's whole life booked into one step (2026-09-19 17:46: 772 018 048 "cached"
    /// tokens in one minute with 0 prompt tokens, when the counter was first recorded). Those
    /// tokens are REAL cache hits of the hours before, so they are not thrown away: they are
    /// spread over the earlier buckets in which prompts were read but no cache hit was recorded,
    /// in proportion to each bucket's prompt tokens and never more than them. Totals stay right,
    /// and no hour shows an impossible spike. Returns (tier, ts, tokens, tokens spread back).
    /// (A cache hit can legitimately land a bucket before its prompt is counted, hence both
    /// conditions. Safe at every start: once repaired there is nothing left to find.)
    pub fn repair_impossible_token_buckets(&self) -> R<Vec<(i64, i64, f64, f64)>> {
        const MAX_CACHED_TOK_S: f64 = 500_000.0;
        let (Some(cached), Some(prompt)) = (self.metric_id("tok_cached", false)?, self.metric_id("tok_prompt", false)?) else { return Ok(Vec::new()) };
        let mut bad: Vec<(i64, i64, f64)> = Vec::new();
        {
            let mut st = self.conn.prepare("SELECT c.res, c.ts, c.avg * c.n, COALESCE((SELECT p.avg * p.n FROM rollup p WHERE p.res = c.res AND p.metric = ?2 AND p.ts = c.ts), 0) FROM rollup c WHERE c.metric = ?1")?;
            let mut rows = st.query(params![cached, prompt])?;
            while let Some(r) = rows.next()? {
                let (res, ts, sum, prompt_sum): (i64, i64, f64, f64) = (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?);
                if sum > MAX_CACHED_TOK_S * res as f64 && sum > 2.0 * prompt_sum {
                    bad.push((res, ts, sum));
                }
            }
        }
        let mut out = Vec::new();
        for (res, ts, sum) in bad {
            // the earlier buckets of this tier that read prompts while the cache counter said nothing
            let mut holes: Vec<(i64, f64)> = Vec::new();
            {
                let mut st = self.conn.prepare("SELECT p.ts, p.avg * p.n FROM rollup p WHERE p.res = ?1 AND p.metric = ?2 AND p.ts < ?3 AND p.avg * p.n > 0 AND COALESCE((SELECT c.avg * c.n FROM rollup c WHERE c.res = p.res AND c.metric = ?4 AND c.ts = p.ts), 0) = 0")?;
                let mut rows = st.query(params![res, prompt, ts, cached])?;
                while let Some(r) = rows.next()? {
                    holes.push((r.get(0)?, r.get(1)?));
                }
            }
            let room: f64 = holes.iter().map(|(_, p)| *p).sum();
            let factor = if room > 0.0 { (sum / room).min(1.0) } else { 0.0 };
            for (hole_ts, prompt_sum) in &holes {
                let v = prompt_sum * factor;
                self.conn.execute("INSERT OR REPLACE INTO rollup(res, metric, ts, avg, min, max, n) VALUES (?1, ?2, ?3, ?4, ?4, ?4, 1)", params![res, cached, hole_ts, v])?;
            }
            self.conn.execute("DELETE FROM rollup WHERE res = ?1 AND metric = ?2 AND ts = ?3", params![res, cached, ts])?;
            out.push((res, ts, sum, room * factor));
        }
        Ok(out)
    }

    /// STARTUP SANITY CHECK + REPAIR for a persisted loadout's `read_peak_tok_s` (card #88,
    /// verifier-2 closing #44): #44 D2 fixed the WRITE-time formula (wall-clock seconds, not
    /// summed request-seconds across concurrent readers - the same class of inflation
    /// `repair_burst_probes` above guards on the decode side), but a value computed under the OLD
    /// formula before that fix is a persisted running max: it never comes back down on its own.
    /// Live proof this repair exists to fix: loadout `fad9f4db5111` stored `read_peak_tok_s`
    /// 242,702.3 beside `13d4dd53f66b`'s real 6,406.4, both the same model on the same image tag -
    /// `lss compare` would put a 38x-inflated number beside a real one in the one feature whose
    /// whole job is comparing loadouts. UNLIKE the probe/C1 repairs above, there is no raw
    /// per-window record to replay the fixed formula against - the sliding read-window that
    /// produced the peak was in-memory only, never itself persisted - so an impossible value is
    /// CLEARED (never guessed at a corrected number), the same "only ever remove a false reading,
    /// never invent one" rule `repair_probes_the_gauge_missed` documents. `MAX_PREFILL_TOK_S` is
    /// deliberately generous for any realistic self-hosted box (README: "anyone can use this") -
    /// comfortably below the card's own corrupted 242,702 with real margin, comfortably above any
    /// plausible real reading (the card's own real figure is 6,406). Idempotent: once cleared,
    /// `read_peak_tok_s` starts accumulating fresh under the current (correct) formula and this
    /// repair finds nothing on the next start. Returns (loadout id, the cleared value).
    pub fn repair_impossible_read_peaks(&self) -> R<Vec<(String, f64)>> {
        const MAX_PREFILL_TOK_S: f64 = 100_000.0;
        let mut cleared = Vec::new();
        for (id, mut acc, first_seen) in self.recent_loadouts(50)? {
            if acc.read_peak_tok_s > MAX_PREFILL_TOK_S {
                cleared.push((id.id.clone(), acc.read_peak_tok_s));
                acc.read_peak_tok_s = 0.0;
                self.save_loadout(&id, &acc, first_seen)?;
            }
        }
        Ok(cleared)
    }

    /// Sum of a counted metric (`tok_gen` …) over `[from, to)` in one rollup tier.
    pub fn rollup_sum(&self, res: i64, metric: &str, from: i64, to: i64) -> R<f64> {
        let mut total = 0.0;
        self.rollup_rows(res, metric, from, to, |_, a| total += a.sum)?;
        Ok(total)
    }

    pub fn save_loadout(&self, id: &LoadoutIdentity, acc: &LoadoutAcc, first_seen: i64) -> R<()> {
        self.conn.execute(
            "INSERT INTO loadout(id, identity, acc, first_seen, last_seen) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(id) DO UPDATE SET acc = excluded.acc, last_seen = excluded.last_seen",
            params![id.id, serde_json::to_string(id).unwrap_or_default(), serde_json::to_string(acc).unwrap_or_default(), first_seen, acc.last_ts.max(first_seen)],
        )?;
        Ok(())
    }

    pub fn load_loadout(&self, id: &str) -> R<Option<(LoadoutAcc, i64)>> {
        let row: Option<(String, i64)> = self.conn.query_row("SELECT acc, first_seen FROM loadout WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        Ok(row.map(|(acc, first)| (serde_json::from_str(&acc).unwrap_or_default(), first)))
    }

    /// The newest `limit` loadouts, newest first.
    pub fn recent_loadouts(&self, limit: u32) -> R<Vec<(LoadoutIdentity, LoadoutAcc, i64)>> {
        let mut st = self.conn.prepare("SELECT identity, acc, first_seen FROM loadout ORDER BY last_seen DESC, first_seen DESC LIMIT ?1")?;
        let rows = st.query_map([limit], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?)))?;
        Ok(rows.filter_map(Result::ok).filter_map(|(id, acc, first)| Some((serde_json::from_str(&id).ok()?, serde_json::from_str(&acc).unwrap_or_default(), first))).collect())
    }

    /// A bench run that has just started: returns its id.
    pub fn bench_start(&self, loadout_id: &str, profile: &str, started_at: i64, card: &Scorecard) -> R<i64> {
        self.conn.execute("INSERT INTO bench_runs(loadout_id, profile, started_at, status, scorecard) VALUES (?1, ?2, ?3, 'running', ?4)", params![loadout_id, profile, started_at, serde_json::to_string(card).unwrap_or_default()])?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn bench_finish(&self, id: i64, card: &Scorecard) -> R<()> {
        self.conn.execute("UPDATE bench_runs SET ended_at = ?2, status = ?3, scorecard = ?4 WHERE id = ?1", params![id, card.ended_at, card.status, serde_json::to_string(card).unwrap_or_default()])?;
        Ok(())
    }

    /// Runs left `running` by a collector that died mid-bench: they did not finish.
    pub fn bench_close_stale(&self, now: i64, why: &str) -> R<usize> {
        let mut st = self.conn.prepare("SELECT id, scorecard FROM bench_runs WHERE status = 'running'")?;
        let stale: Vec<(i64, String)> = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.filter_map(Result::ok).collect();
        for (id, json) in &stale {
            let mut card: Scorecard = serde_json::from_str(json).unwrap_or_default();
            card.run_id = *id;
            card.status = "aborted".into();
            card.aborted = Some(why.to_string());
            card.ended_at = now;
            card.duration_s = (now - card.started_at).max(0);
            self.bench_finish(*id, &card)?;
        }
        Ok(stale.len())
    }

    /// The newest `limit` bench runs, newest first (finished or not).
    pub fn bench_runs(&self, limit: u32) -> R<Vec<Scorecard>> {
        let mut st = self.conn.prepare("SELECT id, scorecard FROM bench_runs ORDER BY id DESC LIMIT ?1")?;
        let rows = st.query_map([limit], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
        Ok(rows
            .filter_map(Result::ok)
            .filter_map(|(id, json)| {
                let mut card: Scorecard = serde_json::from_str(&json).ok()?;
                card.run_id = id;
                Some(card)
            })
            .collect())
    }

    /// Probe requests the gate answered since `ts` (they are in its log and in its user table).
    pub fn probes_logged_since(&self, ts: i64) -> R<u64> {
        self.conn.query_row("SELECT COUNT(*) FROM probes WHERE ts >= ?1 AND http_status IS NOT NULL", [ts], |r| r.get::<_, i64>(0)).map(|n| n.max(0) as u64)
    }

    pub fn begin(&self) -> R<()> {
        self.conn.execute_batch("BEGIN IMMEDIATE")
    }

    pub fn commit(&self) -> R<()> {
        self.conn.execute_batch("COMMIT")
    }

    /// Size of the database file plus its WAL, in bytes.
    pub fn size_bytes(&self) -> R<i64> {
        let pages: i64 = self.conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
        let size: i64 = self.conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
        Ok(pages * size)
    }
}

const PROBE_COLS: &str = "ts, status, ttft_ms, decode_tok_s, tokens, detail, http_status, valid, invalid_reason, unverified";

fn probe_row(r: &rusqlite::Row) -> R<ProbeRecord> {
    Ok(ProbeRecord { ts: r.get(0)?, status: r.get(1)?, ttft_ms: r.get(2)?, decode_tok_s: r.get(3)?, tokens: r.get(4)?, detail: r.get(5)?, http_status: r.get(6)?, valid: r.get(7)?, invalid_reason: r.get(8)?, unverified: r.get(9)? })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lss_core::rules::Severity;

    #[test]
    fn incident_open_close_and_idempotent_events() {
        let db = Db::memory();
        db.open_incident("serve_down", 100, "/v1/models: refused").unwrap();
        let open = db.incidents_since(0).unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].end, None);
        assert_eq!(db.close_incident("serve_down", 400).unwrap(), 1);
        assert_eq!(db.incidents_since(0).unwrap()[0].end, Some(400));
        assert_eq!(db.close_incident("serve_down", 500).unwrap(), 0, "nothing left open");

        assert!(db.event_incident("xid", 77, "GPU3 Xid 8", None).unwrap());
        assert!(!db.event_incident("xid", 77, "GPU3 Xid 8", None).unwrap(), "same event is booked once");
        assert!(db.event_incident("xid", 77, "GPU1 Xid 8", None).unwrap());
        let all = db.incidents_since(0).unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].start, 100, "newest first");
    }

    #[test]
    fn an_xid_is_one_incident_however_its_description_is_worded() {
        let db = Db::memory();
        let key = lss_core::incidents::xid_key("GPU1", 154, 1_789_321_416);
        assert!(db.event_incident("xid", 1_789_321_416, "GPU1 Xid 154 (see NVIDIA Xid catalogue) GPU recovery action changed", Some(&key)).unwrap());
        // the same kernel line, re-read by a version that words the hint differently
        assert!(!db.event_incident("xid", 1_789_321_416, "GPU1 Xid 154 (GPU recovery action changed (reset / reboot required)) GPU recovery action changed", Some(&key)).unwrap());
        // another GPU, another Xid number or another second IS another incident
        assert!(db.event_incident("xid", 1_789_321_416, "GPU2 Xid 154 x", Some(&lss_core::incidents::xid_key("GPU2", 154, 1_789_321_416))).unwrap());
        assert!(db.event_incident("xid", 1_789_321_416, "GPU1 Xid 119 x", Some(&lss_core::incidents::xid_key("GPU1", 119, 1_789_321_416))).unwrap());
        assert!(db.event_incident("xid", 1_789_321_417, "GPU1 Xid 154 x", Some(&lss_core::incidents::xid_key("GPU1", 154, 1_789_321_417))).unwrap());
        assert_eq!(db.incidents_since(0).unwrap().len(), 4);
    }

    #[test]
    fn the_migration_merges_xids_that_were_booked_twice_under_two_wordings() {
        // the incidents table as the previous collector left it (ids 4 and 7 are one event)
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE incidents(id INTEGER PRIMARY KEY AUTOINCREMENT, start INTEGER NOT NULL, end INTEGER, kind TEXT NOT NULL, detail TEXT NOT NULL);
             INSERT INTO incidents(id, start, end, kind, detail) VALUES
               (3, 1789321415, 1789321415, 'xid', 'GPU1 Xid 119 (GSP RPC timeout / GSP error) pid=1973913'),
               (4, 1789321416, 1789321416, 'xid', 'GPU1 Xid 154 (see NVIDIA Xid catalogue) GPU recovery action changed from 0x0 (None) to 0x1 (GPU Reset Required)'),
               (5, 1789679648, 1789679648, 'xid', 'GPU1 Xid 8 (GPU stopped processing (hang / watchdog)) pid=569804'),
               (7, 1789321416, 1789321416, 'xid', 'GPU1 Xid 154 (GPU recovery action changed (reset / reboot required)) GPU recovery action changed from 0x0 (None) to 0x1 (GPU Reset Required)'),
               (8, 1789836312, 1789836312, 'container_restart', 'the gateway new StartedAt (previous run lasted 5h04m)');",
        )
        .unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        Db::migrate(&conn).unwrap();
        Db::migrate(&conn).unwrap(); // idempotent
        let db = Db { conn, metric_ids: RefCell::default() };
        let all = db.incidents_since(0).unwrap();
        let ids: Vec<i64> = all.iter().map(|i| i.id).collect();
        assert_eq!(ids, vec![8, 5, 4, 3], "id 7 is merged into id 4; nothing else is touched");
        let merged = all.iter().find(|i| i.id == 4).unwrap();
        assert!(merged.detail.contains("reset / reboot required"), "the surviving row carries the newer wording: {}", merged.detail);
        // and the back-fill reading that kernel line again books nothing
        let key = lss_core::incidents::xid_key("GPU1", 154, 1_789_321_416);
        assert!(!db.event_incident("xid", 1_789_321_416, "GPU1 Xid 154 (yet another wording)", Some(&key)).unwrap());
        assert!(!db.event_incident("xid", 1_789_679_648, "GPU1 Xid 8 (reworded)", Some(&lss_core::incidents::xid_key("GPU1", 8, 1_789_679_648))).unwrap());
        assert_eq!(db.incidents_since(0).unwrap().len(), 4);
    }

    #[test]
    fn seven_day_window_keeps_open_and_overlapping_incidents() {
        let db = Db::memory();
        db.open_incident("serve_down", 10, "still open").unwrap();
        db.event_incident("xid", 20, "old", None).unwrap();
        db.open_incident("gate_down", 30, "ended inside").unwrap();
        db.close_incident("gate_down", 1500).unwrap();
        let kinds: Vec<String> = db.incidents_since(1000).unwrap().into_iter().map(|i| i.kind).collect();
        assert_eq!(kinds, vec!["gate_down", "serve_down"]);
    }

    #[test]
    fn restarts_today_excludes_the_gate() {
        let db = Db::memory();
        db.event_incident("container_restart", 50, "glm-x RestartCount 0 -> 1", None).unwrap();
        db.event_incident("container_restart", 60, "the gateway new StartedAt", None).unwrap();
        db.event_incident("container_restart", 5, "glm-x RestartCount yesterday", None).unwrap();
        assert_eq!(db.count_restarts_since(10, "the gateway").unwrap(), 1);
    }

    /// D1, 2026-09-20: `unverified` used to have no column at all, so `insert_probe` dropped it
    /// silently and `probe_row` hard-coded `false` - a `CouldNotRuleOut` reading came back
    /// PROVED. This is the round-trip that would have caught it.
    #[test]
    fn a_could_not_rule_out_probe_is_still_unverified_after_a_db_round_trip() {
        let db = Db::memory();
        let detail = format!("{}: no gateway and this engine does not report how many requests are running", lss_core::probe::UNVERIFIED_NOTE);
        db.insert_probe(&ProbeRecord { ts: 1000, status: "ok".into(), ttft_ms: Some(120.0), decode_tok_s: Some(248.6), tokens: Some(128), detail: detail.clone(), http_status: Some(200), valid: true, invalid_reason: None, unverified: true }).unwrap();
        let back = &db.recent_probes(1).unwrap()[0];
        assert!(back.valid && back.unverified, "{back:?}");
        assert_eq!(back.detail, detail);
        assert!(db.last_valid_probe().unwrap().unwrap().unverified, "the reading `lss status` shows also carries the caveat");
    }

    #[test]
    fn alerts_probes_kv_and_retention() {
        let db = Db::memory();
        let id = db.insert_alert(&AlertEvent { ts: 1000, rule: "serve_down".into(), severity: Severity::Warn, message: "m".into(), recovered: false }).unwrap();
        assert!(!db.recent_alerts(20).unwrap()[0].delivered);
        db.mark_delivered(id).unwrap();
        let a = &db.recent_alerts(20).unwrap()[0];
        assert!(a.delivered);
        assert_eq!(a.severity, "warn");

        db.insert_probe(&ProbeRecord::skipped_busy(900, 1.0, 0.0)).unwrap();
        db.insert_probe(&ProbeRecord { ts: 1200, status: "ok".into(), ttft_ms: Some(90.0), decode_tok_s: Some(190.0), tokens: Some(128), detail: String::new(), http_status: Some(200), valid: true, invalid_reason: None , unverified: false}).unwrap();
        assert_eq!(db.last_probe_attempt().unwrap(), Some(1200));
        assert_eq!(db.recent_probes(50).unwrap()[1].status, "skipped_busy");
        assert!(!db.recent_probes(50).unwrap()[0].unverified);

        assert_eq!(db.kv_get("engine").unwrap(), None);
        db.kv_set("engine", "{}").unwrap();
        db.kv_set("engine", "{\"a\":1}").unwrap();
        assert_eq!(db.kv_get("engine").unwrap().as_deref(), Some("{\"a\":1}"));

        db.insert_sample(&Sample { ts: 500, ..Default::default() }).unwrap();
        db.insert_sample(&Sample { ts: 5000, serve_up: true, ..Default::default() }).unwrap();
        db.open_incident("serve_down", 1, "ancient but open").unwrap();
        db.prune(1100 + 50, Retention { raw: 50, m1: 50, m10: 50 }).unwrap();
        assert_eq!(db.samples_since(0).unwrap().len(), 1);
        assert_eq!(db.samples_since(0).unwrap()[0].ts, 5000);
        assert_eq!(db.recent_probes(50).unwrap().len(), 1);
        assert!(db.recent_alerts(20).unwrap().is_empty());
        assert_eq!(db.incidents_since(0).unwrap().len(), 1, "open incidents survive retention");
    }

    #[test]
    fn a_database_from_before_http_status_is_migrated_and_its_rows_still_count() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE probes (id INTEGER PRIMARY KEY AUTOINCREMENT, ts INTEGER NOT NULL, status TEXT NOT NULL, ttft_ms REAL, decode_tok_s REAL, tokens INTEGER, detail TEXT NOT NULL DEFAULT '');
             INSERT INTO probes(ts, status, detail) VALUES (100, 'ok', ''), (200, 'error', 'HTTP 429'), (300, 'skipped_busy', 'running=1 queue=0'), (400, 'ok', '');",
        )
        .unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        Db::migrate(&conn).unwrap();
        Db::migrate(&conn).unwrap(); // idempotent
        let db = Db { conn, metric_ids: RefCell::default() };
        db.insert_probe(&ProbeRecord { ts: 500, status: "error".into(), ttft_ms: None, decode_tok_s: None, tokens: None, detail: "HTTP 502".into(), http_status: Some(502), valid: false, invalid_reason: Some("error".into()) , unverified: false}).unwrap();
        assert_eq!(db.recent_probes(50).unwrap()[0].http_status, Some(502));
        assert!(db.recent_probes(50).unwrap().iter().all(|p| !p.unverified), "a database from before this column existed reads as proved, never as unproved");
        // since the gate started at t=150: 429 was rejected before admission, skipped was never sent
        assert_eq!(db.probes_admitted_since(150).unwrap(), 2);
        assert_eq!(db.probes_admitted_since(0).unwrap(), 3);
        db.kv_set("xid_backfill_from", "1").unwrap();
        db.kv_del("xid_backfill_from").unwrap();
        assert_eq!(db.kv_get("xid_backfill_from").unwrap(), None);
    }

    #[test]
    fn legacy_probes_are_judged_by_their_ttft_and_the_last_valid_reading_is_found() {
        // a table exactly as the first collector created it, with the live rows of 2026-09-19
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE probes (id INTEGER PRIMARY KEY AUTOINCREMENT, ts INTEGER NOT NULL, status TEXT NOT NULL, ttft_ms REAL, decode_tok_s REAL, tokens INTEGER, detail TEXT NOT NULL DEFAULT '');
             INSERT INTO probes(ts, status, ttft_ms, decode_tok_s) VALUES (100, 'ok', 135.0, 200.2), (200, 'ok', 133.0, 192.6), (300, 'ok', 57800.0, 153.9), (400, 'ok', 1444.0, 141.4), (500, 'ok', 26548.4, 183.9);
             INSERT INTO probes(ts, status, detail) VALUES (600, 'skipped_busy', 'running=1 queue=0'), (700, 'timeout', 'timed out');",
        )
        .unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        Db::migrate(&conn).unwrap();
        let db = Db { conn, metric_ids: RefCell::default() };
        assert!(db.recent_probes(50).unwrap().iter().all(|p| p.valid), "the new column defaults to valid");
        assert_eq!(db.invalidate_legacy_probes(3000.0).unwrap(), 4);
        assert_eq!(db.invalidate_legacy_probes(3000.0).unwrap(), 0, "idempotent");
        let rows = db.recent_probes(50).unwrap();
        let reason = |ts: i64| rows.iter().find(|p| p.ts == ts).unwrap().invalid_reason.clone();
        assert_eq!(reason(300).as_deref(), Some("slow_ttft"));
        assert_eq!(reason(500).as_deref(), Some("slow_ttft"));
        assert_eq!(reason(600).as_deref(), Some("busy_before"));
        assert_eq!(reason(700).as_deref(), Some("timeout"));
        assert_eq!(reason(400), None, "1.4 s TTFT passes the 3 s test; contention without a TTFT trace cannot be judged after the fact");
        assert_eq!(db.last_valid_probe().unwrap().unwrap().ts, 400);
        assert_eq!(db.recent_valid_tok_s(12).unwrap(), vec![200.2, 192.6, 141.4], "oldest first, invalid ones left out");
        assert_eq!(db.recent_valid_tok_s(2).unwrap(), vec![192.6, 141.4]);

        let mut bad = ProbeRecord { ts: 800, status: "ok".into(), ttft_ms: Some(140.0), decode_tok_s: Some(120.0), tokens: Some(128), detail: String::new(), http_status: Some(200), valid: true, invalid_reason: None , unverified: false};
        bad.invalidate("contended", "the gate admitted 1 other request(s) during the probe".into());
        db.insert_probe(&bad).unwrap();
        let back = &db.recent_probes(1).unwrap()[0];
        assert_eq!((back.valid, back.invalid_reason.as_deref()), (false, Some("contended")));
        assert_eq!(db.last_valid_probe().unwrap().unwrap().ts, 400, "an invalid probe never becomes the reading");
    }
    #[test]
    fn a_counters_whole_life_booked_into_one_bucket_is_spread_back_and_real_traffic_is_untouched() {
        use lss_core::series::{Agg, RollupBatch};
        let db = Db::memory();
        let put = |res: i64, ts: i64, metric: &str, v: f64| db.write_rollup(&RollupBatch { res, ts, rows: vec![(metric.to_string(), Agg::from_row(v, v, v, 1.0))] }, true).unwrap();
        // twelve hours in which prompts were read while the cache counter was not recorded (0)...
        for k in 0..4 {
            put(60, 1_000 + k * 60, "tok_prompt", 250_000_000.0);
            put(60, 1_000 + k * 60, "tok_cached", 0.0);
        }
        // ... then the counter's whole life in one minute, with no prompt tokens of its own
        put(60, 6_000, "tok_cached", 772_018_048.0);
        // real traffic after it: a busy minute next to its prompt tokens, and a cache hit that
        // landed one bucket before its prompt was counted
        put(60, 6_060, "tok_cached", 9_000_000.0);
        put(60, 6_060, "tok_prompt", 9_100_000.0);
        put(60, 6_120, "tok_cached", 250_000.0);
        let fixed = db.repair_impossible_token_buckets().unwrap();
        assert_eq!(fixed.len(), 1);
        assert_eq!((fixed[0].0, fixed[0].1), (60, 6_000));
        assert!((fixed[0].3 - 772_018_048.0).abs() < 1.0, "all of it found a home: {fixed:?}");
        // the total is what it was; no bucket is impossible any more; cached <= prompt everywhere
        assert!((db.rollup_sum(60, "tok_cached", 0, 10_000).unwrap() - (772_018_048.0 + 9_250_000.0)).abs() < 1.0);
        assert_eq!(db.rollup_sum(60, "tok_cached", 6_000, 6_060).unwrap(), 0.0);
        let first = db.rollup_sum(60, "tok_cached", 1_000, 1_060).unwrap();
        assert!((first - 193_004_512.0).abs() < 1.0 && first <= 250_000_000.0, "a quarter each, under that minute's prompt tokens: {first}");
        assert!(db.repair_impossible_token_buckets().unwrap().is_empty(), "nothing left to repair: safe to run at every start");
        // more than the earlier prompts can hold: capped at them (cached is never more than read)
        let db = Db::memory();
        let put = |ts: i64, metric: &str, v: f64| db.write_rollup(&RollupBatch { res: 60, ts, rows: vec![(metric.to_string(), Agg::from_row(v, v, v, 1.0))] }, true).unwrap();
        put(1_000, "tok_prompt", 1_000_000.0);
        put(6_000, "tok_cached", 772_018_048.0);
        let fixed = db.repair_impossible_token_buckets().unwrap();
        assert_eq!((fixed[0].3, db.rollup_sum(60, "tok_cached", 0, 10_000).unwrap()), (1_000_000.0, 1_000_000.0));
    }

    #[test]
    fn stored_burst_readings_are_re_judged_and_the_loadout_figures_rebuilt() {
        use lss_core::loadout::{identity, LoadoutAcc};
        let db = Db::memory();
        let put = |ts: i64, v: f64| {
            db.insert_probe(&lss_core::probe::ProbeRecord { ts, status: "ok".into(), ttft_ms: Some(140.0), decode_tok_s: Some(v), tokens: Some(128), detail: String::new(), http_status: Some(200), valid: true, invalid_reason: None , unverified: false}).unwrap();
        };
        // ten ordinary readings around 190 tok/s, and the two real ones of 2026-09-20
        for (i, v) in [188.0, 191.0, 185.0, 193.0, 190.0, 186.0, 192.0, 189.0, 187.0, 194.0].into_iter().enumerate() {
            put(1_000 + i as i64 * 300, v);
        }
        put(5_000, 5_596.3);
        put(5_300, 26_773.8);
        let id = identity("model-a", "img:1", &["python3".to_string()], &[]);
        let mut acc = LoadoutAcc::default();
        for p in db.probes_between(0, i64::MAX).unwrap() {
            acc.observe_probe(&p);
        }
        assert_eq!(acc.c1_probes, 12);
        db.save_loadout(&id, &acc, 0).unwrap();

        let repaired = db.repair_burst_probes().unwrap();
        assert_eq!(repaired.iter().map(|(ts, _)| *ts).collect::<Vec<_>>(), vec![5_000, 5_300]);
        let rows = db.probes_between(0, i64::MAX).unwrap();
        let bad: Vec<&lss_core::probe::ProbeRecord> = rows.iter().filter(|p| !p.valid).collect();
        assert_eq!(bad.len(), 2);
        assert_eq!(bad[0].invalid_reason.as_deref(), Some("burst"));
        assert!(bad[0].detail.contains("buffered, not streamed"), "{}", bad[0].detail);
        // the loadout no longer counts them, and "one user alone" is the median of what is left
        let (fixed, _) = db.load_loadout(&id.id).unwrap().expect("loadout");
        assert_eq!((fixed.c1_probes, fixed.c1_best), (10, 194.0));
        let row = lss_core::loadout::row(&id, &fixed, 6_000, true);
        assert_eq!((row.c1_tok_s, row.c1_best_tok_s), (Some(189.5), Some(194.0)));
        // running it again finds nothing: safe at every start
        assert!(db.repair_burst_probes().unwrap().is_empty());
    }

    /// card #88: the exact live shapes from the card's own reproducer - one loadout with an
    /// impossible pre-#44-D2 peak (242,702.3), one with a real one (6,406.4), both surviving a
    /// collector restart the way `lss compare`/`/loadouts` would actually see them.
    #[test]
    fn an_impossible_prefill_peak_from_before_the_wall_clock_fix_is_cleared_a_real_one_is_untouched() {
        use lss_core::loadout::identity;
        let db = Db::memory();
        let impossible = identity("model-a", "img:1", &["python3".to_string()], &[]);
        let mut bad_acc = LoadoutAcc { read_peak_tok_s: 242_702.3, ..Default::default() };
        db.save_loadout(&impossible, &bad_acc, 0).unwrap();
        let real = identity("model-a", "img:1", &["python3".to_string(), "--x".to_string()], &[]);
        let good_acc = LoadoutAcc { read_peak_tok_s: 6_406.4, ..Default::default() };
        db.save_loadout(&real, &good_acc, 100).unwrap();

        let cleared = db.repair_impossible_read_peaks().unwrap();
        assert_eq!(cleared, vec![(impossible.id.clone(), 242_702.3)]);
        let (fixed, _) = db.load_loadout(&impossible.id).unwrap().expect("loadout");
        assert_eq!(fixed.read_peak_tok_s, 0.0, "cleared, never guessed at a corrected number");
        let (untouched, _) = db.load_loadout(&real.id).unwrap().expect("loadout");
        assert_eq!(untouched.read_peak_tok_s, 6_406.4, "a real reading below the ceiling is left exactly as it was");

        // running it again finds nothing: safe at every start, and it starts fresh under the
        // current (correct) formula rather than getting stuck at 0 forever
        assert!(db.repair_impossible_read_peaks().unwrap().is_empty());
        bad_acc.read_peak_tok_s = 5_800.0; // a fresh, correctly-computed reading after the clear
        db.save_loadout(&impossible, &bad_acc, 0).unwrap();
        assert!(db.repair_impossible_read_peaks().unwrap().is_empty());
        assert_eq!(db.load_loadout(&impossible.id).unwrap().unwrap().0.read_peak_tok_s, 5_800.0);
    }

    /// #45, 2026-09-20: the live 08:45:11 case exactly - running=0.0 in every stored sample
    /// around the probe, while generation_tokens_total advances 50-534 tokens per step. Also
    /// covers a clean reading left untouched, and "not enough stored samples to say anything"
    /// being safely skipped rather than guessed at. (A GPU cold-clock signal was tried alongside
    /// the counter here and reverted the same day - see `validate()`'s own doc comment; it is not
    /// part of this repair.)
    #[test]
    fn probes_the_old_idle_gate_missed_are_found_from_the_stored_samples() {
        use lss_core::prom::ServeMetrics;
        let db = Db::memory();
        let sample_at = |ts: i64, gen_tokens: f64| Sample { ts, serve_up: true, metrics: Some(ServeMetrics { running: 0.0, generation_tokens_total: gen_tokens, ..Default::default() }), gpus_ok: true, ..Default::default() };
        let probe = |ts: i64| lss_core::probe::ProbeRecord { ts, status: "ok".into(), ttft_ms: Some(90.0), decode_tok_s: Some(190.0), tokens: Some(128), detail: String::new(), http_status: Some(200), valid: true, invalid_reason: None, unverified: false };

        // 08:45: another client's 288 tokens landed entirely inside the probe's own window
        db.insert_probe(&probe(1000)).unwrap();
        db.insert_sample(&sample_at(998, 48_213.0)).unwrap();
        db.insert_sample(&sample_at(1002, 48_213.0 + 288.0)).unwrap();

        // a clean reading: the counter grows by exactly the probe's own answer
        db.insert_probe(&probe(2000)).unwrap();
        db.insert_sample(&sample_at(1998, 90_000.0)).unwrap();
        db.insert_sample(&sample_at(2002, 90_128.0)).unwrap();

        // nothing stored anywhere near this one: left alone, never guessed at
        db.insert_probe(&probe(9000)).unwrap();

        // 18:27, live: the contaminating sample sat RIGHT ON the window's upper edge - the first
        // version of this window (`+2` slack) excluded it by one second and missed this exact row
        db.insert_probe(&probe(5000)).unwrap();
        db.insert_sample(&sample_at(4998, 1_000.0)).unwrap();
        db.insert_sample(&sample_at(5003, 1_000.0 + 300.0)).unwrap();

        // 01:13:44, live (78.6 tok/s, the real numbers): only ONE sample fell inside the previous
        // window at all (before and after both landed a full 5 s away, poll-phase dependent) - not
        // a boundary bug this time, just not enough room. Needs the full poll-interval slack.
        db.insert_probe(&lss_core::probe::ProbeRecord { decode_tok_s: Some(78.6), ..probe(6000) }).unwrap();
        db.insert_sample(&sample_at(5995, 1_809_956.0)).unwrap();
        db.insert_sample(&sample_at(6005, 1_809_956.0 + 567.0)).unwrap();

        let repaired = db.repair_probes_the_gauge_missed(5).unwrap();
        assert_eq!(repaired.iter().map(|(ts, reason, _)| (*ts, *reason)).collect::<Vec<_>>(), vec![(1000, "contended"), (5000, "contended"), (6000, "contended")], "{repaired:?}");
        let detail_for = |ts: i64| repaired.iter().find(|(t, ..)| *t == ts).unwrap().2.clone();
        assert!(detail_for(1000).contains("grew by 288") && detail_for(1000).contains("160 more") && detail_for(1000).contains("128 tokens"), "{}", detail_for(1000));
        assert!(detail_for(5000).contains("grew by 300"), "{}", detail_for(5000));
        assert!(detail_for(6000).contains("grew by 567"), "{}", detail_for(6000));

        let rows: HashMap<i64, bool> = db.recent_probes(50).unwrap().into_iter().map(|p| (p.ts, p.valid)).collect();
        assert_eq!((rows[&1000], rows[&2000], rows[&5000], rows[&6000], rows[&9000]), (false, true, false, false, true), "{rows:?}");
        // running it again finds nothing new: safe at every start
        assert!(db.repair_probes_the_gauge_missed(5).unwrap().is_empty());
    }

    /// #45, 2026-09-20: the GPU cold-clock check shipped, then was shown wrong and removed the
    /// same day - but a repair only ever marks MORE rows invalid, never un-invalidates, so the
    /// rows it flagged before removal need an explicit correction or they stay wrongly invalid
    /// forever. A row invalidated for a DIFFERENT, still-correct reason (contended) must survive
    /// untouched - this only undoes the one specific mechanism that was wrong.
    #[test]
    fn the_reverted_cold_clock_checks_false_invalidations_are_undone() {
        use lss_core::loadout::identity;
        let db = Db::memory();
        let ok = |ts: i64| lss_core::probe::ProbeRecord { ts, status: "ok".into(), ttft_ms: Some(90.0), decode_tok_s: Some(190.0), tokens: Some(128), detail: String::new(), http_status: Some(200), valid: true, invalid_reason: None, unverified: false };
        let mut cold_clock_victim = ok(1000);
        cold_clock_victim.invalidate("cold_clock", "GPU at 180 MHz (idle/power-save) right before the probe began: the clock ramp lands in the timing, not just the answer".into());
        db.insert_probe(&cold_clock_victim).unwrap();
        let mut really_contended = ok(2000);
        really_contended.invalidate("contended", "running=3 queue=0 right after the probe".into());
        db.insert_probe(&really_contended).unwrap();
        db.insert_probe(&ok(3000)).unwrap();

        let restored = db.undo_cold_clock_false_invalidations().unwrap();
        assert_eq!(restored, vec![1000]);
        let rows: HashMap<i64, (bool, Option<String>)> = db.recent_probes(50).unwrap().into_iter().map(|p| (p.ts, (p.valid, p.invalid_reason))).collect();
        assert_eq!(rows[&1000], (true, None), "the wrongly-invalidated row is valid again, reason cleared");
        assert_eq!(rows[&2000], (false, Some("contended".to_string())), "a row invalid for a real, unrelated reason is untouched");
        assert_eq!(rows[&3000], (true, None));
        // the loadout's C1 figures now count the restored row too
        let id = identity("glm", "img:1", &["python3".to_string()], &[]);
        let mut acc = LoadoutAcc::default();
        for p in db.probes_between(0, i64::MAX).unwrap() {
            acc.observe_probe(&p);
        }
        db.save_loadout(&id, &acc, 0).unwrap();
        db.undo_cold_clock_false_invalidations().unwrap(); // a no-op the second time, but exercises the rebuild path again
        let (fixed, _) = db.load_loadout(&id.id).unwrap().expect("loadout");
        assert_eq!(fixed.c1_probes, 2, "1000 and 3000: both valid, 2000 stays excluded");
        // running it again finds nothing left to restore: safe at every start
        assert!(db.undo_cold_clock_false_invalidations().unwrap().is_empty());
    }
}
