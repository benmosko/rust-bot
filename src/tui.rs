//! Professional trading dashboard TUI using ratatui + crossterm.
//! Renders portfolio, session stats, active rounds, spread capture, P&L sparkline, and trade log.

use crate::types::{TuiData, TuiEvent, TuiRoundRow, TuiSpreadRow};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, List, ListItem, Paragraph, Row, Sparkline, Table};
use ratatui::{Frame, Terminal};
use rust_decimal::Decimal;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const PNL_HISTORY_LEN: usize = 60;
const TRADE_LOG_LEN: usize = 20;
const TICK_MS: u64 = 500;

/// Writer that forwards each log line to the TUI trade log via TuiEvent::TradeLog.
/// Used as a tracing_subscriber writer so that log output appears in the dashboard.
#[allow(dead_code)]
pub struct TuiWriter {
    inner: Mutex<TuiWriterInner>,
}

struct TuiWriterInner {
    buf: Vec<u8>,
    tx: mpsc::Sender<TuiEvent>,
}

impl TuiWriter {
    #[allow(dead_code)]
    pub fn new(tx: mpsc::Sender<TuiEvent>) -> Self {
        Self {
            inner: Mutex::new(TuiWriterInner {
                buf: Vec::new(),
                tx,
            }),
        }
    }
}

