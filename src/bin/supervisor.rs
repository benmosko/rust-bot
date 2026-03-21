// Suggested alias: Set-Alias polysup { cargo run --release --bin supervisor }
//
// Long-running Telegram supervisor: polls commands and spawns the trading bot as a child.
// If this process exits or is killed, the child bot is terminated as well (OS default on Windows).
// Run with the same TELEGRAM_BOT_TOKEN / TELEGRAM_CHAT_ID as the main bot.

use std::collections::HashSet;
use std::path::Path;
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
    query_session_pnl, query_session_stats, resolve_session_id_for_supervisor,
    webapp_dashboard_url_from_env, SessionStatsSnapshot,
};
use polymarket_client_sdk::types::Address;
use reqwest::Client;
use rust_decimal::prelude::ToPrimitive;
use serde::Deserialize;
use tokio::process::{Child, Command};
use tokio::time::sleep;

/// `/balance` command: fixed proxy wallet from project docs (same as `telegram.rs`).
const BALANCE_PROXY_WALLET: &str = "0xD6d35B777089235c9CCDcD4830BF1BBda2A06300";

/// Inline WebApp button for the same Mini App URL `polymarket-bot` uses on startup (if env is set).
fn telegram_webapp_dashboard_markup() -> Option<serde_json::Value> {
    let url = webapp_dashboard_url_from_env()?;
    Some(serde_json::json!({
        "inline_keyboard": [[{
            "text": "📊 Live dashboard",
            "web_app": { "url": url }
        }]]
    }))
}

