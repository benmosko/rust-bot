//! Telegram notifications and long-poll command handler (reqwest only; no teloxide).

use crate::balance::fetch_usdc_balance;
use crate::execution::ExecutionEngine;
use crate::types::{Coin, Period, RoundHistoryEntry, RoundHistoryStatus};
use polymarket_client_sdk::types::Address;
use reqwest::Client;
use rust_decimal::prelude::ToPrimitive;
use serde::Deserialize;
use std::collections::HashSet;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::time::{sleep, Duration};
use tracing::{error, warn};

const DEFAULT_BOT_TOKEN: &str = "8669252052:AAFTJ9UeTTixZ2cOtfZ5ySXzBWoYWcs1hSE";
const DEFAULT_CHAT_ID: &str = "5789011208";
/// `/balance` command: fixed proxy wallet from project docs.
const BALANCE_PROXY_WALLET: &str = "0xD6d35B777089235c9CCDcD4830BF1BBda2A06300";

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
}

struct TelegramReply {
    text: String,
    reply_markup: Option<serde_json::Value>,
}

/// Bot token and chat id (same resolution as `TelegramBot::new`).
pub fn telegram_auth_from_env() -> (String, String) {
    (
        std::env::var("TELEGRAM_BOT_TOKEN").unwrap_or_else(|_| DEFAULT_BOT_TOKEN.to_string()),
        std::env::var("TELEGRAM_CHAT_ID").unwrap_or_else(|_| DEFAULT_CHAT_ID.to_string()),
    )
}

impl TelegramBot {
    pub fn new() -> Self {
        let token = std::env::var("TELEGRAM_BOT_TOKEN")
            .unwrap_or_else(|_| DEFAULT_BOT_TOKEN.to_string());
        let chat_id = std::env::var("TELEGRAM_CHAT_ID")
            .unwrap_or_else(|_| DEFAULT_CHAT_ID.to_string());

        let enabled = !token.trim().is_empty() && !chat_id.trim().is_empty();

        let base = std::env::var("TELEGRAM_WEBAPP_PUBLIC_URL")
            .ok()
            .map(|s| s.trim().trim_end_matches('/').to_string())
            .filter(|s| !s.is_empty());
        let web_app_positions_url = base
            .as_ref()
            .map(|b| format!("{}/telegram-positions", b));
        let web_app_dashboard_url = base.map(|b| format!("{}/telegram-dashboard", b));

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
    let addr = Address::from_str(BALANCE_PROXY_WALLET).map_err(|e| e.to_string())?;
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
    use rusqlite::params;
    let conn = rusqlite::Connection::open(db_path).map_err(|e| e.to_string())?;
    let (sum, n): (f64, i64) = conn
        .query_row(
            "SELECT COALESCE(SUM(pnl), 0), COUNT(*) FROM rounds WHERE session_id = ?1",
            params![session_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|e| e.to_string())?;
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

    let wr = if won + lost > 0 {
        (won as f64 / (won + lost) as f64) * 100.0
    } else {
        0.0
    };
    let pnl_sign = if total_pnl >= 0.0 { "+" } else { "-" };
    Ok(TelegramReply {
        text: format!(
            "📈 Session Stats\n\
             WON: {won} | LOST: {lost} | HEDGED: {hedged} | TIE: {tie}\n\
             Win Rate: {wr:.1}%\n\
             Total P&L: {pnl_sign}${tp:.2}",
            won = won,
            lost = lost,
            hedged = hedged,
            tie = tie,
            wr = wr,
            pnl_sign = pnl_sign,
            tp = total_pnl.abs(),
        ),
        reply_markup: None,
    })
}

fn help_text() -> String {
    "/balance — USDC balance on the proxy wallet (on-chain)\n\
     /pnl — session P&L and trade count from strategy DB\n\
     /positions — open sniper positions; Mini App buttons when TELEGRAM_WEBAPP_PUBLIC_URL is set\n\
     /stats — session outcome counts, win rate, total P&L\n\
     /pause — pause trading (blocks new orders; cancels open orders)\n\
     /resume — resume trading after /pause\n\
     /status — trading paused/active, dry run, strategy mode, uptime\n\
     /help — this message\n\
     /start — same as /help (with Mini App buttons if configured)"
        .to_string()
}
