// Suggested alias: Set-Alias polysup { cargo run --release --bin supervisor }
//
// Long-running Telegram supervisor: polls commands and spawns the trading bot as a child.
// If this process exits or is killed, the child bot is terminated as well (OS default on Windows).
// Run with the same TELEGRAM_BOT_TOKEN / TELEGRAM_CHAT_ID as the main bot.

use std::collections::HashSet;
use std::path::PathBuf;
use std::process::Stdio;
use std::str::FromStr;
use std::time::{Duration, Instant};

fn format_duration_hms(d: Duration) -> String {
    let secs = d.as_secs();
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{}h {}m {}s", h, m, s)
    } else if m > 0 {
        format!("{}m {}s", m, s)
    } else {
        format!("{}s", secs)
    }
}

use chrono::{DateTime, Local};
use polymarket_bot::balance::fetch_usdc_balance;
use polymarket_bot::telegram::{
    query_latest_session_id, query_session_pnl, query_session_stats, SessionStatsSnapshot,
};
use polymarket_client_sdk::types::Address;
use reqwest::Client;
use rust_decimal::prelude::ToPrimitive;
use serde::Deserialize;
use tokio::process::{Child, Command};
use tokio::time::sleep;

/// `/balance` command: fixed proxy wallet from project docs (same as `telegram.rs`).
const BALANCE_PROXY_WALLET: &str = "0xD6d35B777089235c9CCDcD4830BF1BBda2A06300";

fn default_polybot_binary() -> String {
    if cfg!(target_os = "windows") {
        "target\\release\\polymarket-bot.exe".to_string()
    } else {
        "target/release/polymarket-bot".to_string()
    }
}

fn default_polygon_rpc() -> String {
    std::env::var("POLYGON_RPC_URL").unwrap_or_else(|_| "https://polygon-rpc.com".to_string())
}

fn default_strategy_db() -> String {
    std::env::var("STRATEGY_DB_PATH").unwrap_or_else(|_| "logs/strategy.db".to_string())
}

fn telegram_token() -> String {
    std::env::var("TELEGRAM_BOT_TOKEN").unwrap_or_default()
}

fn telegram_chat_id() -> String {
    std::env::var("TELEGRAM_CHAT_ID").unwrap_or_default()
}

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

fn last_log_activity_line(logs_dir: &str) -> String {
    let Ok(entries) = std::fs::read_dir(logs_dir) else {
        return "no logs/ directory".to_string();
    };
    let mut best: Option<(std::time::SystemTime, String)> = None;
    for e in entries.filter_map(|e| e.ok()) {
        let Ok(meta) = e.metadata() else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let Ok(m) = meta.modified() else {
            continue;
        };
        let name = e.file_name().to_string_lossy().into_owned();
        best = match best {
            None => Some((m, name)),
            Some((t0, _)) if m > t0 => Some((m, name)),
            Some((t0, n0)) => Some((t0, n0)),
        };
    }
    let Some((t, name)) = best else {
        return "no log files".to_string();
    };
    let dt: DateTime<Local> = t.into();
    format!(
        "{} (mtime {})",
        name,
        dt.format("%Y-%m-%d %H:%M:%S")
    )
}

struct Supervisor {
    client: Client,
    token: String,
    chat_id: String,
    bot_process: Option<Child>,
    bot_started_at: Option<Instant>,
    strategy_db_path: String,
    bot_command: String,
    rpc_url: String,
}

