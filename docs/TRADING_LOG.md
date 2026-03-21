# Round History Data Flow

Round History tracks sniper positions for the TUI "Round History" panel and session P&L. Entries are created by the sniper, updated on hedge fill, and resolved by Gamma API.

## Data Structure

```rust
// types.rs
pub struct RoundHistoryEntry {
    pub id: String,              // "condition_id:order_id" for primary
    pub fill_time: DateTime<Utc>,
    pub coin: Coin,
    pub period: Period,
    pub side_label: String,      // "UP" or "DOWN"
    pub entry_price: Decimal,
    pub hedge_price: Option<Decimal>,
    pub hedge_shares: Option<Decimal>,
    pub pair_cost: Option<Decimal>,
    pub shares: Decimal,
    pub pnl: Option<Decimal>,    // None until HEDGED or WON/LOST
    pub status: RoundHistoryStatus,
    pub condition_id: String,
    pub round_start: i64,
    pub round_end: i64,
}

pub enum RoundHistoryStatus {
    Open,    // Primary filled, hedge not yet filled
    Hedged,  // Both legs filled; pnl from (1 - pair_cost) * min(shares, hedge_shares)
    Won,     // Unhedged, Up won (if side UP) or Down won (if side DOWN)
    Lost,    // Unhedged, lost side
}
```

## Where Entries Are Created

**sniper.rs** — `record_sniper_fill_if_new()`

- On primary GTC fill (not hedge):
  1. If `hedge_update_for` or `dual_poll_second_leg` match existing OPEN entry → **UPDATE** that entry (hedge fill → HEDGED)
  2. Else → **PUSH** new `RoundHistoryEntry` with `status: Open`

```rust
// Primary fill path:
let entry = RoundHistoryEntry {
    id: format!("{}:{}", condition_id, order_id),
    status: RoundHistoryStatus::Open,
    pnl: None,
    ...
};
round_history.lock().push(entry);
```

## Where Entries Are Updated (Hedge)

**sniper.rs** — same `record_sniper_fill_if_new()` when hedge fills

- `hedge_rid` identifies the OPEN row to update
- Sets `hedge_price`, `hedge_shares`, `pair_cost`, `pnl`, `status: Hedged`

```rust
// Hedge fill path: find entry by id, update in place
entry.hedge_price = Some(maker_price);
entry.hedge_shares = Some(size_matched);
entry.pair_cost = Some(entry_price + maker_price);
entry.pnl = Some((1 - pair_cost) * matched);
entry.status = RoundHistoryStatus::Hedged;
```

## Where Entries Are Resolved (Unhedged → WON/LOST)

**market_discovery.rs** — `resolve_round_history_open_entries_gamma()`

- **When**: Spawned by main.rs when round expires (`now > round_end + 60`)
- **What**: Polls `GET /markets?condition_id=...` until `closed=true` and `outcomePrices` indicate winner
- **Index 0 = Up, Index 1 = Down**; `"1"` = won
- **Action**: For each OPEN entry with matching `condition_id` + `round_start`:
  - `pnl = pnl_unhedged_if_resolved(up_won)` (win: `(1-entry)*shares`, loss: `-entry*shares`)
  - `status = Won` or `Lost`

```rust
// market_discovery.rs
for e in g.iter_mut() {
    if e.status != RoundHistoryStatus::Open { continue; }
    let pnl = e.pnl_unhedged_if_resolved(up_won);
    e.pnl = Some(pnl);
    e.status = if (up_won && e.side_label == "UP") || (!up_won && e.side_label == "DOWN")
        { RoundHistoryStatus::Won } else { RoundHistoryStatus::Lost };
}
```

- Polls every 30s; exits when all matching OPEN rows resolved or shutdown

## Who Reads Round History

| Consumer | Use |
|----------|-----|
| **main.rs** | `RoundHistoryEntry::sum_session_pnl()` for TUI `PnlUpdate`, `SessionStats` |
| **tui.rs** | Snapshot each render for Round History panel, header P&L, win rate |
| **main.rs PnL logger** | 30s snapshot for log line |

## Session P&L Formula

```rust
// types.rs
pub fn sum_session_pnl(entries: &[RoundHistoryEntry]) -> Decimal {
    entries.iter().filter_map(|e| e.pnl).sum()
}
```

- Only entries with `pnl.is_some()` (HEDGED or WON/LOST) contribute
- OPEN entries contribute 0
