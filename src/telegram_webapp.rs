//! HTTPS Mini App: positions countdown + live dashboard (initData validated server-side).

use crate::types::{Coin, Period, RoundHistoryEntry, RoundHistoryStatus};
use chrono::{TimeZone, Utc};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::Json;
use axum::Router;
use dashmap::DashMap;
use rust_decimal::prelude::ToPrimitive;
use serde::Deserialize;
use serde::Serialize;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::watch;
use tower_http::cors::CorsLayer;
use tracing::{error, info, warn};

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

const HTML_POSITIONS: &str = include_str!("../static/telegram_positions.html");
const HTML_DASHBOARD: &str = include_str!("../static/telegram_dashboard.html");

/// Max age of `auth_date` in initData (replay protection).
const MAX_AUTH_AGE_SECS: i64 = 86400;

#[derive(Clone)]
struct MiniAppState {
    bot_token: String,
    allowed_chat_id: String,
    round_history: Arc<Mutex<Vec<RoundHistoryEntry>>>,
    trading_paused: Arc<AtomicBool>,
    dry_run: Arc<AtomicBool>,
    balance_rx: watch::Receiver<rust_decimal::Decimal>,
    strategy_status: Arc<DashMap<String, String>>,
    live_rounds: Arc<Mutex<Vec<LiveRoundRow>>>,
    session_start: chrono::DateTime<chrono::Utc>,
    #[allow(dead_code)]
    session_start_balance: rust_decimal::Decimal,
    strategy_mode: String,
}

#[derive(Deserialize)]
struct InitDataBody {
    init_data: String,
}

#[derive(Serialize)]
struct PositionsResponse {
    server_now: i64,
    positions: Vec<PositionRow>,
}

#[derive(Serialize)]
struct PositionRow {
    coin: String,
    period: String,
    side: String,
    entry_price: f64,
    /// Primary leg filled size (shares).
    shares: f64,
    round_start: i64,
    round_end: i64,
    /// e.g. `12:30–12:35 UTC` for the round window.
    round_time_range_utc: String,
}

/// One row per configured (coin, period) — same book prices and strategy line as the TUI Active Rounds table.
#[derive(Clone, Serialize)]
pub struct LiveRoundRow {
    pub coin: String,
    pub period: String,
    /// e.g. `BTC 5m`
    pub market_label: String,
    pub round_start: i64,
    pub round_end: i64,
    pub round_time_range_utc: String,
    /// 0.0–1.0 through the current round window.
    pub elapsed_pct: f64,
    pub yes_ask: f64,
    pub no_ask: f64,
    pub strategy: String,
    pub status: String,
}

/// Shorten long sniper status strings for narrow layouts (matches TUI `abbrev_active_round_strategy`).
pub fn abbrev_strategy_line(s: &str) -> String {
    let t = s.trim();
    let t = t.replace("Cap reached", "Capped");
    let t = t.replace("Dual filled, rest cx'd", "Dual+cx");
    if t.chars().count() > 32 {
        t.chars().take(29).collect::<String>() + "…"
    } else {
        t
    }
}

#[derive(Serialize)]
struct DashboardResponse {
    server_now: i64,
    trading_paused: bool,
    dry_run: bool,
    strategy_mode: String,
    balance_usdc: f64,
    session_pnl: f64,
    uptime_secs: u64,
    positions: Vec<PositionRow>,
    strategy_status: Vec<StatusRow>,
    live_rounds: Vec<LiveRoundRow>,
}

#[derive(Serialize)]
struct StatusRow {
    key: String,
    value: String,
}

/// Starts the HTTP server for Mini Apps + JSON APIs. Requires HTTPS in front for production.
pub fn spawn_http_server(
    bind: SocketAddr,
    bot_token: String,
    allowed_chat_id: String,
    round_history: Arc<Mutex<Vec<RoundHistoryEntry>>>,
    trading_paused: Arc<AtomicBool>,
    dry_run: Arc<AtomicBool>,
    balance: Arc<crate::balance::BalanceManager>,
    strategy_status: Arc<DashMap<String, String>>,
    live_rounds: Arc<Mutex<Vec<LiveRoundRow>>>,
    session_start: chrono::DateTime<chrono::Utc>,
    session_start_balance: rust_decimal::Decimal,
    strategy_mode: String,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let state = MiniAppState {
            bot_token,
            allowed_chat_id,
            round_history,
            trading_paused,
            dry_run,
            balance_rx: balance.receiver(),
            strategy_status,
            live_rounds,
            session_start,
            session_start_balance,
            strategy_mode,
        };
        let app = Router::new()
            .route("/telegram-positions", get(serve_positions_html))
            .route("/telegram-dashboard", get(serve_dashboard_html))
            .route("/api/telegram-positions", post(positions_handler))
            .route("/api/telegram-dashboard", post(dashboard_handler))
            .with_state(state)
            .layer(CorsLayer::permissive());

        let listener = match tokio::net::TcpListener::bind(bind).await {
            Ok(l) => l,
            Err(e) => {
                error!(error = %e, %bind, "Telegram Mini App HTTP bind failed");
                return;
            }
        };
        info!(%bind, "Telegram Mini App HTTP server (positions + dashboard)");
        if let Err(e) = axum::serve(listener, app.into_make_service()).await {
            error!(error = %e, "Telegram Mini App HTTP server error");
        }
    })
}