impl Write for TuiWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut inner = self.inner.lock().map_err(|_| std::io::ErrorKind::Other)?;
        inner.buf.extend_from_slice(buf);
        while let Some(i) = inner.buf.iter().position(|&b| b == b'\n') {
            let line = String::from_utf8_lossy(&inner.buf[..i]).trim().to_string();
            inner.buf.drain(..=i);
            if !line.is_empty() {
                let _ = inner.tx.try_send(TuiEvent::TradeLog(line));
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Writer returned by the closure for tracing_subscriber; implements Write.
pub(crate) struct TuiWriterRef(Arc<Mutex<TuiWriterInner>>);

impl Write for TuiWriterRef {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut inner = self.0.lock().map_err(|_| std::io::ErrorKind::Other)?;
        inner.buf.extend_from_slice(buf);
        while let Some(i) = inner.buf.iter().position(|&b| b == b'\n') {
            let line = String::from_utf8_lossy(&inner.buf[..i]).trim().to_string();
            inner.buf.drain(..=i);
            if !line.is_empty() {
                let _ = inner.tx.try_send(TuiEvent::TradeLog(line));
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Returns a closure that implements `MakeWriter` for the tracing layer:
/// each call returns a writer that forwards log lines to the TUI trade log.
pub fn tui_writer_layer(tx: mpsc::Sender<TuiEvent>) -> impl Fn() -> TuiWriterRef + Send + 'static {
    let arc = Arc::new(Mutex::new(TuiWriterInner {
        buf: Vec::new(),
        tx,
    }));
    move || TuiWriterRef(arc.clone())
}

/// Focused panel for keyboard (Tab cycle).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Main,
    TradeLog,
}

/// Run the TUI: alternate screen, raw mode, panic hook, event loop.
/// Reads from `rx`, applies events to internal `TuiData`, renders every tick.
/// Returns when user presses `q` or `shutdown` is cancelled; caller must restore terminal.
pub async fn run_tui(
    mut rx: mpsc::Receiver<TuiEvent>,
    shutdown: CancellationToken,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic| {
        let _ = execute!(std::io::stderr(), LeaveAlternateScreen);
        let _ = disable_raw_mode();
        original_hook(panic);
    }));

    enable_raw_mode()?;
    let mut stdout = io::stderr();
    execute!(stdout, EnterAlternateScreen)?;

    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut data = TuiData::default();
    let mut focus = Focus::Main;
    let mut log_scroll: usize = 0;

    loop {
        if shutdown.is_cancelled() {
            break;
        }

        // Drain all pending TuiEvents (non-blocking)
        while let Ok(ev) = rx.try_recv() {
            apply_event(&mut data, ev);
        }

        terminal.draw(|f| ui(f, &data, focus, log_scroll))?;

        // Poll for input with 500ms timeout
        if event::poll(Duration::from_millis(TICK_MS))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match key.code {
                    KeyCode::Char('q') => break,
                    KeyCode::Char('p') => {
                        data.paused = true;
                        let _ = rx.try_recv(); // ignore stale
                    }
                    KeyCode::Char('r') => data.paused = false,
                    KeyCode::Up => {
                        if focus == Focus::TradeLog && log_scroll < data.trade_log.len().saturating_sub(1) {
                            log_scroll = log_scroll.saturating_add(1).min(data.trade_log.len().saturating_sub(1));
                        }
                    }
                    KeyCode::Down => {
                        if focus == Focus::TradeLog && log_scroll > 0 {
                            log_scroll = log_scroll.saturating_sub(1);
                        }
                    }
                    KeyCode::Tab => {
                        focus = match focus {
                            Focus::Main => Focus::TradeLog,
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
        TuiEvent::PnlUpdate(pnl) => {
            data.pnl = pnl;
            let v = pnl.to_string().parse::<f64>().unwrap_or(0.0);
            data.pnl_history.push(v);
            if data.pnl_history.len() > PNL_HISTORY_LEN {
                data.pnl_history.remove(0);
            }
        }
        TuiEvent::SessionStats {
            trades,
            rounds,
            win_rate_pct,
            pnl_pct,
            uptime_secs,
            avg_pair_cost,
            rebates,
        } => {
            data.trades = trades;
            data.rounds = rounds;
            data.win_rate_pct = win_rate_pct;
            data.pnl_pct = pnl_pct;
            data.uptime_secs = uptime_secs;
            data.avg_pair_cost = avg_pair_cost;
            data.rebates = rebates;
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

fn ui(f: &mut Frame, data: &TuiData, focus: Focus, log_scroll: usize) {
    let area = f.area();

    let chunks = Layout::new(
        Direction::Vertical,
        [
            Constraint::Length(3),
            Constraint::Length(5),
            Constraint::Length(8),
            Constraint::Length(6),
            Constraint::Min(6),
            Constraint::Length(1),
        ],
    )
    .split(area);

    // Title
    let title = Paragraph::new(" POLYMARKET BOT ")
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
        Line::from(vec![Span::raw("Open Pos:  "), Span::raw(data.open_pos.to_string())]),
    ];
    let portfolio = Paragraph::new(portfolio_text).block(portfolio_block);
    f.render_widget(portfolio, row1[0]);

    let pnl_color = if data.pnl >= Decimal::ZERO { Color::Green } else { Color::Red };
    let pnl_str = format!("{:+}", data.pnl);
    let pnl_pct_str = format!("{:+.1}%", data.pnl_pct);
    let uptime_str = format_uptime(data.uptime_secs);
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
            Span::styled(format!("{:.1}%", data.win_rate_pct), Style::default().fg(Color::White)),
        ]),
        Line::from(vec![
            Span::raw("Trades: "),
            Span::raw(data.trades.to_string()),
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

    // Row 2: Active Rounds table
    let header_cells = ["Market", "Elapsed", "YES", "NO", "Strategy", "Status"]
        .iter()
        .map(|h| Cell::from(*h).style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)));
    let header = Row::new(header_cells).height(1);

    let rows: Vec<Row> = data
        .active_rounds
        .iter()
        .take(8)
        .map(|r| {
            let market = format!("{} {}m", r.coin.as_str().to_uppercase(), r.period.as_minutes());
            let pct = (r.elapsed_pct * 10.0).round() as usize;
            let bar: String = (0..10).map(|i| if i < pct { '█' } else { '░' }).collect();
            let yes_s = format!("${:.2}", r.yes_price);
            let no_s = format!("${:.2}", r.no_price);
            Row::new(vec![
                Cell::from(market),
                Cell::from(bar),
                Cell::from(yes_s),
                Cell::from(no_s),
                Cell::from(r.strategy.clone()),
                Cell::from(r.status.clone()),
            ])
        })
        .collect();

    let table = Table::new(rows, [Constraint::Length(10), Constraint::Length(12), Constraint::Length(8), Constraint::Length(8), Constraint::Length(12), Constraint::Length(10)])
        .header(header)
        .block(Block::default().title(" Active Rounds ").borders(Borders::ALL).border_style(Style::default().fg(Color::DarkGray)));
    f.render_widget(table, chunks[2]);

    // Row 3: Spread Capture + P&L Chart
    let row3 = Layout::new(
        Direction::Horizontal,
        [Constraint::Percentage(50), Constraint::Percentage(50)],
    )
    .split(chunks[3]);

    let spread_header = Row::new(vec![
        Cell::from("Round").style(Style::default().fg(Color::Cyan)),
        Cell::from("PairCost").style(Style::default().fg(Color::Cyan)),
        Cell::from("Δ").style(Style::default().fg(Color::Cyan)),
        Cell::from("Est P").style(Style::default().fg(Color::Cyan)),
    ]);
    let spread_rows: Vec<Row> = data
        .spread_captures
        .iter()
        .take(5)
        .map(|s| {
            let delta = format!("{:.0}%", s.imbalance_pct * 100.0);
            let est = format!("+${:.2}", s.est_profit);
            Row::new(vec![
                Cell::from(s.round_key.clone()),
                Cell::from(format!("${:.3}", s.pair_cost)),
                Cell::from(delta),
                Cell::from(est),
            ])
        })
        .collect();
    let spread_table = Table::new(spread_rows, [Constraint::Min(12), Constraint::Length(8), Constraint::Length(4), Constraint::Length(8)])
        .header(spread_header)
        .block(Block::default().title(" Spread Capture ").borders(Borders::ALL).border_style(Style::default().fg(Color::DarkGray)));
    f.render_widget(spread_table, row3[0]);

    let spark_data: Vec<u64> = if data.pnl_history.is_empty() {
        vec![0]
    } else {
        let min = data.pnl_history.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = data.pnl_history.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let range = (max - min).max(1.0);
        data.pnl_history
            .iter()
            .map(|v| (((v - min) / range) * 8.0).round() as u64)
            .collect()
    };
    let sparkline = Sparkline::default()
        .data(spark_data.as_slice())
        .block(Block::default().title(" P&L Chart ").borders(Borders::ALL).border_style(Style::default().fg(Color::DarkGray)))
        .style(Style::default().fg(Color::Green));
    f.render_widget(sparkline, row3[1]);

    // Trade Log (chunks[4])
    let log_items: Vec<ListItem> = data
        .trade_log
        .iter()
        .skip(log_scroll)
        .take(20)
        .map(|s| ListItem::new(s.as_str()))
        .collect();
    let log_list = List::new(log_items)
        .block(Block::default().title(" Trade Log (last 20) ").borders(Borders::ALL).border_style(if focus == Focus::TradeLog { Style::default().fg(Color::Yellow) } else { Style::default().fg(Color::DarkGray) }));
    f.render_widget(log_list, chunks[4]);

    // Footer
    let footer = Paragraph::new(" q: quit │ p: pause │ r: resume │ ↑↓: scroll log │ tab: cycle focus ")
        .style(Style::default().fg(Color::DarkGray));
    f.render_widget(footer, chunks[5]);
}

fn format_uptime(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    format!("{}h{}m", h, m)
}