fn default_polybot_binary() -> String {
    let candidates: &[&str] = if cfg!(target_os = "windows") {
        &[
            r"target\release-fast\polymarket-bot.exe",
            r"target\release\polymarket-bot.exe",
        ]
    } else {
        &[
            "target/release-fast/polymarket-bot",
            "target/release/polymarket-bot",
        ]
    };
    for c in candidates {
        if Path::new(c).exists() {
            return (*c).to_string();
        }
    }
    candidates[0].to_string()
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

/// True if an OS process with this PID exists (fallback when `Child` handle state is wrong).
fn process_exists(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(windows)]
    {
        use std::process::Command as StdCommand;
        let Ok(output) = StdCommand::new("tasklist")
            .args(["/FI", &format!("PID eq {}", pid), "/FO", "CSV", "/NH"])
            .output()
        else {
            return false;
        };
        let s = String::from_utf8_lossy(&output.stdout);
        if s.contains("INFO: No tasks") {
            return false;
        }
        s.contains(&format!("\"{}\"", pid))
    }
    #[cfg(not(windows))]
    {
        use std::process::Command as StdCommand;
        StdCommand::new("/bin/sh")
            .args(["-c", &format!("kill -0 {} 2>/dev/null", pid)])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
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
    /// Set when we spawn the bot; used with `process_exists` if the `Child` handle disagrees.
    tracked_bot_pid: Option<u32>,
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
            tracked_bot_pid: None,
            bot_started_at: None,
            strategy_db_path: default_strategy_db(),
            bot_command,
            rpc_url: default_polygon_rpc(),
        }
    }

    /// Clear webhook so `getUpdates` long polling works (Telegram rejects polling if a webhook is set).
    async fn telegram_delete_webhook(&self) {
        let url = format!(
            "https://api.telegram.org/bot{}/deleteWebhook",
            self.token
        );
        let body = serde_json::json!({ "drop_pending_updates": false });
        match self.client.post(&url).json(&body).send().await {
            Ok(r) if r.status().is_success() => {}
            Ok(r) => {
                let status = r.status();
                let body = r.text().await.unwrap_or_default();
                eprintln!(
                    "supervisor: deleteWebhook HTTP {} — {}",
                    status,
                    body.chars().take(300).collect::<String>()
                );
            }
            Err(e) => eprintln!("supervisor: deleteWebhook error: {}", e),
        }
    }

    /// Log webhook URL (if any) for debugging 409 / polling conflicts.
    async fn telegram_log_webhook_info(&self) {
        let url = format!(
            "https://api.telegram.org/bot{}/getWebhookInfo",
            self.token
        );
        match self.client.get(&url).send().await {
            Ok(r) => {
                if let Ok(text) = r.text().await {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                        let wurl = v
                            .get("result")
                            .and_then(|x| x.get("url"))
                            .and_then(|u| u.as_str())
                            .unwrap_or("");
                        if wurl.is_empty() {
                            eprintln!("supervisor: getWebhookInfo: no webhook URL (OK for long polling)");
                        } else {
                            eprintln!(
                                "supervisor: getWebhookInfo: webhook URL is set — {}",
                                wurl
                            );
                        }
                    } else {
                        eprintln!("supervisor: getWebhookInfo: {}", text.chars().take(200).collect::<String>());
                    }
                }
            }
            Err(e) => eprintln!("supervisor: getWebhookInfo error: {}", e),
        }
    }

    async fn send_message_with_retry(&self, text: &str, reply_markup: Option<serde_json::Value>) {
        let url = format!("https://api.telegram.org/bot{}/sendMessage", self.token);
        let mut backoff_ms: u64 = 1000;
        loop {
            let mut body = serde_json::json!({
                "chat_id": self.chat_id,
                "text": text,
                "parse_mode": "HTML",
            });
            if let Some(ref m) = reply_markup {
                body["reply_markup"] = m.clone();
            }
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
    /// On `try_wait` I/O errors we keep the handle and assume the child is still running — dropping
    /// the handle would orphan the process and falsely report "not running" (seen on Windows).
    fn child_running(&mut self) -> bool {
        if let Some(ref mut ch) = self.bot_process {
            match ch.try_wait() {
                Ok(None) => return true,
                Ok(Some(_)) => {
                    self.bot_process = None;
                    self.bot_started_at = None;
                    self.tracked_bot_pid = None;
                }
                Err(e) => {
                    eprintln!(
                        "supervisor: try_wait error (keeping child handle): {}",
                        e
                    );
                    return true;
                }
            }
        }
        false
    }

    /// Reconcile `Child` handle with OS process table (fixes false "not running" on some Windows setups).
    fn refresh_bot_state(&mut self) -> (bool, u32, Option<Duration>) {
        let running = self.child_running();
        let pid_from_handle = self
            .bot_process
            .as_ref()
            .and_then(|c| c.id())
            .filter(|&id| id != 0);

        if running {
            if let Some(p) = pid_from_handle {
                self.tracked_bot_pid = Some(p);
            }
            let pid = pid_from_handle.or(self.tracked_bot_pid).unwrap_or(0);
            return (
                true,
                pid,
                self.bot_started_at.map(|t| t.elapsed()),
            );
        }

        if let Some(tp) = self.tracked_bot_pid {
            if process_exists(tp) {
                eprintln!(
                    "supervisor: no live Child handle but PID {} exists (OS); reporting running",
                    tp
                );
                return (
                    true,
                    tp,
                    self.bot_started_at.map(|t| t.elapsed()),
                );
            }
            self.tracked_bot_pid = None;
            self.bot_started_at = None;
        }

        (false, 0, None)
    }

    async fn notify_if_child_exited(&mut self) {
        if let Some(ref mut child) = self.bot_process {
            match child.try_wait() {
                Ok(Some(status)) => {
                    self.bot_started_at = None;
                    self.bot_process = None;
                    self.tracked_bot_pid = None;
                    let code = status.code();
                    self.send_message_with_retry(
                        &format!("⚠️ Bot exited unexpectedly (exit code: {:?})", code),
                        None,
                    )
                    .await;
                }
                Ok(None) => {}
                Err(e) => eprintln!(
                    "supervisor: try_wait error in notify_if_child_exited (ignored): {}",
                    e
                ),
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
        if !Path::new(path).exists() {
            return "No trading data yet.".to_string();
        }
        let sid = match resolve_session_id_for_supervisor(path, "logs") {
            Ok(Some(s)) => s,
            Ok(None) => return "No trading data yet.".to_string(),
            Err(e) => return format!("DB error: {}", e),
        };
        match query_session_pnl(path, &sid) {
            Ok((sum, n)) => {
                if n == 0 {
                    return format!(
                        "📊 Session P&L ({})\n+$0.0000 (0 trades — no resolutions logged yet)",
                        sid
                    );
                }
                let sign = if sum >= 0.0 { "+" } else { "-" };
                format!(
                    "📊 Session P&L ({})\n{}${:.4} ({} trades)",
                    sid,
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
        if !Path::new(path).exists() {
            return "No trading data yet.".to_string();
        }
        let sid = match resolve_session_id_for_supervisor(path, "logs") {
            Ok(Some(s)) => s,
            Ok(None) => return "No trading data yet.".to_string(),
            Err(e) => return format!("DB error: {}", e),
        };
        match query_session_stats(path, &sid) {
            Ok(s) => Self::format_stats_card(&sid, &s),
            Err(e) => format!("DB error: {}", e),
        }
    }

    fn format_stats_card(session_id: &str, s: &SessionStatsSnapshot) -> String {
        let wr = if s.won + s.lost > 0 {
            (s.won as f64 / (s.won + s.lost) as f64) * 100.0
        } else {
            0.0
        };
        let pnl_sign = if s.total_pnl >= 0.0 { "+" } else { "-" };
        format!(
            "📈 Session Stats — {}\n\
             WON: {} | LOST: {} | HEDGED: {} | TIE: {}\n\
             Win Rate: {:.1}%\n\
             Total P&L: {}${:.2}\n\
             \n\
             (Session id: newest `logs/polymarket-bot.<session>.log` by mtime; if none, latest round row in DB.)",
            session_id,
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
        let base = format!(
            "📊 Polybot Dashboard (supervisor)\n\
             ━━━━━━━━━━━━━━━━━━━━\n\
             \n\
             {}\n\
             {}\n\
             \n\
             {}\n\
             \n\
             📍 Open positions: live list is in the Mini App (⏱) when the bot is running.\n\
             \n\
             Latest log: {}",
            balance_line,
            bot_line,
            stats,
            last_log_activity_line("logs")
        );
        if webapp_dashboard_url_from_env().is_some() {
            format!(
                "{}\n\n👇 Use the <b>Live dashboard</b> button below for the HTML Mini App.",
                base
            )
        } else {
            format!(
                "{}\n\n<i>No Mini App URL: set TELEGRAM_WEBAPP_PUBLIC_URL in .env (e.g. ngrok) and restart the bot — same as for startup keyboard buttons.</i>",
                base
            )
        }
    }

    async fn handle_command(&mut self, text: &str) -> String {
        let key = normalize_command(text);
        let (running, pid, uptime) = self.refresh_bot_state();

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
             /pnl — latest session P&L from {} (session id shown)\n\
             /stats — latest session stats from DB (session id + note)\n\
             /dashboard — balance + bot status + DB stats + Live dashboard button if TELEGRAM_WEBAPP_PUBLIC_URL is set\n\
             /help — this message\n\
             \n\
             Bot binary: {}\n\
             Child env: DISABLE_TELEGRAM_POLLING=true, POLYBOT_SUPERVISOR_CHILD=1, DISABLE_TUI=true.",
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
        if self.refresh_bot_state().0 {
            return "⚠️ Bot is already running.".to_string();
        }
        let mut cmd = Command::new(&self.bot_command);
        cmd.env("RUST_LOG", "info")
            .env("DISABLE_TELEGRAM_POLLING", "true")
            .env("POLYBOT_SUPERVISOR_CHILD", "1")
            .env("DISABLE_TUI", "true")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => return format!("Failed to spawn bot: {}", e),
        };
        let pid = child.id().unwrap_or(0);
        self.tracked_bot_pid = Some(pid);
        self.bot_process = Some(child);
        self.bot_started_at = Some(Instant::now());
        sleep(Duration::from_secs(3)).await;
        if !self.refresh_bot_state().0 {
            return format!(
                "⚠️ Bot crashed immediately after start (PID was {}). Check logs/.",
                pid
            );
        }
        format!("🟢 Bot started (PID: {})", pid)
    }

    async fn cmd_stop_bot(&mut self) -> String {
        if !self.refresh_bot_state().0 {
            return "⚠️ Bot is not running.".to_string();
        }
        let kill_pid = self.tracked_bot_pid;
        if let Some(mut ch) = self.bot_process.take() {
            self.bot_started_at = None;
            self.tracked_bot_pid = None;
            if let Err(e) = ch.kill().await {
                return format!("Failed to stop bot: {}", e);
            }
            let _ = ch.wait().await;
        } else if let Some(pid) = kill_pid {
            self.bot_started_at = None;
            self.tracked_bot_pid = None;
            #[cfg(windows)]
            {
                let _ = Command::new("taskkill")
                    .args(["/PID", &pid.to_string(), "/F", "/T"])
                    .status()
                    .await;
            }
            #[cfg(not(windows))]
            {
                let _ = Command::new("/bin/kill")
                    .args(["-TERM", &pid.to_string()])
                    .status()
                    .await;
            }
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

    // Same as `Config::from_env()` in main: load .env / polymarket_keys.env so TELEGRAM_* work from files.
    dotenv::dotenv().ok();
    dotenv::from_filename("polymarket_keys.env").ok();

    let mut sup = Supervisor::new();
    if sup.token.trim().is_empty() || sup.chat_id.trim().is_empty() {
        eprintln!("Set TELEGRAM_BOT_TOKEN and TELEGRAM_CHAT_ID");
        std::process::exit(1);
    }

    eprintln!(
        "supervisor: pid={} exe={}",
        std::process::id(),
        std::env::current_exe().unwrap_or_default().display()
    );

    let mut offset: i64 = 0;
    let mut seen: HashSet<i64> = HashSet::new();
    const SEEN_CAP: usize = 2048;
    let mut fail_attempts: u32 = 0;

    sup.telegram_delete_webhook().await;
    sup.telegram_log_webhook_info().await;

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
            let status = resp.status();
            if status.as_u16() == 409 {
                eprintln!(
                    "supervisor: getUpdates 409 Conflict — another getUpdates session exists for this bot token.\n\
                     • On this PC: .\\scripts\\stop-telegram-pollers.ps1 then wait ~5s; or .\\scripts\\scan-telegram-processes.ps1 to find strays.\n\
                     • Elsewhere: stop polymarket-bot/supervisor on EC2 or other machines using the same TELEGRAM_BOT_TOKEN.\n\
                     • Last resort: @BotFather -> /revoke -> put the new token in .env\n\
                     Retrying deleteWebhook + getUpdates in 15s..."
                );
                sup.telegram_delete_webhook().await;
                sup.telegram_log_webhook_info().await;
                sleep(Duration::from_secs(15)).await;
                continue;
            }
            eprintln!("supervisor: getUpdates HTTP {}", status);
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

        // Process in update_id order so /startbot always runs before /dashboard in one batch.
        let mut batch = upd.result;
        batch.sort_by_key(|u| u.update_id);

        let mut max_id: i64 = -1;
        for u in batch {
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
            let key = normalize_command(text.trim());
            let reply = sup.handle_command(text.trim()).await;
            let markup = if key == "/dashboard" {
                telegram_webapp_dashboard_markup()
            } else {
                None
            };
            sup.send_message_with_retry(&reply, markup).await;
        }
        if max_id >= 0 {
            offset = max_id + 1;
        }
    }
}
