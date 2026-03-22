//! Persistent SQLite telemetry for sniper rounds (backtesting / analysis). Not on the trading critical path.
//! `round_price_ticks` stores per-second Polymarket top-of-book + Binance spot for each active market round.
//!
//! [`StrategyLogger`] keeps `db_path` + `session_id` and opens a short-lived `rusqlite::Connection` per
//! call so the handle stays `Send` and can live in `Arc<Mutex<_>>` across async tasks (`Connection` is not `Send`).
//! [`StrategyLogger::new`] and every connection opened for logging run [`ensure_strategy_schema`] so new tables
//! are applied even if the DB predates a migration or was copied from another host.

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::PathBuf;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS rounds (
    id                    INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id            TEXT NOT NULL,
    timestamp_utc         TEXT NOT NULL,
    coin                  TEXT NOT NULL,
    period                TEXT NOT NULL,
    side                  TEXT NOT NULL,
    round_start           INTEGER NOT NULL,
    round_end             INTEGER NOT NULL,
    entry_price           REAL NOT NULL,
    shares                INTEGER NOT NULL,
    best_ask_at_entry     REAL,
    best_bid_at_entry     REAL,
    binance_spot_at_entry REAL,
    chainlink_open        REAL,
    time_remaining_secs   INTEGER,
    entry_window_secs     INTEGER,
    hedge_price           REAL,
    pair_cost             REAL,
    chainlink_close       REAL,
    outcome               TEXT,
    pnl                   REAL,
    binance_spot_at_close REAL,
    binance_delta_pct     REAL,

    UNIQUE(round_start, coin, period, side)
);

CREATE INDEX IF NOT EXISTS idx_rounds_session ON rounds(session_id);
CREATE INDEX IF NOT EXISTS idx_rounds_coin_period ON rounds(coin, period);
CREATE INDEX IF NOT EXISTS idx_rounds_outcome ON rounds(outcome);

-- One row per UTC second per (session, coin, period, round) while the sniper task is alive.
CREATE TABLE IF NOT EXISTS round_price_ticks (
    id                    INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id            TEXT NOT NULL,
    ts_utc                INTEGER NOT NULL,
    coin                  TEXT NOT NULL,
    period                TEXT NOT NULL,
    round_start           INTEGER NOT NULL,
    round_end             INTEGER NOT NULL,
    best_ask_yes          REAL,
    best_ask_no           REAL,
    best_bid_yes          REAL,
    best_bid_no           REAL,
    binance_spot          REAL,
    time_remaining_secs   INTEGER,

    UNIQUE(session_id, coin, period, round_start, ts_utc)
);

CREATE INDEX IF NOT EXISTS idx_round_price_ticks_session ON round_price_ticks(session_id);
CREATE INDEX IF NOT EXISTS idx_round_price_ticks_lookup ON round_price_ticks(coin, period, round_start, ts_utc);

-- Append-only: why the sniper skipped entry (cap, filters, sanity, sizing, place errors).
CREATE TABLE IF NOT EXISTS sniper_decision_events (
    id                    INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id            TEXT NOT NULL,
    ts_utc                INTEGER NOT NULL,
    ts_ms                 INTEGER NOT NULL DEFAULT 0,
    coin                  TEXT NOT NULL,
    period                TEXT NOT NULL,
    round_start           INTEGER NOT NULL,
    round_end             INTEGER NOT NULL,
    reason_code           TEXT NOT NULL,
    detail                TEXT,
    best_ask_yes          REAL,
    best_ask_no           REAL,
    time_remaining_secs   INTEGER,
    filled_count          INTEGER,
    entry_window_secs     INTEGER
);

CREATE INDEX IF NOT EXISTS idx_sniper_decision_session ON sniper_decision_events(session_id);
CREATE INDEX IF NOT EXISTS idx_sniper_decision_lookup ON sniper_decision_events(coin, period, round_start, ts_utc);
"#;

/// Tables that must exist before skipping `CREATE TABLE IF NOT EXISTS` batch. Add new names when the schema grows.
const REQUIRED_TABLES: &[&str] = &["rounds", "round_price_ticks", "sniper_decision_events"];

fn strategy_schema_complete(conn: &Connection) -> rusqlite::Result<bool> {
    for name in REQUIRED_TABLES {
        let exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
            params![*name],
            |r| r.get(0),
        )?;
        if !exists {
            return Ok(false);
        }
    }
    Ok(true)
}