impl Supervisor {
    fn new() -> Self {
        let bot_command =
            std::env::var("POLYBOT_BINARY").unwrap_or_else(|_| default_polybot_binary());
        Self {
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(60))
                .connect_timeout(std::time::Duration::from_secs(15))
                .build()
                .unwrap_or_else(|_| Client::new()),
            token: telegram_token(),
            chat_id: telegram_chat_id(),
            bot_process: None,
            bot_started_at: None,
            strategy_db_path: default_strategy_db(),
            bot_command,
            rpc_url: default_polygon_rpc(),
        }
    }

    async fn send_message_with_retry(&self, text: &str) {
        let url = format!("https://api.telegram.org/bot{}/sendMessage", self.token);
        let mut backoff_ms: u64 = 1000;
        loop {
            let body = serde_json::json!({
                "chat_id": self.chat_id,
                "text": text,
                "parse_mode": "HTML",
            });
            match self.client.post(&url).json(&body).send().await {
                Ok(resp) => {
                    if resp.status().is_success() {
                        return;
                    }
                    eprintln!(
                        "supervisor: sendMessage HTTP {} — retry in {}ms",
                        resp.status(),
                        backoff_ms
                    );
                }
                Err(e) => {
                    eprintln!(
                        "supervisor: sendMessage error {} — retry in {}ms",
                        e,
                        backoff_ms
                    );
                }
            }
            sleep(Duration::from_millis(backoff_ms)).await;
            backoff_ms = (backoff_ms * 2).min(300_000);
        }
    }

    fn poll_backoff_sleep_ms(attempt: u32) -> u64 {
        let base = 1000u64 * (1u64 << attempt.min(8));
        base.min(300_000)
    }

    /// Returns true if a child is present and still running; clears state if it has exited.
    fn child_running(&mut self) -> bool {
        if let Some(ref mut ch) = self.bot_process {
            match ch.try_wait() {
                Ok(None) => return true,
                Ok(Some(_)) => {
                    self.bot_process = None;
                    self.bot_started_at = None;
                }
                Err(_) => {
                    self.bot_process = None;
                    self.bot_started_at = None;
                }
            }
        }
        false
    }

    async fn notify_if_child_exited(&mut self) {
        if let Some(ref mut child) = self.bot_process {
            match child.try_wait() {
                Ok(Some(status)) => {
                    self.bot_started_at = None;
                    self.bot_process = None;
                    let code = status.code();
                    self.send_message_with_retry(&format!(
                        "⚠️ Bot exited unexpectedly (exit code: {:?})",
                        code
                    ))
                    .await;
                }
                Ok(None) => {}
                Err(e) => eprintln!("supervisor: try_wait error: {}", e),
            }
        }
    }

    async fn cmd_balance(&self) -> String {
        let addr = match Address::from_str(BALANCE_PROXY_WALLET) {
            Ok(a) => a,
            Err(e) => return format!("Balance error: {}", e),
        };
        match fetch_usdc_balance(&self.rpc_url, addr).await {
            Ok(b) => {
                let v = b.to_f64().unwrap_or(0.0);
                format!("💰 Balance: ${:.2} USDC", v)
            }
            Err(e) => format!("Balance error: {}", e),
        }
    }

    fn latest_session_pnl_line(&self) -> String {
        let path = &self.strategy_db_path;
        if !PathBuf::from(path).exists() {
            return "No trading data yet.".to_string();
        }
        let sid = match query_latest_session_id(path) {
            Ok(Some(s)) => s,
            Ok(None) => return "No trading data yet.".to_string(),
            Err(e) => return format!("DB error: {}", e),
        };
        match query_session_pnl(path, &sid) {
            Ok((sum, n)) => {
                if n == 0 {
                    return "No trading data yet.".to_string();
                }
                let sign = if sum >= 0.0 { "+" } else { "-" };
                format!(
                    "📊 Session P&L: {}${:.4} ({} trades)",
                    sign,
                    sum.abs(),
                    n
                )
            }
            Err(e) => format!("DB error: {}", e),
        }
    }

    fn latest_session_stats_line(&self) -> String {
        let path = &self.strategy_db_path;
        if !PathBuf::from(path).exists() {
            return "No trading data yet.".to_string();
        }
        let sid = match query_latest_session_id(path) {
            Ok(Some(s)) => s,
            Ok(None) => return "No trading data yet.".to_string(),
            Err(e) => return format!("DB error: {}", e),
        };
        match query_session_stats(path, &sid) {
            Ok(s) => Self::format_stats_card(&s),
            Err(e) => format!("DB error: {}", e),
        }
    }

    fn format_stats_card(s: &SessionStatsSnapshot) -> String {
        let wr = if s.won + s.lost > 0 {
            (s.won as f64 / (s.won + s.lost) as f64) * 100.0
        } else {
            0.0
        };
        let pnl_sign = if s.total_pnl >= 0.0 { "+" } else { "-" };
        format!(
            "📈 Session Stats\n\
             WON: {} | LOST: {} | HEDGED: {} | TIE: {}\n\
             Win Rate: {:.1}%\n\
             Total P&L: {}${:.2}",
            s.won,
            s.lost,
            s.hedged,
            s.tie,
            wr,
            pnl_sign,
            s.total_pnl.abs(),
        )
    }

    async fn cmd_dashboard(&self, bot_running: bool, pid: u32, uptime: Option<Duration>) -> String {
        let balance_line = self.cmd_balance().await;
        let bot_line = if bot_running {
            let up = uptime
                .map(format_duration_hms)
                .unwrap_or_else(|| "?".to_string());
            format!("🟢 Bot is running (PID: {}, uptime: {})", pid, up)
        } else {
            "🔴 Bot is not running".to_string()
        };
        let stats = self.latest_session_stats_line();
        format!(
            "📊 Polybot Dashboard (supervisor)\n\
             ━━━━━━━━━━━━━━━━━━━━\n\
             \n\
             {}\n\
             {}\n\
             \n\
             {}\n\
             \n\
             📍 Open positions: available when the trading bot is running.\n\
             \n\
             Latest log: {}",
            balance_line,
            bot_line,
            stats,
            last_log_activity_line("logs")
        )
    }

    async fn handle_command(&mut self, text: &str) -> String {
        let key = normalize_command(text);
        let running = self.child_running();
        let pid = self
            .bot_process
            .as_ref()
            .and_then(|c| c.id())
            .unwrap_or(0);
        let uptime = self.bot_started_at.map(|t| t.elapsed());

        match key.as_str() {
            "/start" | "/startbot" => self.cmd_start_bot().await,
            "/stop" | "/stopbot" => self.cmd_stop_bot().await,
            "/restart" => self.cmd_restart_bot().await,
            "/status" => self.cmd_status(running, pid, uptime).await,
            "/balance" => self.cmd_balance().await,
            "/pnl" => self.latest_session_pnl_line(),
            "/stats" => self.latest_session_stats_line(),
            "/dashboard" => self.cmd_dashboard(running, pid, uptime).await,
            "/help" => self.help_text(),
            s if s.starts_with('/') => "Unknown command. Try /help".to_string(),
            _ => "Unknown command. Try /help".to_string(),
        }
    }

    fn help_text(&self) -> String {
        format!(
            "Supervisor commands (trading bot may be stopped):\n\
             /start or /startbot — start polymarket-bot child process\n\
             /stop or /stopbot — stop the bot\n\
             /restart — stop then start\n\
             /status — bot PID / uptime + latest log mtime\n\
             /balance — on-chain USDC (proxy wallet)\n\
             /pnl — latest session P&L from {}\n\
             /stats — latest session stats from DB\n\
             /dashboard — balance + bot status + DB stats\n\
             /help — this message\n\
             \n\
             Bot binary: {}\n\
             Child env includes DISABLE_TELEGRAM_POLLING=true.",
            self.strategy_db_path,
            self.bot_command
        )
    }

    async fn cmd_status(&self, running: bool, pid: u32, uptime: Option<Duration>) -> String {
        let log = last_log_activity_line("logs");
        if running {
            let up = uptime
                .map(format_duration_hms)
                .unwrap_or_else(|| "?".to_string());
            format!(
                "🟢 Bot is running (PID: {}, uptime: {})\n\
                 Latest log: {}",
                pid, up, log
            )
        } else {
            format!("🔴 Bot is not running\nLatest log: {}", log)
        }
    }

    async fn cmd_start_bot(&mut self) -> String {
        if self.child_running() {
            return "⚠️ Bot is already running.".to_string();
        }
        let mut cmd = Command::new(&self.bot_command);
        cmd.env("RUST_LOG", "info")
            .env("DISABLE_TELEGRAM_POLLING", "true")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => return format!("Failed to spawn bot: {}", e),
        };
        let pid = child.id().unwrap_or(0);
        self.bot_process = Some(child);
        self.bot_started_at = Some(Instant::now());
        sleep(Duration::from_secs(3)).await;
        if !self.child_running() {
            return format!(
                "⚠️ Bot crashed immediately after start (PID was {}). Check logs/.",
                pid
            );
        }
        format!("🟢 Bot started (PID: {})", pid)
    }

    async fn cmd_stop_bot(&mut self) -> String {
        if !self.child_running() {
            return "⚠️ Bot is not running.".to_string();
        }
        if let Some(mut ch) = self.bot_process.take() {
            self.bot_started_at = None;
            if let Err(e) = ch.kill().await {
                return format!("Failed to stop bot: {}", e);
            }
            let _ = ch.wait().await;
        }
        "🔴 Bot stopped.".to_string()
    }

    async fn cmd_restart_bot(&mut self) -> String {
        let stop = self.cmd_stop_bot().await;
        sleep(Duration::from_secs(2)).await;
        let start = self.cmd_start_bot().await;
        format!("Restart:\n1) {}\n2) {}", stop, start)
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

#[tokio::main]
async fn main() {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("rustls crypto provider");

    let mut sup = Supervisor::new();
    if sup.token.trim().is_empty() || sup.chat_id.trim().is_empty() {
        eprintln!("Set TELEGRAM_BOT_TOKEN and TELEGRAM_CHAT_ID");
        std::process::exit(1);
    }

    let mut offset: i64 = 0;
    let mut seen: HashSet<i64> = HashSet::new();
    const SEEN_CAP: usize = 2048;
    let mut fail_attempts: u32 = 0;

    loop {
        sup.notify_if_child_exited().await;

        let url = format!(
            "https://api.telegram.org/bot{}/getUpdates",
            sup.token
        );
        let body = serde_json::json!({
            "offset": offset,
            "timeout": 30,
            "allowed_updates": ["message", "edited_message"],
        });
        let resp = match sup.client.post(&url).json(&body).send().await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("supervisor: getUpdates error: {}", e);
                sleep(Duration::from_millis(Supervisor::poll_backoff_sleep_ms(fail_attempts)))
                    .await;
                fail_attempts = fail_attempts.saturating_add(1);
                continue;
            }
        };
        fail_attempts = 0;

        if !resp.status().is_success() {
            eprintln!("supervisor: getUpdates HTTP {}", resp.status());
            sleep(Duration::from_secs(5)).await;
            continue;
        }

        let parse: Result<TgGetUpdatesResponse, _> = resp.json().await;
        let upd = match parse {
            Ok(u) if u.ok => u,
            Ok(_) => {
                sleep(Duration::from_secs(5)).await;
                continue;
            }
            Err(e) => {
                eprintln!("supervisor: getUpdates JSON: {}", e);
                sleep(Duration::from_secs(5)).await;
                continue;
            }
        };

        let mut max_id: i64 = -1;
        for u in upd.result {
            max_id = max_id.max(u.update_id);
            if seen.len() > SEEN_CAP {
                seen.clear();
            }
            if !seen.insert(u.update_id) {
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
                .map(|id| id.to_string() == sup.chat_id)
                .unwrap_or(false);
            if !chat_ok {
                continue;
            }
            let text = msg.text.unwrap_or_default();
            let reply = sup.handle_command(text.trim()).await;
            sup.send_message_with_retry(&reply).await;
        }
        if max_id >= 0 {
            offset = max_id + 1;
        }
    }
}
