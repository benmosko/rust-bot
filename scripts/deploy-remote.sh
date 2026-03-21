#!/usr/bin/env bash
# Consumed by deploy-from-local.ps1 (piped over SSH). Not run on the server by systemd.
set -euo pipefail
cd ~/polymarket-bot
git pull --ff-only
cargo build --release --locked --bin polymarket-bot --bin supervisor
sudo systemctl restart polymarket-supervisor 2>/dev/null || true
sudo systemctl restart polymarket-bot 2>/dev/null || true
echo "---"
systemctl is-active polymarket-supervisor 2>/dev/null || echo "polymarket-supervisor: not installed or inactive"
systemctl is-active polymarket-bot 2>/dev/null || echo "polymarket-bot: not installed or inactive"