async fn serve_positions_html() -> Html<&'static str> {
    Html(HTML_POSITIONS)
}

async fn serve_dashboard_html() -> Html<&'static str> {
    Html(HTML_DASHBOARD)
}

async fn positions_handler(
    State(state): State<MiniAppState>,
    Json(body): Json<InitDataBody>,
) -> Response {
    match validate_and_user_id(&body.init_data, &state.bot_token) {
        Ok(user_id) => {
            if !chat_id_matches(&state.allowed_chat_id, user_id) {
                return (
                    StatusCode::FORBIDDEN,
                    "Forbidden: chat does not match TELEGRAM_CHAT_ID",
                )
                    .into_response();
            }
        }
        Err(e) => {
            warn!(%e, "Mini App initData validation failed");
            return (StatusCode::UNAUTHORIZED, e).into_response();
        }
    }

    let guard = match state.round_history.lock() {
        Ok(g) => g,
        Err(_) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, "state lock poisoned").into_response();
        }
    };

    let server_now = chrono::Utc::now().timestamp();
    let mut positions: Vec<PositionRow> = Vec::new();
    for e in guard.iter() {
        if !matches!(e.status, RoundHistoryStatus::Open) {
            continue;
        }
        positions.push(PositionRow {
            coin: coin_display(e.coin),
            period: period_display(e.period),
            side: e.side_label.clone(),
            entry_price: e.entry_price.to_f64().unwrap_or(0.0),
            shares: e.shares.to_f64().unwrap_or(0.0),
            round_start: e.round_start,
            round_end: e.round_end,
            round_time_range_utc: format_round_time_range_utc(e.round_start, e.round_end),
        });
    }
    drop(guard);

    Json(PositionsResponse {
        server_now,
        positions,
    })
    .into_response()
}

async fn dashboard_handler(
    State(state): State<MiniAppState>,
    Json(body): Json<InitDataBody>,
) -> Response {
    match validate_and_user_id(&body.init_data, &state.bot_token) {
        Ok(user_id) => {
            if !chat_id_matches(&state.allowed_chat_id, user_id) {
                return (
                    StatusCode::FORBIDDEN,
                    "Forbidden: chat does not match TELEGRAM_CHAT_ID",
                )
                    .into_response();
            }
        }
        Err(e) => {
            warn!(%e, "Dashboard initData validation failed");
            return (StatusCode::UNAUTHORIZED, e).into_response();
        }
    }

    let guard = match state.round_history.lock() {
        Ok(g) => g,
        Err(_) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, "state lock poisoned").into_response();
        }
    };
    let rh: Vec<RoundHistoryEntry> = guard.clone();
    drop(guard);

    let server_now = chrono::Utc::now().timestamp();
    let balance_usdc = state
        .balance_rx
        .borrow()
        .to_f64()
        .unwrap_or(0.0);
    let session_pnl = crate::types::RoundHistoryEntry::sum_session_pnl(&rh)
        .to_f64()
        .unwrap_or(0.0);

    let mut positions: Vec<PositionRow> = Vec::new();
    for e in rh.iter() {
        if !matches!(e.status, RoundHistoryStatus::Open) {
            continue;
        }
        positions.push(PositionRow {
            coin: coin_display(e.coin),
            period: period_display(e.period),
            side: e.side_label.clone(),
            entry_price: e.entry_price.to_f64().unwrap_or(0.0),
            shares: e.shares.to_f64().unwrap_or(0.0),
            round_start: e.round_start,
            round_end: e.round_end,
            round_time_range_utc: format_round_time_range_utc(e.round_start, e.round_end),
        });
    }

    let mut strategy_status: Vec<StatusRow> = Vec::new();
    for r in state.strategy_status.iter() {
        strategy_status.push(StatusRow {
            key: format_strategy_status_key_display(r.key()),
            value: r.value().clone(),
        });
    }
    strategy_status.sort_by(|a, b| a.key.cmp(&b.key));

    let uptime_secs = (chrono::Utc::now() - state.session_start)
        .num_seconds()
        .max(0) as u64;

    let live_rounds = state
        .live_rounds
        .lock()
        .map(|g| g.clone())
        .unwrap_or_default();

    Json(DashboardResponse {
        server_now,
        trading_paused: state.trading_paused.load(Ordering::Relaxed),
        dry_run: state.dry_run.load(Ordering::Relaxed),
        strategy_mode: state.strategy_mode.clone(),
        balance_usdc,
        session_pnl,
        uptime_secs,
        positions,
        strategy_status,
        live_rounds,
    })
    .into_response()
}

