//! Telegram notifications and long-poll command handler (reqwest only; no teloxide).

use crate::balance::fetch_usdc_balance;
use crate::execution::ExecutionEngine;
use crate::types::{Coin, Period, RoundHistoryEntry, RoundHistoryStatus};
use polymarket_client_sdk::types::Address;
use reqwest::Client;
use rust_decimal::prelude::ToPrimitive;
use serde::Deserialize;
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::time::{sleep, Duration};
use tracing::{error, warn};

/// Shared dashboard counters and hooks (Telegram `/dashboard`, `send_round_resolution`, sniper momentum).
#[derive(Clone)]
pub struct TelegramDashboardState {
    pub last_resolution: Arc<Mutex<Option<(String, Instant)>>>,
    pub momentum_blocks: Arc<AtomicU64>,
    pub last_momentum_block: Arc<Mutex<Option<String>>>,
}

impl Default for TelegramDashboardState {
    fn default() -> Self {
        Self {
            last_resolution: Arc::new(Mutex::new(None)),
            momentum_blocks: Arc::new(AtomicU64::new(0)),
            last_momentum_block: Arc::new(Mutex::new(None)),
        }
    }
}

/// Latest session id from `rounds.timestamp_utc` ordering (legacy fallback when no log files).
pub fn query_latest_session_id(db_path: &str) -> Result<Option<String>, rusqlite::Error> {
    use rusqlite::OptionalExtension;
    let conn = rusqlite::Connection::open(db_path)?;
    conn.query_row(
        "SELECT session_id FROM rounds ORDER BY timestamp_utc DESC LIMIT 1",
        [],
        |row| row.get(0),
    )
    .optional()
}

/// Session id from the newest `polymarket-bot.<session>.log` under `logs_dir` (by file mtime), matching `main.rs`.
pub fn session_id_from_newest_bot_log(logs_dir: &str) -> Option<String> {
    let dir = Path::new(logs_dir);
    let entries = fs::read_dir(dir).ok()?;
    let prefix = "polymarket-bot.";
    let suffix = ".log";
    let mut best: Option<(std::time::SystemTime, String)> = None;
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if !name.starts_with(prefix) || !name.ends_with(suffix) {
            continue;
        }
        let sid = name[prefix.len()..name.len() - suffix.len()].to_string();
        let meta = e.metadata().ok()?;
        let mtime = meta.modified().ok()?;
        match &best {
            None => best = Some((mtime, sid)),
            Some((t, _)) if mtime > *t => best = Some((mtime, sid)),
            _ => {}
        }
    }
    best.map(|(_, s)| s)
}

/// Which session supervisor `/stats` and `/pnl` should use: newest bot log (current run), else latest row in DB.
pub fn resolve_session_id_for_supervisor(
    db_path: &str,
    logs_dir: &str,
) -> Result<Option<String>, rusqlite::Error> {
    if let Some(sid) = session_id_from_newest_bot_log(logs_dir) {
        return Ok(Some(sid));
    }
    query_latest_session_id(db_path)
}

/// Outcome counts + total P&amp;L for one `session_id` (same query as `/stats`).
pub struct SessionStatsSnapshot {
    pub won: i64,
    pub lost: i64,
    pub hedged: i64,
    pub tie: i64,
    pub total_pnl: f64,
}

