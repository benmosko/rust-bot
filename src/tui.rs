//! Professional trading dashboard TUI using ratatui + crossterm.
//! Renders portfolio, session stats, active rounds, round history, P&L sparkline, and trade log.

use crate::types::{
    Coin, Period, RoundHistoryEntry, RoundHistoryStatus, TuiData, TuiEvent, TuiRoundRow, TuiSpreadRow,
};
use chrono::Local;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, List, ListItem, Paragraph, Row, Table};
use ratatui::{Frame, Terminal};
use rust_decimal::Decimal;
use std::io;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::{self, MissedTickBehavior};
use tokio_util::sync::CancellationToken;

const TRADE_LOG_LEN: usize = 20;

/// Gaps between Round History columns (ratatui adds `(cols - 1) * spacing` on top of column widths).
const ROUND_HISTORY_COLUMN_SPACING: u16 = 1;

/// Round History: fixed columns + flexible Shares. `usable_width` is inner table width **minus**
/// inter-column gaps so `Length` sums match what `Layout::split` assigns to cells (avoids clipping).
/// `FIXED_MAX` is column total when P&L uses 10 cols and shares 0.
fn round_history_column_constraints(usable_width: u16) -> [Constraint; 9] {
    const W_TIME: u16 = 8;
    const W_COIN: u16 = 6;
    const W_SIDE: u16 = 4;
    const W_PRICE: u16 = 6;
    const W_PNL_MAX: u16 = 10;
    const W_PNL_MIN: u16 = 9;
    const W_ST: u16 = 6;
    // All columns except Shares when P&L uses max width (10).
    const FIXED_MAX: u16 = W_TIME + W_COIN + W_SIDE + 3 * W_PRICE + W_PNL_MAX + W_ST;

    let w = usable_width.max(1);
    if w > FIXED_MAX {
        let share = w - FIXED_MAX;
        [
            Constraint::Length(W_TIME),
            Constraint::Length(W_COIN),
            Constraint::Length(W_SIDE),
            Constraint::Length(W_PRICE),
            Constraint::Length(W_PRICE),
            Constraint::Length(W_PRICE),
            Constraint::Length(share),
            Constraint::Length(W_PNL_MAX),
            Constraint::Length(W_ST),
        ]
    } else if w == FIXED_MAX {
        // Exactly enough for fixed + 1-char shares; use narrow P&L header width
        [
            Constraint::Length(W_TIME),
            Constraint::Length(W_COIN),
            Constraint::Length(W_SIDE),
            Constraint::Length(W_PRICE),
            Constraint::Length(W_PRICE),
            Constraint::Length(W_PRICE),
            Constraint::Length(1),
            Constraint::Length(W_PNL_MIN),
            Constraint::Length(W_ST),
        ]
    } else {
        // Very narrow terminal: proportional; may still clip if w is tiny
        [
            Constraint::Ratio(8, 60),
            Constraint::Ratio(6, 60),
            Constraint::Ratio(4, 60),
            Constraint::Ratio(6, 60),
            Constraint::Ratio(6, 60),
            Constraint::Ratio(6, 60),
            Constraint::Ratio(8, 60),
            Constraint::Ratio(10, 60),
            Constraint::Ratio(6, 60),
        ]
    }
}

/// Shorten long strategy lines so narrow terminals still show useful text.
fn abbrev_active_round_strategy(s: &str) -> String {
    let t = s.trim();
    let t = t.replace("Cap reached", "Capped");
    let t = t.replace("Dual filled, rest cx'd", "Dual+cx");
    if t.chars().count() > 32 {
        t.chars().take(29).collect::<String>() + "…"
    } else {
        t
    }
}

/// Focused panel for keyboard (Tab cycle).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Main,
    RoundHistory,
    TradeLog,
}

