# Polymarket Bot — AI Context

## Project Overview
Rust Polymarket sniper bot with hedge. Runs on AWS EC2 24/7. Trades crypto up/down prediction markets on Polymarket via CLOB + Gamma API.

## Architecture (One Sentence per Module)
- **sniper.rs** — Late-round GTC maker + hedge (shotgun approach, cap fills per round)
- **spread_capture.rs** — Gabagool DCA via FAK limit at best ask
- **momentum.rs** — Binance spot directional (early/mid/late/preemptive modes)
- **market_maker.rs** — Two-sided quoting with inventory skew
- **execution.rs** — Shared CLOB client (place/cancel/poll orders)
- **orderbook.rs** — SDK WebSocket streams + price threshold events
- **market_discovery.rs** — Gamma API fetch + round detection + resolution
- **tui.rs** — Ratatui dashboard (active rounds, round history, trade log)
- **pnl.rs** — Position/P&L tracking, invested vs realized
- **redemption.rs** — CTF redeem (merge/redeem on Polygon)
- **config.rs** — Env vars via dotenv
- **types.rs** — Shared types (Market, Round, RoundHistoryEntry, TuiEvent)
- **strategy_sizing.rs** — Shared sizing (single-leg, dual-leg)
- **risk.rs** — Risk manager (circuit breaker, volatility pause)
- **balance.rs** — USDC balance polling via Polygon RPC
- **spot_feed.rs** — Binance WebSocket kline feed (1m close price)

## Key Env Vars and Defaults

### Wallet
| Var | Default | Notes |
|-----|---------|-------|
| `PK` / `POLYMARKET_PRIVATE_KEY` | (required) | EOA or Safe signer |
| `SIG_TYPE` / `SIGNATURE_TYPE` | `0` | `0`=EOA, `2`=Gnosis Safe |
| `FUNDER` / `FUNDER_ADDRESS` | (signer if empty) | Proxy/Safe address holding USDC |
| `POLYGON_RPC_URL` | `https://polygon-rpc.com` | RPC for balance + redemption |

### Sniper
| Var | Default |
|-----|---------|
| `SNIPE_MIN_BID` | `0.90` |
| `SNIPE_MAX_SPREAD` | `0.03` |
| `SNIPE_MIN_ELAPSED_PCT` | `0.70` |
| `SNIPER_MAX_SHARES` | `10` |
| `SNIPER_CAPITAL_DEPLOY_PCT` | `0.01` |
| `SNIPER_MIN_SHARES` | `3` |
| `SNIPER_MAX_FILLS_PER_ROUND` | `2` |
| `SNIPER_ENTRY_MIN_BEST_ASK` | `0.96` |
| `SNIPER_HEDGE_MAX_PAIR_COST` | `0.99` |
| `SNIPER_ENTRY_WINDOW_5M` | `60` |
| `SNIPER_ENTRY_WINDOW_15M` | `180` |

### Spread Capture (SC_)
| Var | Default |
|-----|---------|
| `SC_CHEAP_SIDE_MAX_ASK` | `0.45` |
| `SC_MAX_PAIR_COST` | `0.98` |
| `SC_MAX_QTY_IMBALANCE` | `10` |
| `SC_MAX_ACTIVE_MARKETS` | `2` |
| `SC_FAK_ORDER_SIZE` | `5` |
| `SC_ORDER_COOLDOWN_MS` | `500` |
| `SC_PREFER_15M` | `true` |
| `SC_MAX_IMBALANCE_PCT` | `0.10` |

### Momentum (MOM_)
| Var | Default |
|-----|---------|
| `MOM_MIN_SPOT_MOVE_PCT` | `0.003` |
| `MOM_LATE_ENTRY_MIN_BID` | `0.85` |
| `MOM_PREEMPTIVE_CANCEL_MS` | `100` |