fn coin_display(c: Coin) -> String {
    c.as_str().to_uppercase()
}

fn period_display(p: Period) -> String {
    format!("{}m", p.as_minutes())
}

/// `coin_periodMin_roundStart_strategy...` → `BTC 15m 12:30–12:45 UTC · sniper` (replaces raw epoch in the label).
fn format_strategy_status_key_display(key: &str) -> String {
    let parts: Vec<&str> = key.split('_').collect();
    if parts.len() < 4 {
        return key.to_string();
    }
    let coin = parts[0].to_uppercase();
    let Ok(period_min) = parts[1].parse::<u64>() else {
        return key.to_string();
    };
    let Ok(round_start) = parts[2].parse::<i64>() else {
        return key.to_string();
    };
    let strategy = parts[3..].join("_");
    let dur_secs = (period_min as i64) * 60;
    let round_end = round_start + dur_secs;
    let start = Utc.timestamp_opt(round_start, 0).single();
    let end = Utc.timestamp_opt(round_end, 0).single();
    match (start, end) {
        (Some(s), Some(e)) => {
            let range = if s.date_naive() == e.date_naive() {
                format!("{}–{} UTC", s.format("%H:%M"), e.format("%H:%M"))
            } else {
                format!(
                    "{} – {} UTC",
                    s.format("%b %d %H:%M"),
                    e.format("%b %d %H:%M")
                )
            };
            format!("{} {}m {} · {}", coin, period_min, range, strategy)
        }
        _ => key.to_string(),
    }
}

pub fn format_round_time_range_utc(round_start: i64, round_end: i64) -> String {
    let start = Utc.timestamp_opt(round_start, 0).single();
    let end = Utc.timestamp_opt(round_end, 0).single();
    match (start, end) {
        (Some(s), Some(e)) => {
            if s.date_naive() == e.date_naive() {
                format!("{}–{} UTC", s.format("%H:%M"), e.format("%H:%M"))
            } else {
                format!(
                    "{} – {} UTC",
                    s.format("%b %d %H:%M"),
                    e.format("%b %d %H:%M")
                )
            }
        }
        _ => String::new(),
    }
}

fn chat_id_matches(allowed: &str, user_id: i64) -> bool {
    let allowed = allowed.trim();
    if allowed.is_empty() {
        return false;
    }
    if let Ok(n) = allowed.parse::<i64>() {
        return n == user_id;
    }
    false
}

fn validate_and_user_id(init_data: &str, bot_token: &str) -> Result<i64, String> {
    if init_data.is_empty() {
        return Err("empty init_data".into());
    }

    let mut pairs: Vec<(String, String)> = Vec::new();
    for (k, v) in url::form_urlencoded::parse(init_data.as_bytes()) {
        pairs.push((k.into_owned(), v.into_owned()));
    }

    let mut hash_hex: Option<String> = None;
    let mut map: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    for (k, v) in pairs {
        if k == "hash" {
            hash_hex = Some(v);
        } else {
            map.insert(k, v);
        }
    }

    let expected = hash_hex.ok_or_else(|| "missing hash".to_string())?;

    let data_check_string = map
        .iter()
        .map(|(k, v)| format!("{}={}", k, v))
        .collect::<Vec<_>>()
        .join("\n");

    let secret_key = {
        let mut mac = HmacSha256::new_from_slice(b"WebAppData").map_err(|e| e.to_string())?;
        mac.update(bot_token.as_bytes());
        mac.finalize().into_bytes()
    };

    let computed = {
        let mut mac = HmacSha256::new_from_slice(&secret_key).map_err(|e| e.to_string())?;
        mac.update(data_check_string.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    };

    if !constant_time_eq(computed.as_bytes(), expected.to_ascii_lowercase().as_bytes()) {
        return Err("invalid hash".into());
    }

    let auth_date: i64 = map
        .get("auth_date")
        .ok_or_else(|| "missing auth_date".to_string())?
        .parse()
        .map_err(|_| "bad auth_date".to_string())?;
    let now = chrono::Utc::now().timestamp();
    if (now - auth_date).abs() > MAX_AUTH_AGE_SECS {
        return Err("auth_date expired".into());
    }

    let user_json = map.get("user").ok_or_else(|| "missing user".to_string())?;
    let v: serde_json::Value =
        serde_json::from_str(user_json).map_err(|_| "bad user json".to_string())?;
    let id = v
        .get("id")
        .and_then(|x| x.as_i64())
        .ok_or_else(|| "missing user id".to_string())?;
    Ok(id)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}