/// Run the TUI: alternate screen, raw mode, panic hook, event loop.
/// Reads from `rx`, applies events to internal `TuiData`, renders every tick.
/// Returns when user presses `q` or `shutdown` is cancelled; caller must restore terminal.
pub async fn run_tui(
    mut rx: mpsc::Receiver<TuiEvent>,
    shutdown: CancellationToken,
    dry_run: Arc<AtomicBool>,
    market_slots: Vec<(Coin, Period)>,
    round_history: Arc<Mutex<Vec<RoundHistoryEntry>>>,
    session_start: std::time::Instant,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic| {
        let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
        let _ = disable_raw_mode();
        original_hook(panic);
    }));

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;

    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut data = TuiData::default();
    data.market_slots = market_slots;
    data.session_start = session_start;
    let mut focus = Focus::Main;
    let mut log_scroll: usize = 0;
    let mut round_history_scroll: usize = 0;

    // Frequent redraw so session P&L (sum of round_history `pnl`) updates soon after resolution.
    let mut render_tick = time::interval(Duration::from_millis(250));
    render_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    'tui: loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => break 'tui,
            ev = rx.recv() => {
                match ev {
                    Some(e) => apply_event(&mut data, e),
                    None => break 'tui,
                }
            }
            _ = render_tick.tick() => {}
        }

        if shutdown.is_cancelled() {
            break 'tui;
        }

        while let Ok(ev) = rx.try_recv() {
            apply_event(&mut data, ev);
        }

        let is_dry_run = dry_run.load(Ordering::Relaxed);
        let round_history_snapshot: Vec<RoundHistoryEntry> = round_history
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default();
        terminal.draw(|f| {
            ui(
                f,
                &data,
                focus,
                log_scroll,
                round_history_scroll,
                &round_history_snapshot,
                is_dry_run,
            )
        })?;

        while event::poll(Duration::ZERO)? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match key.code {
                    KeyCode::Char('q') => break 'tui,
                    KeyCode::Char('p') => {
                        data.paused = true;
                        let _ = rx.try_recv(); // ignore stale
                    }
                    KeyCode::Char('r') => data.paused = false,
                    KeyCode::Up => {
                        match focus {
                            Focus::TradeLog => {
                                if log_scroll < data.trade_log.len().saturating_sub(1) {
                                    log_scroll = log_scroll
                                        .saturating_add(1)
                                        .min(data.trade_log.len().saturating_sub(1));
                                }
                            }
                            Focus::RoundHistory => {
                                round_history_scroll = round_history_scroll.saturating_add(1);
                            }
                            Focus::Main => {}
                        }
                    }
                    KeyCode::Down => {
                        match focus {
                            Focus::TradeLog => {
                                if log_scroll > 0 {
                                    log_scroll = log_scroll.saturating_sub(1);
                                }
                            }
                            Focus::RoundHistory => {
                                round_history_scroll = round_history_scroll.saturating_sub(1);
                            }
                            Focus::Main => {}
                        }
                    }
                    KeyCode::Tab => {
                        focus = match focus {
                            Focus::Main => Focus::RoundHistory,
                            Focus::RoundHistory => Focus::TradeLog,
                            Focus::TradeLog => Focus::Main,
                        };
                    }
                    _ => {}
                }
            }
        }
    }

    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    drop(terminal);
    disable_raw_mode()?;
    Ok(())
}

fn apply_event(data: &mut TuiData, ev: TuiEvent) {
    match ev {
        TuiEvent::BalanceUpdate(b) => data.balance = b,
        TuiEvent::PnlUpdate(_) => {
            // Session P&L in the header is never stored here: `ui()` recomputes
            // `RoundHistoryEntry::sum_session_pnl` from the live `round_history` snapshot each draw.
        }
        TuiEvent::SessionStats {
            rounds,
            uptime_secs: _,
            avg_pair_cost,
            rebates,
            session_start_balance,
        } => {
            data.rounds = rounds;
            // uptime is computed on every render from data.session_start, not from events
            data.avg_pair_cost = avg_pair_cost;
            data.rebates = rebates;
            data.session_start_balance = session_start_balance;
        }
        TuiEvent::InvestedUpdate(inv) => data.invested = inv,
        TuiEvent::OpenPosUpdate(n) => data.open_pos = n,
        TuiEvent::Paused(p) => data.paused = p,
        TuiEvent::TradeLog(line) => {
            data.trade_log.insert(0, line);
            if data.trade_log.len() > TRADE_LOG_LEN {
                data.trade_log.pop();
            }
        }
        TuiEvent::RoundUpdate {
            coin,
            period,
            round_start,
            elapsed_pct,
            yes_price,
            no_price,
            strategy,
            status,
        } => {
            let row = TuiRoundRow {
                coin,
                period,
                round_start,
                elapsed_pct,
                yes_price,
                no_price,
                strategy,
                status,
            };
            if let Some(existing) = data.active_rounds.iter_mut().find(|r| r.round_start == round_start && r.coin == coin && r.period == period) {
                *existing = row;
            } else {
                data.active_rounds.insert(0, row);
            }
            data.active_rounds.sort_by(|a, b| b.round_start.cmp(&a.round_start));
            if data.active_rounds.len() > 20 {
                data.active_rounds.truncate(20);
            }
        }
        TuiEvent::SpreadUpdate {
            round_key,
            pair_cost,
            imbalance_pct,
            est_profit,
        } => {
            let row = TuiSpreadRow {
                round_key,
                pair_cost,
                imbalance_pct,
                est_profit,
            };
            if let Some(existing) = data.spread_captures.iter_mut().find(|r| r.round_key == row.round_key) {
                *existing = row;
            } else {
                data.spread_captures.insert(0, row);
            }
            if data.spread_captures.len() > 10 {
                data.spread_captures.truncate(10);
            }
        }
    }
}