fn ensure_strategy_schema(conn: &Connection) -> Result<()> {
    if strategy_schema_complete(conn).context("strategy_schema_complete")? {
        return Ok(());
    }
    conn.execute_batch(SCHEMA)
        .context("strategy DB schema (CREATE TABLE / indexes)")?;
    Ok(())
}

pub struct RoundEntry {
    pub timestamp_utc: String,
    pub coin: String,
    pub period: String,
    pub side: String,
    pub round_start: i64,
    pub round_end: i64,
    pub entry_price: f64,
    pub shares: i32,
    pub best_ask_at_entry: Option<f64>,
    pub best_bid_at_entry: Option<f64>,
    pub binance_spot_at_entry: Option<f64>,
    pub chainlink_open: Option<f64>,
    pub time_remaining_secs: Option<i32>,
    pub entry_window_secs: Option<i32>,
}

/// Persistent strategy DB. Opens a short-lived SQLite connection per call so the handle is `Send` and safe to share across async tasks.
pub struct StrategyLogger {
    db_path: PathBuf,
    session_id: String,
}

impl StrategyLogger {
    pub fn new(db_path: &str, session_id: &str) -> Result<Self> {
        let db_path = PathBuf::from(db_path);
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).with_context(|| format!("create_dir_all {:?}", parent))?;
        }
        let conn = Connection::open(&db_path).with_context(|| format!("open {:?}", db_path))?;
        conn.execute_batch("PRAGMA journal_mode=WAL;")
            .context("PRAGMA journal_mode=WAL")?;
        ensure_strategy_schema(&conn)?;

        Ok(Self {
            db_path,
            session_id: session_id.to_string(),
        })
    }

    fn open(&self) -> Result<Connection> {
        let conn = Connection::open(&self.db_path).with_context(|| format!("open {:?}", self.db_path))?;
        conn.execute_batch("PRAGMA journal_mode=WAL;")
            .context("PRAGMA journal_mode=WAL")?;
        ensure_strategy_schema(&conn)?;
        Ok(conn)
    }

    pub fn log_entry(&self, entry: &RoundEntry) -> Result<()> {
        let conn = self.open()?;
        conn.execute(
            r#"
            INSERT OR IGNORE INTO rounds (
                session_id, timestamp_utc, coin, period, side,
                round_start, round_end, entry_price, shares,
                best_ask_at_entry, best_bid_at_entry, binance_spot_at_entry,
                chainlink_open, time_remaining_secs, entry_window_secs,
                hedge_price, pair_cost, chainlink_close, outcome, pnl,
                binance_spot_at_close, binance_delta_pct
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5,
                ?6, ?7, ?8, ?9,
                ?10, ?11, ?12,
                ?13, ?14, ?15,
                NULL, NULL, NULL, NULL, NULL,
                NULL, NULL
            )
            "#,
            params![
                self.session_id.as_str(),
                entry.timestamp_utc.as_str(),
                entry.coin.as_str(),
                entry.period.as_str(),
                entry.side.as_str(),
                entry.round_start,
                entry.round_end,
                entry.entry_price,
                entry.shares,
                entry.best_ask_at_entry,
                entry.best_bid_at_entry,
                entry.binance_spot_at_entry,
                entry.chainlink_open,
                entry.time_remaining_secs,
                entry.entry_window_secs,
            ],
        )
        .context("INSERT OR IGNORE rounds")?;
        Ok(())
    }

    /// Polymarket YES/NO top-of-book + Binance spot, once per UTC second for this round (sniper background task).
    pub fn log_round_price_tick(
        &self,
        ts_utc: i64,
        coin: &str,
        period: &str,
        round_start: i64,
        round_end: i64,
        best_ask_yes: Option<f64>,
        best_ask_no: Option<f64>,
        best_bid_yes: Option<f64>,
        best_bid_no: Option<f64>,
        binance_spot: Option<f64>,
        time_remaining_secs: i64,
    ) -> Result<()> {
        let conn = self.open()?;
        conn.execute(
            r#"
            INSERT OR REPLACE INTO round_price_ticks (
                session_id, ts_utc, coin, period, round_start, round_end,
                best_ask_yes, best_ask_no, best_bid_yes, best_bid_no,
                binance_spot, time_remaining_secs
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
            "#,
            params![
                self.session_id.as_str(),
                ts_utc,
                coin,
                period,
                round_start,
                round_end,
                best_ask_yes,
                best_ask_no,
                best_bid_yes,
                best_bid_no,
                binance_spot,
                time_remaining_secs,
            ],
        )
        .context("INSERT OR REPLACE round_price_ticks")?;
        Ok(())
    }

    /// Append-only row when the sniper skips or fails an entry attempt (see `reason_code`).
    #[allow(clippy::too_many_arguments)]
    pub fn log_sniper_decision(
        &self,
        ts_utc: i64,
        ts_ms: i32,
        coin: &str,
        period: &str,
        round_start: i64,
        round_end: i64,
        reason_code: &str,
        detail: Option<&str>,
        best_ask_yes: Option<f64>,
        best_ask_no: Option<f64>,
        time_remaining_secs: i64,
        filled_count: Option<u32>,
        entry_window_secs: Option<i32>,
    ) -> Result<()> {
        let conn = self.open()?;
        conn.execute(
            r#"
            INSERT INTO sniper_decision_events (
                session_id, ts_utc, ts_ms, coin, period, round_start, round_end,
                reason_code, detail, best_ask_yes, best_ask_no,
                time_remaining_secs, filled_count, entry_window_secs
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
            "#,
            params![
                self.session_id.as_str(),
                ts_utc,
                ts_ms,
                coin,
                period,
                round_start,
                round_end,
                reason_code,
                detail,
                best_ask_yes,
                best_ask_no,
                time_remaining_secs,
                filled_count.map(|u| u as i64),
                entry_window_secs,
            ],
        )
        .context("INSERT sniper_decision_events")?;
        Ok(())
    }

    pub fn log_hedge(
        &self,
        coin: &str,
        period: &str,
        round_start: i64,
        side: &str,
        hedge_price: f64,
        pair_cost: f64,
    ) -> Result<()> {
        let conn = self.open()?;
        conn.execute(
            r#"
            UPDATE rounds SET hedge_price = ?1, pair_cost = ?2
            WHERE round_start = ?3 AND coin = ?4 AND period = ?5 AND side = ?6
            "#,
            params![hedge_price, pair_cost, round_start, coin, period, side],
        )
        .context("UPDATE rounds hedge")?;
        Ok(())
    }

    pub fn log_resolution(
        &self,
        coin: &str,
        period: &str,
        round_start: i64,
        side: &str,
        chainlink_close: f64,
        outcome: &str,
        pnl: f64,
        binance_spot_at_close: Option<f64>,
    ) -> Result<()> {
        let conn = self.open()?;
        let prior_outcome: Option<Option<String>> = conn
            .query_row(
                r#"
                SELECT outcome FROM rounds
                WHERE round_start = ?1 AND coin = ?2 AND period = ?3 AND side = ?4
                "#,
                params![round_start, coin, period, side],
                |row| row.get(0),
            )
            .optional()
            .context("SELECT outcome for idempotency")?;
        if let Some(Some(ref o)) = prior_outcome {
            if !o.is_empty() {
                return Ok(());
            }
        }

        let entry_spot: Option<f64> = conn
            .query_row(
                r#"
                SELECT binance_spot_at_entry FROM rounds
                WHERE round_start = ?1 AND coin = ?2 AND period = ?3 AND side = ?4
                "#,
                params![round_start, coin, period, side],
                |row| row.get(0),
            )
            .optional()
            .context("SELECT binance_spot_at_entry")?;

        let binance_delta_pct = match (entry_spot, binance_spot_at_close) {
            (Some(e), Some(c)) if e != 0.0 => Some((c - e) / e),
            _ => None,
        };

        conn.execute(
            r#"
            UPDATE rounds SET
                chainlink_close = ?1,
                outcome = ?2,
                pnl = ?3,
                binance_spot_at_close = ?4,
                binance_delta_pct = ?5
            WHERE round_start = ?6 AND coin = ?7 AND period = ?8 AND side = ?9
            "#,
            params![
                chainlink_close,
                outcome,
                pnl,
                binance_spot_at_close,
                binance_delta_pct,
                round_start,
                coin,
                period,
                side,
            ],
        )
        .context("UPDATE rounds resolution")?;
        Ok(())
    }

    pub fn flush(&self) -> Result<()> {
        let conn = self.open()?;
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .context("PRAGMA wal_checkpoint(TRUNCATE)")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn sniper_decision_insert_matches_column_count() {
        let p = std::env::temp_dir().join("polymarket_strategy_sniper_decision_test.db");
        let _ = fs::remove_file(&p);
        let path = p.to_str().expect("utf8 temp path");
        let s = StrategyLogger::new(path, "test-session").expect("logger");
        s.log_sniper_decision(
            1,
            0,
            "btc",
            "5m",
            100,
            400,
            "too_early",
            None,
            None,
            None,
            300,
            Some(0),
            Some(60),
        )
        .expect("14 columns / 14 placeholders must match");
        let _ = fs::remove_file(&p);
    }
}
