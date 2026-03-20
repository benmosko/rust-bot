# Polymarket Rust Trading Bot

Production-grade automated trading bot for Polymarket binary crypto UP/DOWN prediction markets (5m, 15m, 1h for BTC, ETH, SOL, XRP). Implements three proven strategies: **Spread Capture** (gabagool-style dual-side arbitrage), **Momentum** (Binance signal following), and **Market Making** (two-sided liquidity).

## Requirements

- Rust (latest stable): `rustup default stable`
- Polymarket wallet with USDC on Polygon (and optional proxy/safe for `SIGNATURE_TYPE=2`)
- VPS or low-latency environment recommended (<5ms to Polymarket; home WiFi often >150ms and will get adversely selected)

## Build

```bash
cargo build --release
```

## Configuration

1. Copy `.env.example` to `.env` or `polymarket_keys.env` (keys file takes precedence).
2. Set at minimum:
   - `POLYMARKET_PRIVATE_KEY` – Polygon private key (0x-prefixed).
   - `SIGNATURE_TYPE` – `0` for EOA, `2` for Gnosis Safe / proxy wallet.
   - `FUNDER_ADDRESS` – Required when `SIGNATURE_TYPE=2` (proxy/safe address).
3. Adjust strategy and risk parameters as needed (see `.env.example`).

## Run

```bash
cargo run --release
# or
./target/release/polymarket-bot.exe
```

- Logs: JSON to file per `LOG_FILE` (default `polymarket-bot.log`), level from `RUST_LOG` (default `info`).
- State: On graceful shutdown (Ctrl+C), state is written to `state.json` and can be loaded on next start.

## Strategies (all maker-only)

1. **Spread Capture** – Buys YES when YES is cheap, NO when NO is cheap; keeps **pair cost** below target (e.g. 0.97). Profit when pair cost < 1.0 at resolution.
2. **Momentum** – Uses Binance spot to enter in the winning direction (early move, mid-round, or late confirmation); preemptive cancel if spot reverses.
3. **Market Making** – Two-sided quotes; cancel/replace loop target <100ms; stops before round end (e.g. T-30s).

## Important (March 2026)

- **Taker delay removed** – Taker orders execute instantly; cancel/replace must be <100ms.
- **Maker only** – All orders are maker; no taker fees; rebates apply.
- **Fee rate** – Fetched per token via API; SDK includes it in order signatures. Do not hardcode.

## License

Use at your own risk. Not financial advice.
