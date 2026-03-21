//! Persistent SQLite telemetry for sniper rounds (backtesting / analysis). Not on the trading critical path.
//!
//! [`StrategyLogger`] keeps `db_path` + `session_id` and opens a short-lived `rusqlite::Connection` per
//! call so the handle stays `Send` and can live in `Arc<Mutex<_>>` across async tasks (`Connection` is not `Send`).

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
"#;

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
        conn.execute_batch(SCHEMA).context("strategy DB schema")?;

        Ok(Self {
            db_path,
            session_id: session_id.to_string(),
        })
    }

    fn open(&self) -> Result<Connection> {
        let conn = Connection::open(&self.db_path).with_context(|| format!("open {:?}", self.db_path))?;
        conn.execute_batch("PRAGMA journal_mode=WAL;")
            .context("PRAGMA journal_mode=WAL")?;
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