pub fn query_session_stats(
    db_path: &str,
    session_id: &str,
) -> Result<SessionStatsSnapshot, String> {
    use rusqlite::params;
    let conn = rusqlite::Connection::open(db_path).map_err(|e| e.to_string())?;
    let (won, lost, hedged, tie, total_pnl): (i64, i64, i64, i64, f64) = conn
        .query_row(
            r#"
            SELECT
              COALESCE(SUM(CASE WHEN outcome = 'WON' THEN 1 ELSE 0 END), 0),
              COALESCE(SUM(CASE WHEN outcome = 'LOST' THEN 1 ELSE 0 END), 0),
              COALESCE(SUM(CASE WHEN outcome = 'HEDGED' THEN 1 ELSE 0 END), 0),
              COALESCE(SUM(CASE WHEN outcome = 'TIE' THEN 1 ELSE 0 END), 0),
              COALESCE(SUM(pnl), 0)
            FROM rounds WHERE session_id = ?1
            "#,
            params![session_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
        )
        .map_err(|e| e.to_string())?;
    Ok(SessionStatsSnapshot {
        won,
        lost,
        hedged,
        tie,
        total_pnl,
    })
}

/// Session P&amp;L sum and row count (same as `/pnl`).
pub fn query_session_pnl(
    db_path: &str,
    session_id: &str,
) -> Result<(f64, i64), String> {
    use rusqlite::params;
    let conn = rusqlite::Connection::open(db_path).map_err(|e| e.to_string())?;
    conn
        .query_row(
            "SELECT COALESCE(SUM(pnl), 0), COUNT(*) FROM rounds WHERE session_id = ?1",
            params![session_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|e| e.to_string())
}

/// Wallet for Telegram `/balance` and dashboard balance line: `TELEGRAM_BALANCE_ADDRESS`, else `FUNDER_ADDRESS` / `FUNDER` (never hardcode addresses in repo).
pub fn telegram_balance_wallet_from_env() -> Result<Address, String> {
    let s = std::env::var("TELEGRAM_BALANCE_ADDRESS")
        .or_else(|_| std::env::var("FUNDER_ADDRESS"))
        .or_else(|_| std::env::var("FUNDER"))
        .map_err(|_| {
            "Set TELEGRAM_BALANCE_ADDRESS or FUNDER_ADDRESS (or FUNDER) for Telegram /balance".to_string()
        })?;
    let s = s.trim();
    if s.is_empty() {
        return Err("TELEGRAM_BALANCE_ADDRESS / FUNDER is empty".to_string());
    }
    Address::from_str(s).map_err(|e| format!("Invalid balance wallet address: {e}"))
}

#[derive(Clone)]
pub struct TelegramBot {
    inner: Arc<TelegramBotInner>,
}

struct TelegramBotInner {
    client: Client,
    token: String,
    chat_id: String,
    enabled: bool,
    /// Base `TELEGRAM_WEBAPP_PUBLIC_URL` + `/telegram-positions` when Mini App is configured.
    web_app_positions_url: Option<String>,
    /// Base + `/telegram-dashboard` for live dashboard Mini App.
    web_app_dashboard_url: Option<String>,
    dashboard: TelegramDashboardState,
}

struct TelegramReply {
    text: String,
    reply_markup: Option<serde_json::Value>,
}

/// Bot token and chat id (same resolution as `TelegramBot::new`).
pub fn telegram_auth_from_env() -> (String, String) {
    (
        std::env::var("TELEGRAM_BOT_TOKEN").unwrap_or_default(),
        std::env::var("TELEGRAM_CHAT_ID").unwrap_or_default(),
    )
}

/// Join `TELEGRAM_WEBAPP_PUBLIC_URL` with a path segment. If the base URL contains `?query`,
/// the path is inserted before the query (avoids `host?x=/path` mistakes).
fn join_webapp_base_path(base: &str, path: &str) -> String {
    let path = path.trim_start_matches('/');
    if let Some((base_no_q, q)) = base.split_once('?') {
        let base_trim = base_no_q.trim_end_matches('/');
        format!("{}/{}?{}", base_trim, path, q)
    } else {
        let base_trim = base.trim_end_matches('/');
        format!("{}/{}", base_trim, path)
    }
}

/// HTTPS URL for the live dashboard Mini App (`/telegram-dashboard`), if `TELEGRAM_WEBAPP_PUBLIC_URL` is set.
/// Used by the supervisor `/dashboard` command to attach a WebApp button (same URL as `polymarket-bot` startup).
pub fn webapp_dashboard_url_from_env() -> Option<String> {
    let base = std::env::var("TELEGRAM_WEBAPP_PUBLIC_URL").ok()?;
    let b = base.trim().trim_end_matches('/');
    if b.is_empty() {
        return None;
    }
    Some(join_webapp_base_path(b, "telegram-dashboard"))
}

fn webapp_base_looks_like_ngrok(base: &str) -> bool {
    base.to_ascii_lowercase().contains("ngrok")
}

impl TelegramBot {
    pub fn new(dashboard: TelegramDashboardState) -> Self {
        let token = std::env::var("TELEGRAM_BOT_TOKEN").unwrap_or_default();
        let chat_id = std::env::var("TELEGRAM_CHAT_ID").unwrap_or_default();

        let enabled = !token.trim().is_empty() && !chat_id.trim().is_empty();

        let base = std::env::var("TELEGRAM_WEBAPP_PUBLIC_URL")
            .ok()
            .map(|s| s.trim().trim_end_matches('/').to_string())
            .filter(|s| !s.is_empty());
        let web_app_positions_url = base
            .as_ref()
            .map(|b| join_webapp_base_path(b, "telegram-positions"));
        let web_app_dashboard_url = base.map(|b| join_webapp_base_path(&b, "telegram-dashboard"));

        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .connect_timeout(std::time::Duration::from_secs(15))
            .build()
            .unwrap_or_else(|e| {
                error!(error = %e, "Telegram reqwest client build failed; using minimal client");
                Client::new()
            });

        Self {
            inner: Arc::new(TelegramBotInner {
                client,
                token,
                chat_id,
                enabled,
                web_app_positions_url,
                web_app_dashboard_url,
                dashboard,
            }),
        }
    }

    pub fn send_message(&self, text: &str) {
        if !self.inner.enabled {
            return;
        }
        let inner = self.inner.clone();
        let text = text.to_string();
        tokio::spawn(async move {
            let url = format!(
                "https://api.telegram.org/bot{}/sendMessage",
                inner.token
            );
            let body = serde_json::json!({
                "chat_id": inner.chat_id,
                "text": text,
                "parse_mode": "HTML",
            });
            match inner.client.post(&url).json(&body).send().await {
                Ok(resp) => {
                    if !resp.status().is_success() {
                        let status = resp.status();
                        let err_body = resp.text().await.unwrap_or_default();
                        warn!(
                            status = %status,
                            body = %err_body,
                            "Telegram sendMessage failed"
                        );
                    }
                }
                Err(e) => warn!(error = %e, "Telegram sendMessage request error"),
            }
        });
    }

    /// One-shot message after the trading bot starts: Mini App buttons when `TELEGRAM_WEBAPP_PUBLIC_URL` is set.
    pub fn send_startup_message(&self, session_id: &str) {
        if !self.inner.enabled {
            return;
        }
        let inner = self.inner.clone();
        let session_id = session_id.to_string();
        tokio::spawn(async move {
            let has_mini_app = inner.web_app_dashboard_url.is_some()
                || inner.web_app_positions_url.is_some();
            let mut text = format!(
                "🟢 <b>Polymarket bot started</b>\n\
                 Session: <code>{}</code>\n\n",
                session_id
            );
            if has_mini_app {
                text.push_str(
                    "Use the buttons below for the <b>live dashboard</b> inside Telegram (opens a web view; it refreshes automatically).\n\n",
                );
                let any_ngrok = inner
                    .web_app_dashboard_url
                    .as_deref()
                    .or(inner.web_app_positions_url.as_deref())
                    .map(webapp_base_looks_like_ngrok)
                    .unwrap_or(false);
                if any_ngrok {
                    text.push_str(
                        "<i>Free ngrok</i> shows a one-time “Visit Site” screen in the Mini App — tap it once; ngrok then remembers for several days. Paid ngrok or Cloudflare Tunnel skips that screen.\n\n",
                    );
                }
            } else {
                text.push_str(
                    "<b>Mini App buttons</b> are off until you set <code>TELEGRAM_WEBAPP_PUBLIC_URL</code> to a public <b>HTTPS</b> URL that reaches this bot’s HTTP server (<code>TELEGRAM_WEBAPP_BIND</code>). \
                     On a PC, use ngrok or Cloudflare Tunnel to expose the bind.\n\n",
                );
            }
            text.push_str("Text commands: /help · /dashboard · /positions · /status");
            let url = format!(
                "https://api.telegram.org/bot{}/sendMessage",
                inner.token
            );
            let reply_markup = web_app_keyboard(
                inner.web_app_positions_url.as_deref(),
                inner.web_app_dashboard_url.as_deref(),
            );
            let mut body = serde_json::json!({
                "chat_id": inner.chat_id,
                "text": text,
                "parse_mode": "HTML",
                "disable_web_page_preview": true,
            });
            if let Some(m) = reply_markup {
                body["reply_markup"] = m;
            }
            match inner.client.post(&url).json(&body).send().await {
                Ok(resp) => {
                    if !resp.status().is_success() {
                        let status = resp.status();
                        let err_body = resp.text().await.unwrap_or_default();
                        warn!(
                            status = %status,
                            body = %err_body,
                            "Telegram startup sendMessage failed"
                        );
                    }
                }
                Err(e) => warn!(error = %e, "Telegram startup sendMessage request error"),
            }
        });
    }

    pub fn send_round_resolution(
        &self,
        coin: &str,
        period: &str,
        side: &str,
        entry_price: f64,
        outcome: &str,
        pnl: f64,
        chainlink_open: f64,
        chainlink_close: f64,
        time_remaining_secs: u64,
    ) {
        let prefix = match outcome.to_uppercase().as_str() {
            "WON" => "✅ WON",
            "HEDGED" => "🔄 HEDGED",
            "LOST" => "❌ LOST",
            "TIE" => "➖ TIE",
            _ => "➖ TIE",
        };
        let coin_u = coin.to_uppercase();
        let pnl_sign = if pnl >= 0.0 { "+" } else { "-" };
        let msg = format!(
            "{prefix} | {coin_u} {period} {side}\n\
             Entry: ${entry:.4} | P&L: {pnl_sign}${pnl_abs:.2}\n\
             Chainlink: {open:.2} → {close:.2}\n\
             Entered with {tr}s left",
            prefix = prefix,
            coin_u = coin_u,
            period = period,
            side = side,
            entry = entry_price,
            pnl_sign = pnl_sign,
            pnl_abs = pnl.abs(),
            open = chainlink_open,
            close = chainlink_close,
            tr = time_remaining_secs,
        );
        let emoji = match outcome.to_uppercase().as_str() {
            "WON" => "✅",
            "HEDGED" => "🔄",
            "LOST" => "❌",
            "TIE" => "➖",
            _ => "➖",
        };
        let outcome_u = outcome.to_uppercase();
        let summary_line = format!(
            "{emoji} {coin_u} {period} {side} — {outcome_u} {pnl_sign}${pnl_abs:.2}",
            emoji = emoji,
            coin_u = coin_u,
            period = period,
            side = side,
            outcome_u = outcome_u,
            pnl_sign = pnl_sign,
            pnl_abs = pnl.abs(),
        );
        if let Ok(mut g) = self.inner.dashboard.last_resolution.lock() {
            *g = Some((summary_line, Instant::now()));
        }
        self.send_message(&msg);
    }

    pub fn start_polling(
        &self,
        rpc_url: String,
        strategy_db_path: String,
        session_id: String,
        round_history: Arc<Mutex<Vec<RoundHistoryEntry>>>,
        trading_paused: Arc<AtomicBool>,
        dry_run: Arc<AtomicBool>,
        execution: Arc<ExecutionEngine>,
        strategy_mode_label: String,
        session_start: chrono::DateTime<chrono::Utc>,
    ) {
        let inner = self.inner.clone();
        if !inner.enabled {
            return;
        }
        tokio::spawn(async move {
            let mut offset: i64 = 0;
            let mut seen_update_ids: HashSet<i64> = HashSet::new();
            const SEEN_CAP: usize = 2048;
            loop {
                let url = format!(
                    "https://api.telegram.org/bot{}/getUpdates",
                    inner.token
                );
                let body = serde_json::json!({
                    "offset": offset,
                    "timeout": 30,
                    "allowed_updates": ["message", "edited_message"],
                });
                let result = inner.client.post(&url).json(&body).send().await;
                match result {
                    Ok(resp) => {
                        if !resp.status().is_success() {
                            warn!(status = %resp.status(), "Telegram getUpdates HTTP error");
                            sleep(Duration::from_secs(5)).await;
                            continue;
                        }
                        let parse: Result<TgGetUpdatesResponse, _> = resp.json().await;
                        match parse {
                            Ok(upd) => {
                                if !upd.ok {
                                    warn!("Telegram getUpdates ok=false");
                                    sleep(Duration::from_secs(5)).await;
                                    continue;
                                }
                                let mut max_id: i64 = -1;
                                for u in upd.result {
                                    max_id = max_id.max(u.update_id);
                                    if seen_update_ids.len() > SEEN_CAP {
                                        seen_update_ids.clear();
                                    }
                                    if !seen_update_ids.insert(u.update_id) {
                                        warn!(
                                            id = u.update_id,
                                            "Telegram duplicate update_id skipped"
                                        );
                                        continue;
                                    }
                                    let msg = match u.message.or(u.edited_message) {
                                        Some(m) => m,
                                        None => continue,
                                    };
                                    let chat_ok = msg
                                        .chat
                                        .as_ref()
                                        .and_then(|c| c.id)
                                        .map(|id| id.to_string() == inner.chat_id)
                                        .unwrap_or(false);
                                    if !chat_ok {
                                        continue;
                                    }
                                    let text = msg.text.unwrap_or_default();
                                    let reply = match handle_command(
                                        text.trim(),
                                        &rpc_url,
                                        &strategy_db_path,
                                        &session_id,
                                        &round_history,
                                        inner.web_app_positions_url.as_deref(),
                                        inner.web_app_dashboard_url.as_deref(),
                                        &trading_paused,
                                        &dry_run,
                                        &execution,
                                        &strategy_mode_label,
                                        session_start,
                                        &inner.dashboard,
                                    )
                                    .await
                                    {
                                        Ok(s) => s,
                                        Err(e) => {
                                            error!(error = %e, "Telegram command handler error");
                                            TelegramReply {
                                                text: format!("Error: {}", e),
                                                reply_markup: None,
                                            }
                                        }
                                    };
                                    let send_inner = inner.clone();
                                    tokio::spawn(async move {
                                        let url = format!(
                                            "https://api.telegram.org/bot{}/sendMessage",
                                            send_inner.token
                                        );
                                        let body = if let Some(markup) = reply.reply_markup {
                                            serde_json::json!({
                                                "chat_id": send_inner.chat_id,
                                                "text": reply.text,
                                                "parse_mode": "HTML",
                                                "reply_markup": markup,
                                            })
                                        } else {
                                            serde_json::json!({
                                                "chat_id": send_inner.chat_id,
                                                "text": reply.text,
                                                "parse_mode": "HTML",
                                            })
                                        };
                                        if let Err(e) =
                                            send_inner.client.post(&url).json(&body).send().await
                                        {
                                            warn!(error = %e, "Telegram reply send failed");
                                        }
                                    });
                                }
                                if max_id >= 0 {
                                    offset = max_id + 1;
                                }
                            }
                            Err(e) => {
                                warn!(error = %e, "Telegram getUpdates JSON decode error");
                                sleep(Duration::from_secs(5)).await;
                            }
                        }
                    }
                    Err(e) => {
                        warn!(error = %e, "Telegram getUpdates network error");
                        sleep(Duration::from_secs(5)).await;
                    }
                }
            }
        });
    }
}

#[derive(Debug, Deserialize)]
struct TgGetUpdatesResponse {
    ok: bool,
    #[serde(default)]
    result: Vec<TgUpdate>,
}

#[derive(Debug, Deserialize)]
struct TgUpdate {
    update_id: i64,
    #[serde(default)]
    message: Option<TgMessage>,
    #[serde(default)]
    edited_message: Option<TgMessage>,
}

#[derive(Debug, Deserialize)]
struct TgMessage {
    text: Option<String>,
    chat: Option<TgChat>,
}

#[derive(Debug, Deserialize)]
struct TgChat {
    id: Option<i64>,
}

/// First command token, without `@BotName` (e.g. `/balance@MyBot` → `/balance`).
fn normalize_command(text: &str) -> String {
    text.trim()
        .split_whitespace()
        .next()
        .unwrap_or("")
        .split('@')
        .next()
        .unwrap_or("")
        .to_lowercase()
}

async fn handle_command(
    text: &str,
    rpc_url: &str,
    strategy_db_path: &str,
    session_id: &str,
    round_history: &Arc<Mutex<Vec<RoundHistoryEntry>>>,
    web_app_positions_url: Option<&str>,
    web_app_dashboard_url: Option<&str>,
    trading_paused: &Arc<AtomicBool>,
    dry_run: &Arc<AtomicBool>,
    execution: &Arc<ExecutionEngine>,
    strategy_mode_label: &str,
    session_start: chrono::DateTime<chrono::Utc>,
    dashboard: &TelegramDashboardState,
) -> Result<TelegramReply, String> {
    let key = normalize_command(text);
    if key == "/balance" {
        return cmd_balance(rpc_url).await;
    }
    if key == "/pnl" {
        return cmd_pnl(strategy_db_path, session_id);
    }
    if key == "/positions" {
        return cmd_positions(round_history, web_app_positions_url, web_app_dashboard_url);
    }
    if key == "/stats" {
        return cmd_stats(strategy_db_path, session_id);
    }
    if key == "/dashboard" {
        return cmd_dashboard(
            rpc_url,
            strategy_db_path,
            session_id,
            round_history,
            web_app_positions_url,
            web_app_dashboard_url,
            trading_paused,
            dry_run,
            strategy_mode_label,
            session_start,
            dashboard,
        )
        .await;
    }
    if key == "/pause" {
        trading_paused.store(true, Ordering::Relaxed);
        let exec = execution.clone();
        tokio::spawn(async move {
            if let Err(e) = exec.cancel_all().await {
                error!(error = %e, "Telegram /pause: cancel_all failed");
            }
        });
        return Ok(TelegramReply {
            text: "⏸ Trading paused. Open orders cancelled.".to_string(),
            reply_markup: None,
        });
    }
    if key == "/resume" {
        trading_paused.store(false, Ordering::Relaxed);
        return Ok(TelegramReply {
            text: "▶ Trading resumed.".to_string(),
            reply_markup: None,
        });
    }
    if key == "/status" {
        return Ok(cmd_status(
            trading_paused,
            dry_run,
            strategy_mode_label,
            session_start,
        ));
    }
    if key == "/help" || key == "/start" {
        return Ok(TelegramReply {
            text: help_text(),
            reply_markup: web_app_keyboard(web_app_positions_url, web_app_dashboard_url),
        });
    }
    if key.starts_with('/') {
        return Ok(TelegramReply {
            text: "Unknown command. Try /help".to_string(),
            reply_markup: None,
        });
    }
    Ok(TelegramReply {
        text: "Unknown command. Try /help".to_string(),
        reply_markup: None,
    })
}

fn cmd_status(
    trading_paused: &Arc<AtomicBool>,
    dry_run: &Arc<AtomicBool>,
    strategy_mode_label: &str,
    session_start: chrono::DateTime<chrono::Utc>,
) -> TelegramReply {
    let paused = trading_paused.load(Ordering::Relaxed);
    let dr = dry_run.load(Ordering::Relaxed);
    let trade_line = if paused {
        "Trading: <b>PAUSED</b> (no new orders)"
    } else {
        "Trading: <b>ACTIVE</b>"
    };
    let dry_line = if dr {
        "Orders: <b>DRY RUN</b> (simulated)"
    } else {
        "Orders: <b>LIVE</b>"
    };
    let uptime = (chrono::Utc::now() - session_start)
        .num_seconds()
        .max(0);
    let h = uptime / 3600;
    let m = (uptime % 3600) / 60;
    let s = uptime % 60;
    TelegramReply {
        text: format!(
            "📡 <b>Status</b>\n\
             {trade_line}\n\
             {dry_line}\n\
             Strategy mode: <code>{strategy_mode_label}</code>\n\
             Uptime: {h}h {m}m {s}s",
        ),
        reply_markup: None,
    }
}

fn web_app_keyboard(
    positions_url: Option<&str>,
    dashboard_url: Option<&str>,
) -> Option<serde_json::Value> {
    let mut rows: Vec<Vec<serde_json::Value>> = Vec::new();
    if let (Some(p), Some(d)) = (positions_url, dashboard_url) {
        rows.push(vec![
            serde_json::json!({"text": "⏱ Positions", "web_app": {"url": p}}),
            serde_json::json!({"text": "📊 Live dashboard", "web_app": {"url": d}}),
        ]);
    } else if let Some(p) = positions_url {
        rows.push(vec![serde_json::json!({"text": "⏱ Positions", "web_app": {"url": p}})]);
    } else if let Some(d) = dashboard_url {
        rows.push(vec![serde_json::json!({"text": "📊 Live dashboard", "web_app": {"url": d}})]);
    }
    if rows.is_empty() {
        return None;
    }
    Some(serde_json::json!({"inline_keyboard": rows}))
}

async fn cmd_balance(rpc_url: &str) -> Result<TelegramReply, String> {
    let addr = telegram_balance_wallet_from_env()?;
    let bal = fetch_usdc_balance(rpc_url, addr)
        .await
        .map_err(|e| e.to_string())?;
    let v = bal.to_f64().unwrap_or(0.0);
    Ok(TelegramReply {
        text: format!("💰 Balance: ${:.2} USDC", v),
        reply_markup: None,
    })
}

fn cmd_pnl(db_path: &str, session_id: &str) -> Result<TelegramReply, String> {
    let (sum, n) = query_session_pnl(db_path, session_id)?;
    let sign = if sum >= 0.0 { "+" } else { "-" };
    Ok(TelegramReply {
        text: format!(
            "📊 Session P&L: {sign}${amt:.2} ({n} trades)",
            sign = sign,
            amt = sum.abs(),
            n = n
        ),
        reply_markup: None,
    })
}

fn cmd_positions(
    round_history: &Arc<Mutex<Vec<RoundHistoryEntry>>>,
    web_app_url: Option<&str>,
    dashboard_url: Option<&str>,
) -> Result<TelegramReply, String> {
    let guard = round_history.lock().map_err(|e| e.to_string())?;
    let now = chrono::Utc::now().timestamp();
    let mut lines: Vec<String> = Vec::new();
    for e in guard.iter() {
        if !matches!(e.status, RoundHistoryStatus::Open) {
            continue;
        }
        let rem = (e.round_end - now).max(0);
        let ep = e.entry_price.to_f64().unwrap_or(0.0);
        lines.push(format!(
            "{} {} {} @ {:.4} ({rem}s left)",
            coin_display(e.coin),
            period_display(e.period),
            e.side_label,
            ep,
            rem = rem,
        ));
    }
    let text = if lines.is_empty() {
        "No open positions.".to_string()
    } else {
        lines.join("\n")
    };
    let reply_markup = web_app_keyboard(web_app_url, dashboard_url);
    Ok(TelegramReply {
        text,
        reply_markup,
    })
}

fn coin_display(c: Coin) -> String {
    c.as_str().to_uppercase()
}

fn period_display(p: Period) -> String {
    format!("{}m", p.as_minutes())
}

fn cmd_stats(db_path: &str, session_id: &str) -> Result<TelegramReply, String> {
    let s = query_session_stats(db_path, session_id)?;
    let wr = if s.won + s.lost > 0 {
        (s.won as f64 / (s.won + s.lost) as f64) * 100.0
    } else {
        0.0
    };
    let pnl_sign = if s.total_pnl >= 0.0 { "+" } else { "-" };
    Ok(TelegramReply {
        text: format!(
            "📈 Session Stats\n\
             WON: {won} | LOST: {lost} | HEDGED: {hedged} | TIE: {tie}\n\
             Win Rate: {wr:.1}%\n\
             Total P&L: {pnl_sign}${tp:.2}",
            won = s.won,
            lost = s.lost,
            hedged = s.hedged,
            tie = s.tie,
            wr = wr,
            pnl_sign = pnl_sign,
            tp = s.total_pnl.abs(),
        ),
        reply_markup: None,
    })
}

fn format_uptime_short_hm(uptime_secs: i64) -> String {
    let h = uptime_secs / 3600;
    let m = (uptime_secs % 3600) / 60;
    if uptime_secs < 60 {
        format!("{}s", uptime_secs)
    } else if h > 0 {
        format!("{}h {}m", h, m)
    } else {
        format!("{}m", m)
    }
}

fn format_relative_since(at: Instant) -> String {
    let secs = at.elapsed().as_secs();
    if secs < 60 {
        format!("{} seconds ago", secs)
    } else if secs < 3600 {
        format!("{} minutes ago", secs / 60)
    } else if secs < 86400 {
        format!("{} hours ago", secs / 3600)
    } else {
        format!("{} days ago", secs / 86400)
    }
}

fn format_positions_tree_dashboard(
    round_history: &Arc<Mutex<Vec<RoundHistoryEntry>>>,
) -> Result<String, String> {
    let guard = round_history.lock().map_err(|e| e.to_string())?;
    let now = chrono::Utc::now().timestamp();
    let mut lines: Vec<String> = Vec::new();
    for e in guard.iter() {
        if !matches!(e.status, RoundHistoryStatus::Open) {
            continue;
        }
        let rem = (e.round_end - now).max(0);
        let ep = e.entry_price.to_f64().unwrap_or(0.0);
        lines.push(format!(
            "{} {} {} @ ${:.2} — {}s left",
            coin_display(e.coin),
            period_display(e.period),
            e.side_label,
            ep,
            rem,
        ));
    }
    let n = lines.len();
    if n == 0 {
        return Ok("📍 Open Positions (0)\n└ None".to_string());
    }
    let mut out = format!("📍 Open Positions ({})\n", n);
    for (i, line) in lines.iter().enumerate() {
        let prefix = if i + 1 == lines.len() { "└" } else { "├" };
        out.push_str(&format!("{} {}\n", prefix, line));
    }
    Ok(out.trim_end().to_string())
}

async fn cmd_dashboard(
    rpc_url: &str,
    strategy_db_path: &str,
    session_id: &str,
    round_history: &Arc<Mutex<Vec<RoundHistoryEntry>>>,
    web_app_positions_url: Option<&str>,
    web_app_dashboard_url: Option<&str>,
    trading_paused: &Arc<AtomicBool>,
    dry_run: &Arc<AtomicBool>,
    strategy_mode_label: &str,
    session_start: chrono::DateTime<chrono::Utc>,
    dashboard: &TelegramDashboardState,
) -> Result<TelegramReply, String> {
    let addr = telegram_balance_wallet_from_env()?;
    let bal = fetch_usdc_balance(rpc_url, addr)
        .await
        .map_err(|e| e.to_string())?;
    let bal_f = bal.to_f64().unwrap_or(0.0);

    let uptime_secs = (chrono::Utc::now() - session_start)
        .num_seconds()
        .max(0);
    let uptime_str = format_uptime_short_hm(uptime_secs);

    let paused = trading_paused.load(Ordering::Relaxed);
    let dr = dry_run.load(Ordering::Relaxed);
    let status_line = if paused {
        "⏸ Status: Trading Paused"
    } else if dr {
        "🟡 Status: Trading Active (dry run)"
    } else {
        "🟢 Status: Trading Active"
    };

    let stats_block = match query_session_stats(strategy_db_path, session_id) {
        Ok(s) => {
            let trades_total = s.won + s.lost + s.hedged + s.tie;
            let wr = if s.won + s.lost > 0 {
                (s.won as f64 / (s.won + s.lost) as f64) * 100.0
            } else {
                0.0
            };
            let pnl_sign = if s.total_pnl >= 0.0 { "+" } else { "-" };
            format!(
                "📈 Session Stats\n\
                 ├ Trades: {}\n\
                 ├ Won: {} | Lost: {} | Hedged: {} | Tie: {}\n\
                 ├ Win Rate: {:.1}%\n\
                 └ P&L: {}${:.4}",
                trades_total,
                s.won,
                s.lost,
                s.hedged,
                s.tie,
                wr,
                pnl_sign,
                s.total_pnl.abs(),
            )
        }
        Err(_) => "📈 Session Stats\n└ (no data)".to_string(),
    };

    let positions_text = format_positions_tree_dashboard(round_history)?;

    let blocked = dashboard.momentum_blocks.load(Ordering::Relaxed);
    let last_block = dashboard
        .last_momentum_block
        .lock()
        .ok()
        .and_then(|g| g.clone())
        .unwrap_or_else(|| "—".to_string());

    let momentum_block = format!(
        "🛡 Momentum Filter\n\
         ├ Blocked: {} entries this session\n\
         └ Last block: {}",
        blocked, last_block
    );

    let last_res_block = match dashboard.last_resolution.lock().ok().and_then(|g| g.clone()) {
        Some((line, at)) => {
            let rel = format_relative_since(at);
            format!("🕐 Last Resolution\n├ {}\n└ {}", line, rel)
        }
        None => "🕐 Last Resolution\n└ No resolutions yet".to_string(),
    };

    let text = format!(
        "📊 Polybot Dashboard\n\
         ━━━━━━━━━━━━━━━━━━━━\n\
         \n\
         💰 Balance: ${:.2} USDC\n\
         ⏱ Uptime: {}\n\
         🔄 Mode: {}\n\
         {}\n\
         \n\
         {}\n\
         \n\
         {}\n\
         \n\
         {}\n\
         \n\
         {}",
        bal_f,
        uptime_str,
        strategy_mode_label,
        status_line,
        stats_block,
        positions_text,
        momentum_block,
        last_res_block,
    );

    let reply_markup = web_app_keyboard(web_app_positions_url, web_app_dashboard_url);
    Ok(TelegramReply {
        text,
        reply_markup,
    })
}

fn help_text() -> String {
    "/balance — USDC balance on the proxy wallet (on-chain)\n\
     /pnl — session P&L and trade count from strategy DB\n\
     /positions — open sniper positions; Mini App buttons when TELEGRAM_WEBAPP_PUBLIC_URL is set\n\
     /stats — session outcome counts, win rate, total P&L\n\
     /dashboard — text snapshot (Mini App live view: buttons on bot start when TELEGRAM_WEBAPP_PUBLIC_URL is set)\n\
     /pause — pause trading (blocks new orders; cancels open orders)\n\
     /resume — resume trading after /pause\n\
     /status — trading paused/active, dry run, strategy mode, uptime\n\
     /help — this message\n\
     /start — same as /help (with Mini App buttons if configured)"
        .to_string()
}