### Market Making (MM_)
| Var | Default |
|-----|---------|
| `MM_HALF_SPREAD` | `0.02` |
| `MM_VOLATILITY_SPREAD` | `0.04` |
| `MM_MAX_INVENTORY_PER_SIDE` | `500` |
| `MM_INVENTORY_IMBALANCE_LIMIT` | `50` |
| `MM_STOP_BEFORE_END_SECS` | `30` |
| `MM_SPOT_VOL_WINDOW_TICKS` | `30` |
| `MM_SPOT_MOVE_PCT` | `0.01` |

### Risk
| Var | Default |
|-----|---------|
| `MAX_CONCURRENT_ROUNDS` | `4` |
| `DAILY_LOSS_LIMIT_PCT` | `0.20` |
| `CIRCUIT_BREAKER_LOSSES` | `3` |
| `CIRCUIT_BREAKER_PAUSE_SECS` | `1800` |
| `MAX_ROUND_EXPOSURE_PCT` | `0.40` |
| `VOLATILITY_PAUSE_THRESHOLD_PCT` | `0.03` |
| `VOLATILITY_PAUSE_SECS` | `300` |

### Markets
| Var | Default |
|-----|---------|
| `COINS` | `btc,eth,sol,xrp` |
| `PERIODS` | `5,15` |

### Other
| Var | Default |
|-----|---------|
| `CANCEL_REPLACE_TARGET_MS` | `100` |
| `CANCEL_REPLACE_HARD_LIMIT_MS` | `200` |
| `RUST_LOG` | `info` |
| `LOG_FILE` | `bot.log` |
| `STRATEGY_MODE` | `sniper_only` |
| `DRY_RUN` | `true` |

## Build & Run
```powershell
cargo build --release --bin polymarket-bot
$env:RUST_LOG="info"; $env:STRATEGY_MODE="sniper_only"; cargo run --release --bin polymarket-bot
```

## Logs
- Path: `logs/<LOG_FILE>.YYYY-MM-DD` (e.g. `logs/bot.log.2025-03-21`)
- On Windows: rolling file may be UTF-16; tail/parse accordingly

## Wallet
- Proxy/Safe: `0xD6d35B777089235c9CCDcD4830BF1BBda2A06300` (example)
- `SIGNATURE_TYPE=2` for Gnosis Safe

## Current Strategy
- **sniper_only** with hedge
- Shotgun: GTC on all qualifying markets, cap at `SNIPER_MAX_FILLS_PER_ROUND=2` per (Period, round_start)
- Hedge: GTC buy at $0.01 (min price) on other side immediately after primary fill. Skip if `entry_price >= 0.99` or `entry_price + 0.01 > SNIPER_HEDGE_MAX_PAIR_COST`. No orderbook watching.
- Entry: only when `best_ask >= SNIPER_ENTRY_MIN_BEST_ASK` and within entry window (5m vs 15m rounds)

## Known Constraints
- GTC min 5 shares
- FAK min $1 notional
- Market `minimum_order_size=5`
- Event-driven sniper: price threshold events from orderbook; sub-2ms detection latency

## Polymarket API
- CLOB: `https://clob.polymarket.com`
- Gamma: `https://gamma-api.polymarket.com`
- SDK: `polymarket-client-sdk` 0.4 (clob, ws, data)

## Common Gotchas
- **Project path has a space** (`C:\Users\User\Documents\rust bot`): quote the path in PowerShell. Use `cd "C:\Users\User\Documents\rust bot"`. For cargo, set the working directory first — don't inline the path. Always `cd` into the project before running cargo; avoid combining with semicolons.
- PowerShell: `$env:VAR="value"` for env vars (e.g. `$env:RUST_LOG="info"` — `RUST_LOG=info` is bash syntax and fails in PowerShell).
- Must use `--bin polymarket-bot` (extra `test-order` binary exists)
- Log file UTF-16 on Windows
- RUST_LOG=debug → TUI disabled, stdout logging
- polymarket-client-sdk `subscribe_orderbook` takes **asset_ids** (CLOB outcome token IDs, e.g. `up_token_id`/`down_token_id`), not `condition_id`
