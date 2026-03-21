# Strategy Reference

## Sniper

**Status: Working (primary strategy)**

### Entry Logic
- **When**: Within `SNIPER_ENTRY_WINDOW_5M` or `SNIPER_ENTRY_WINDOW_15M` sec before round end (period-dependent)
- **Gate**: `best_ask >= SNIPER_ENTRY_MIN_BEST_ASK` (0.96) on YES or NO
- **Trigger**: Price threshold events from OrderbookManager (sub-2ms latency) or 100ms fallback
- **Cap**: `SNIPER_MAX_FILLS_PER_ROUND` primary fills per (Period, round_start) across all coins
- **Sizing**: `strategy_sizing::compute_single_leg_sizing(balance, price, max_shares, deploy_pct, min_shares)`
- **Order**: GTC limit at best_ask (or one tick inside); min 5 shares

### Exit / Hedge
- After primary fill: place GTC hedge on other side at `best_ask - tick` if `fill_price + best_ask <= SNIPER_HEDGE_MAX_PAIR_COST`
- Hedge size = primary fill size
- Unhedged positions: resolved by `market_discovery::resolve_round_history_open_entries_gamma` when Gamma reports `closed` + `outcomePrices`

### Config Vars
| Var | Purpose |
|-----|---------|
| `SNIPER_ENTRY_MIN_BEST_ASK` | Min best_ask to consider entry |
| `SNIPER_MAX_FILLS_PER_ROUND` | Cap primary fills per slot |
| `SNIPER_HEDGE_MAX_PAIR_COST` | Pair cost threshold for hedge; 1.00 = disable |
| `SNIPER_ENTRY_WINDOW_5M` | Sec before end for ≤5m rounds |
| `SNIPER_ENTRY_WINDOW_15M` | Sec before end for >5m rounds |
| `SNIPER_MIN_SHARES`, `SNIPER_MAX_SHARES`, `SNIPER_CAPITAL_DEPLOY_PCT` | Sizing |

---

## Spread Capture (Gabagool)

**Status: Partial**

### Entry Logic
- FAK limit at best ask (not GTC)
- First leg: `best_ask <= SC_CHEAP_SIDE_MAX_ASK`
- Second leg: `yes_avg + no_avg <= SC_MAX_PAIR_COST`, `|yes_qty - no_qty| <= SC_MAX_QTY_IMBALANCE`
- Max `SC_MAX_ACTIVE_MARKETS` market-rounds with inventory
- `SC_PREFER_15M=true` → only 15m markets (5m reserved for sniper)

### Sizing / Timing
- `SC_FAK_ORDER_SIZE` shares per tap
- `SC_ORDER_COOLDOWN_MS` between FAK on same side

### Config Vars
| Var | Purpose |
|-----|---------|
| `SC_CHEAP_SIDE_MAX_ASK` | Max ask on first buy |
| `SC_MAX_PAIR_COST` | yes_avg + no_avg cap |
| `SC_MAX_QTY_IMBALANCE` | Max yes/no imbalance |
| `SC_FAK_ORDER_SIZE` | DCA size per FAK |
| `SC_ORDER_COOLDOWN_MS` | Cooldown per side |

### Constraint
- FAK min $1 notional

---

## Momentum

**Status: Skeleton / Partial**

### Modes
- Early momentum: spot move >= `MOM_MIN_SPOT_MOVE_PCT`
- Late entry: `best_bid >= MOM_LATE_ENTRY_MIN_BID`
- Preemptive cancel: `MOM_PREEMPTIVE_CANCEL_MS` before round end

### Config Vars
| Var | Purpose |
|-----|---------|
| `MOM_MIN_SPOT_MOVE_PCT` | Min spot move to enter |
| `MOM_LATE_ENTRY_MIN_BID` | Min bid for late entry |
| `MOM_PREEMPTIVE_CANCEL_MS` | Cancel before round end |

---

## Market Maker

**Status: Skeleton / Partial**

### Logic
- Two-sided quoting around mid
- Spread: `MM_HALF_SPREAD` or `MM_VOLATILITY_SPREAD` when spot volatility exceeds `MM_SPOT_MOVE_PCT`
- Inventory skew via `MM_INVENTORY_IMBALANCE_LIMIT`
- Stop `MM_STOP_BEFORE_END_SECS` before round end

### Config Vars
| Var | Purpose |
|-----|---------|
| `MM_HALF_SPREAD` | Base half-spread |
| `MM_VOLATILITY_SPREAD` | Widened spread when volatile |
| `MM_MAX_INVENTORY_PER_SIDE` | Cap per side |
| `MM_INVENTORY_IMBALANCE_LIMIT` | Skew limit |
| `MM_STOP_BEFORE_END_SECS` | Stop quoting before end |

---

## Strategy Mode

- `sniper_only`: Only sniper
- `gabagool_only`: Only spread capture (respects SC_PREFER_15M)
- `all`: Spread capture (15m if SC_PREFER_15M) + momentum + market maker + sniper