fn ui(
    f: &mut Frame,
    data: &TuiData,
    focus: Focus,
    log_scroll: usize,
    round_history_scroll: usize,
    round_history: &[RoundHistoryEntry],
    is_dry_run: bool,
) {
    let area = f.area();

    let chunks = Layout::new(
        Direction::Vertical,
        [
            Constraint::Length(3),
            Constraint::Length(5),
            Constraint::Min(10), // 8 data rows + 1 header row = 9 lines minimum (Min ensures it can grow)
            Constraint::Min(12), // Round History + Trade Log (stacked full-width; needs room for both)
            Constraint::Length(1),
        ],
    )
    .split(area);

    // Title with live clock
    let current_time = Local::now().format("%H:%M:%S").to_string();
    let title_text = if is_dry_run {
        format!(" POLYMARKET BOT [DRY RUN]  {} ", current_time)
    } else {
        format!(" POLYMARKET BOT  {} ", current_time)
    };
    let title = Paragraph::new(title_text)
        .block(Block::default().borders(Borders::ALL).style(Style::default().fg(Color::Cyan)));
    f.render_widget(title, chunks[0]);

    // Row 1: Portfolio + Session Stats
    let row1 = Layout::new(
        Direction::Horizontal,
        [Constraint::Length(24), Constraint::Min(30)],
    )
    .split(chunks[1]);

    let portfolio_block = Block::default()
        .title(" Portfolio ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));
    let balance_str = format!("${:.2}", data.balance);
    let invested_str = format!("${:.2}", data.invested);
    let portfolio_text = vec![
        Line::from(vec![Span::raw("Balance:   "), Span::styled(balance_str, Style::default().fg(Color::White))]),
        Line::from(vec![Span::raw("Invested:  "), Span::styled(invested_str, Style::default().fg(Color::White))]),
        Line::from(vec![
            Span::raw("Open Pos:  "),
            Span::raw(data.open_pos.to_string()),
            Span::raw("  (all strategies)"),
        ]),
    ];
    let portfolio = Paragraph::new(portfolio_text).block(portfolio_block);
    f.render_widget(portfolio, row1[0]);

    // Always sum from the current slice (never from TuiEvent::PnlUpdate).
    let session_pnl = RoundHistoryEntry::sum_session_pnl(round_history);
    let win_rate_pct = RoundHistoryEntry::session_win_rate_pct(round_history);
    let trades_resolved = RoundHistoryEntry::resolved_trades_count(round_history);
    let pnl_pct = if data.session_start_balance > Decimal::ZERO {
        (session_pnl / data.session_start_balance)
            .to_string()
            .parse::<f64>()
            .unwrap_or(0.0)
            * 100.0
    } else {
        0.0
    };

    let pnl_color = if session_pnl >= Decimal::ZERO { Color::Green } else { Color::Red };
    let pnl_str = format!("${:+.2}", session_pnl);
    let pnl_pct_str = format!("{:+.1}%", pnl_pct);
    let uptime_secs = data.session_start.elapsed().as_secs();
    let uptime_str = format_uptime(uptime_secs);
    let avg_cost_str = data
        .avg_pair_cost
        .as_ref()
        .map(|c| format!("${:.3}", c))
        .unwrap_or_else(|| "—".to_string());
    let session_text = vec![
        Line::from(vec![
            Span::raw("P&L: "),
            Span::styled(format!("{} ({})  ", pnl_str, pnl_pct_str), Style::default().fg(pnl_color)),
            Span::raw("Win: "),
            Span::styled(format!("{:.1}%", win_rate_pct), Style::default().fg(Color::White)),
        ]),
        Line::from(vec![
            Span::raw("Trades: "),
            Span::raw(trades_resolved.to_string()),
            Span::raw("  Rounds: "),
            Span::raw(data.rounds.to_string()),
            Span::raw("  Uptime: "),
            Span::raw(uptime_str),
        ]),
        Line::from(vec![
            Span::raw("Avg Pair Cost: "),
            Span::raw(avg_cost_str),
            Span::raw("   Rebates: "),
            Span::styled(format!("${:.1}", data.rebates), Style::default().fg(Color::Green)),
        ]),
    ];
    let session_block = Block::default()
        .title(" Session Stats ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));
    let session = Paragraph::new(session_text).block(session_block);
    f.render_widget(session, row1[1]);

    // Row 2: Active Rounds table - always show all 8 rounds
    let header_cells = ["Market", "Elapsed", "YES", "NO", "Strategy", "Status"]
        .iter()
        .map(|h| Cell::from(*h).style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)));
    let header = Row::new(header_cells).height(1);

    let slots = if data.market_slots.is_empty() {
        // Fallback if caller forgot to pass slots (e.g. tests)
        vec![
            (Coin::Btc, Period::Five),
            (Coin::Btc, Period::Fifteen),
            (Coin::Eth, Period::Five),
            (Coin::Eth, Period::Fifteen),
            (Coin::Sol, Period::Five),
            (Coin::Sol, Period::Fifteen),
            (Coin::Xrp, Period::Five),
            (Coin::Xrp, Period::Fifteen),
        ]
    } else {
        data.market_slots.clone()
    };

    // Get current round_start for each slot (round down to period boundary)
    let now = chrono::Utc::now().timestamp();
    let rows: Vec<Row> = slots
        .iter()
        .map(|(coin, period)| {
            let period_secs = period.as_seconds();
            let round_start = (now / period_secs) * period_secs;
            
            // Find matching round in data, or use defaults
            let round_data = data.active_rounds.iter()
                .find(|r| r.coin == *coin && r.period == *period && r.round_start == round_start)
                .cloned();
            
            let (elapsed_pct, yes_price, no_price, strategy, status) = if let Some(r) = round_data {
                (r.elapsed_pct, r.yes_price, r.no_price, r.strategy, r.status)
            } else {
                // Round not discovered yet - show waiting
                let elapsed = (now - round_start) as f64 / period_secs as f64;
                (elapsed.max(0.0).min(1.0), rust_decimal::Decimal::ZERO, rust_decimal::Decimal::ZERO, "—".to_string(), "Waiting".to_string())
            };
            let strategy = abbrev_active_round_strategy(&strategy);

            let market = format!("{} {}m", coin.as_str().to_uppercase(), period.as_minutes());
            let pct = (elapsed_pct * 10.0).round() as usize;
            let bar: String = (0..10).map(|i| if i < pct { '█' } else { '░' }).collect();
            // Show "N/A" if price is zero but market is Active (orderbook not populated yet)
            // Only show "$0.00" if market is Waiting (not discovered yet)
            let yes_s = if yes_price.is_zero() {
                if status == "Waiting" {
                    "$0.00".to_string()
                } else {
                    "N/A".to_string()
                }
            } else {
                format!("${:.2}", yes_price)
            };
            let no_s = if no_price.is_zero() {
                if status == "Waiting" {
                    "$0.00".to_string()
                } else {
                    "N/A".to_string()
                }
            } else {
                format!("${:.2}", no_price)
            };
            Row::new(vec![
                Cell::from(market),
                Cell::from(bar),
                Cell::from(yes_s),
                Cell::from(no_s),
                Cell::from(strategy.clone()),
                Cell::from(status.clone()),
            ])
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Min(9),  // "SOL 15m"
            Constraint::Min(12), // elapsed bar
            Constraint::Min(8),
            Constraint::Min(8),
            Constraint::Min(24), // strategy / sniper status
            Constraint::Min(10), // Active / Waiting / Ended
        ],
    )
        .column_spacing(0)
        .header(header)
        .block(Block::default().title(" Active Rounds ").borders(Borders::ALL).border_style(Style::default().fg(Color::DarkGray)));
    f.render_widget(table, chunks[2]);

    // Row 3: Round History (full width) + Trade Log below — avoids squeezing the history table
    // (50/50 side-by-side made 9 columns + spacing too narrow; time clipped).
    let row3 = Layout::new(
        Direction::Vertical,
        [Constraint::Percentage(52), Constraint::Percentage(48)],
    )
    .split(chunks[3]);

    let rh_block = Block::default()
        .title(" Round History (sniper) ")
        .borders(Borders::ALL)
        .border_style(if focus == Focus::RoundHistory {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::DarkGray)
        });
    let rh_inner = rh_block.inner(row3[0]);
    let rh_split = Layout::new(
        Direction::Vertical,
        [Constraint::Min(2), Constraint::Length(1)],
    )
    .split(rh_inner);

    let rh_header = Row::new(
        [
            "Time",
            "Coin",
            "Side",
            "Entry",
            "Hedge",
            "Pair",
            "Shares",
            "P&L",
            "Status",
        ]
        .iter()
        .map(|h| Cell::from(*h).style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))),
    )
    .height(1);

    let table_area = rh_split[0];
    let visible_body = (table_area.height as usize).saturating_sub(1).max(1);
    let max_rh_scroll = round_history.len().saturating_sub(visible_body);
    // Vec is chronological (oldest → newest). Default window is the newest segment; scroll up (↑)
    // moves toward older entries. Rows render newest-first (top = latest fill).
    let rh_skip = max_rh_scroll.saturating_sub(round_history_scroll.min(max_rh_scroll));

    let rh_rows: Vec<Row> = round_history
        .iter()
        .skip(rh_skip)
        .take(visible_body)
        .rev()
        .map(|e| {
            let t = e
                .fill_time
                .with_timezone(&Local)
                .format("%H:%M:%S")
                .to_string();
            let coin_s = format!("{}{}m", e.coin.as_str().to_uppercase(), e.period.as_minutes());
            let entry_s = format!("${:.2}", e.entry_price);
            let hedge_s = e
                .hedge_price
                .map(|p| format!("${:.2}", p))
                .unwrap_or_else(|| "—".to_string());
            let pair_s = e
                .pair_cost
                .map(|p| format!("${:.2}", p))
                .unwrap_or_else(|| "—".to_string());
            let sh = e.shares.normalize().to_string();
            let pnl_final = match e.pnl {
                None => "—".to_string(),
                Some(p) if p >= Decimal::ZERO => format!("+${:.2}", p),
                Some(p) => format!("-${:.2}", p.abs()),
            };
            let status_s = match e.status {
                RoundHistoryStatus::Open => "OPEN",
                RoundHistoryStatus::Hedged => "HEDGED",
                RoundHistoryStatus::Won => "WON",
                RoundHistoryStatus::Lost => "LOST",
            };
            Row::new(vec![
                Cell::from(t),
                Cell::from(coin_s),
                Cell::from(e.side_label.clone()),
                Cell::from(entry_s),
                Cell::from(hedge_s),
                Cell::from(pair_s),
                Cell::from(sh),
                Cell::from(pnl_final).style(
                    if e.pnl.is_none() {
                        Style::default().fg(Color::DarkGray)
                    } else if e.pnl.is_some_and(|p| p >= Decimal::ZERO) {
                        Style::default().fg(Color::Green)
                    } else {
                        Style::default().fg(Color::Red)
                    },
                ),
                Cell::from(status_s),
            ])
        })
        .collect();

    let total_round_pnl: Decimal = round_history.iter().filter_map(|e| e.pnl).sum();
    let total_line = format!(
        " Total: {}{:.2}",
        if total_round_pnl >= Decimal::ZERO {
            "+$"
        } else {
            "-$"
        },
        total_round_pnl.abs()
    );

    let rh_gaps = (9usize - 1) as u16 * ROUND_HISTORY_COLUMN_SPACING;
    let rh_usable = table_area.width.saturating_sub(rh_gaps).max(1);
    let rh_table = Table::new(rh_rows, round_history_column_constraints(rh_usable))
        .column_spacing(ROUND_HISTORY_COLUMN_SPACING)
        .header(rh_header);
    f.render_widget(rh_table, table_area);

    let total_widget = Paragraph::new(total_line).style(
        if total_round_pnl >= Decimal::ZERO {
            Style::default().fg(Color::Green)
        } else {
            Style::default().fg(Color::Red)
        },
    );
    f.render_widget(total_widget, rh_split[1]);
    f.render_widget(rh_block, row3[0]);

    // Trade Log (full width under Round History)
    let log_items: Vec<ListItem> = data
        .trade_log
        .iter()
        .skip(log_scroll)
        .take(20)
        .map(|s| ListItem::new(s.as_str()))
        .collect();
    let log_list = List::new(log_items)
        .block(Block::default().title(" Trade Log (last 20) ").borders(Borders::ALL).border_style(if focus == Focus::TradeLog { Style::default().fg(Color::Yellow) } else { Style::default().fg(Color::DarkGray) }));
    f.render_widget(log_list, row3[1]);

    // Footer
    let footer = Paragraph::new(" q: quit │ p: pause │ r: resume │ ↑↓: scroll │ tab: focus │ open pos = all strategies; history = sniper ")
        .style(Style::default().fg(Color::DarkGray));
    f.render_widget(footer, chunks[4]);
}

fn format_uptime(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    format!("{}h{}m{}s", h, m, s)
}
