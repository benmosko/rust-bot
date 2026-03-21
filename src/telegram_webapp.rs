//! HTTPS Mini App: live client-side countdown for open positions (`initData` validated server-side).

use crate::types::{Coin, Period, RoundHistoryEntry, RoundHistoryStatus};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::Json;
use axum::Router;
use hmac::{Hmac, Mac};
use rust_decimal::prelude::ToPrimitive;
use serde::Deserialize;
use serde::Serialize;
use sha2::Sha256;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tower_http::cors::CorsLayer;
use tracing::{error, info, warn};

type HmacSha256 = Hmac<Sha256>;

const HTML: &str = include_str!("../static/telegram_positions.html");

/// Max age of `auth_date` in initData (replay protection).
const MAX_AUTH_AGE_SECS: i64 = 86400;

#[derive(Clone)]
struct AppState {
    bot_token: String,
    allowed_chat_id: String,
    round_history: Arc<Mutex<Vec<RoundHistoryEntry>>>,
}

#[derive(Deserialize)]
struct PositionsBody {
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
    round_end: i64,
}

/// Starts the HTTP server for the Mini App + JSON API. Requires HTTPS in front for production
/// (Telegram Web Apps). Set `TELEGRAM_WEBAPP_PUBLIC_URL` in the bot for the Web App button.
pub fn spawn_http_server(
    bind: SocketAddr,
    bot_token: String,
    allowed_chat_id: String,
    round_history: Arc<Mutex<Vec<RoundHistoryEntry>>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let state = AppState {
            bot_token,
            allowed_chat_id,
            round_history,
        };
        let app = Router::new()
            .route("/telegram-positions", get(serve_html))
            .route("/api/telegram-positions", post(positions_handler))
            .with_state(state)
            .layer(CorsLayer::permissive());

        let listener = match tokio::net::TcpListener::bind(bind).await {
            Ok(l) => l,
            Err(e) => {
                error!(error = %e, %bind, "Telegram Mini App HTTP bind failed");
                return;
            }
        };
        info!(%bind, "Telegram Mini App HTTP server listening (HTTPS via reverse proxy in production)");
        if let Err(e) = axum::serve(listener, app.into_make_service()).await {
            error!(error = %e, "Telegram Mini App HTTP server error");
        }
    })
}

async fn serve_html() -> Html<&'static str> {
    Html(HTML)
}

async fn positions_handler(
    State(state): State<AppState>,
    Json(body): Json<PositionsBody>,
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
            round_end: e.round_end,
        });
    }
    drop(guard);

    Json(PositionsResponse {
        server_now,
        positions,
    })
    .into_response()
}

fn coin_display(c: Coin) -> String {
    c.as_str().to_uppercase()
}

fn period_display(p: Period) -> String {
    format!("{}m", p.as_minutes())
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

/// Validates [Telegram Web App init data](https://core.telegram.org/bots/webapps#validating-data-received-via-the-mini-app) and returns Telegram user id.
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
