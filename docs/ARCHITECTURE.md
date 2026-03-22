# Architecture

## Module Dependency Graph

```
main.rs
├── config (Config::from_env)
├── execution (ExecutionEngine) ──► polymarket-client-sdk (CLOB)
├── balance (BalanceManager) ──► alloy (Polygon RPC)
├── risk (RiskManager)
├── orderbook (OrderbookManager) ──► polymarket-client-sdk (ws)
├── market_discovery (fetch_market, resolve_round_history) ──► reqwest (Gamma API)
├── pnl (PnLManager)
├── redemption (RedemptionManager) ──► alloy (CTF)
├── tui (run_tui) ──► ratatui, crossterm
├── spot_feed (SpotFeed) ──► tokio-tungstenite (Binance WS)
│
├── sniper ──► config, execution, orderbook, risk, pnl, strategy_sizing, types
├── spread_capture ──► config, execution, orderbook, pnl, types
├── momentum ──► config, execution, orderbook, risk, spot_feed, pnl, strategy_sizing, types
├── market_maker ──► config, execution, orderbook, spot_feed, pnl, strategy_sizing, types
```

## Data Flow

### WebSocket → Orderbook → Sniper → Execution → CLOB

1. **OrderbookManager** subscribes to CLOB WebSocket per token (`subscribe(token_id)`)
2. SDK emits `BookUpdate` → `OrderbookManager` updates DashMap and:
   - broadcasts `book_update_tx` (token_id) for spread capture
   - if `best_ask >= sniper_entry_min_best_ask`, broadcasts `price_event_tx` (PriceThresholdEvent)
3. **Sniper** receives `price_event_rx` (or 100ms fallback) → evaluates entry gates
4. **Sniper** calls `execution.place_order()` → CLOB POST
5. **ExecutionEngine** holds nonce behind Mutex for EIP-712

### Main Loop (500ms tick)

1. Phase 1: Push `TuiEvent::RoundUpdate` for each slot (prices from orderbook)
2. Phase 2: Parallel Gamma fetch for slots missing `active_rounds`; on success:
   - Insert round → init orderbook → subscribe WS → spawn strategy task(s)
3. Phase 3: Prefetch next round when <30s left; expire rounds >60s past `round_end`:
   - Spawn `resolve_round_history_open_entries_gamma` for OPEN entries
   - Cancel GTC orders, clear `pnl_manager` positions

### TUI Event Channel

- **Single mpsc::channel(4096)** from all strategies + balance loop + main loop
- **TuiEvent** variants: `BalanceUpdate`, `RoundUpdate`, `TradeLog`, `PnlUpdate`, `SessionStats`, etc.
- TUI reads `rx.recv()` and `apply_event()`; render every 1s tick
- `round_history` passed as `Arc<Mutex<Vec<RoundHistoryEntry>>>` — TUI reads snapshot each render

### Shared State

| Shared | Type | Writers | Readers |
|--------|------|---------|---------|
| `round_history` | `Arc<Mutex<Vec<RoundHistoryEntry>>>` | sniper (push/update), market_discovery (resolve) | main, TUI, PnL logger |
| `gtc_filled_by_round` | `DashMap<(Coin,Period,i64), AtomicU32>` | sniper on primary fill (per market) | sniper, main (cleanup) |
| `active_gtc_orders` | `DashMap<order_id, (Execution, slug, Period, round_start)>` | sniper (insert on place, remove on cancel/fill) | main (cancel on expiry) |
| `trade_history` | `DashMap<order_id, FilledTrade>` | strategies on fill | (PnL/redemption reference) |
| `strategy_status` | `DashMap<String, String>` | each strategy | main (TUI round display) |
