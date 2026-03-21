#!/usr/bin/env bash
# Run once ON the Linux server from the repo root (e.g. ~/polymarket-bot):
#   bash scripts/install-supervisor-service.sh
# Prereq: .env exists with TELEGRAM_BOT_TOKEN and TELEGRAM_CHAT_ID (non-empty).
# Requires: sudo, systemd, release binary built (cargo build --release --bin supervisor).
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

ENV_FILE="${ENV_FILE:-$REPO_ROOT/.env}"
UNIT_SRC="$REPO_ROOT/scripts/polymarket-supervisor.service"
UNIT_DST="/etc/systemd/system/polymarket-supervisor.service"
SERVICE_NAME="polymarket-supervisor"

if [[ ! -f "$ENV_FILE" ]]; then
  echo "Missing $ENV_FILE — copy .env.example to .env and set secrets (at least TELEGRAM_*)."
  exit 1
fi

# Non-empty values (same idea as dotenv; no multiline values)
if ! grep -qE '^[[:space:]]*TELEGRAM_BOT_TOKEN=[^[:space:]]+' "$ENV_FILE"; then
  echo "Set TELEGRAM_BOT_TOKEN in $ENV_FILE (non-empty)."
  exit 1
fi
if ! grep -qE '^[[:space:]]*TELEGRAM_CHAT_ID=[^[:space:]]+' "$ENV_FILE"; then
  echo "Set TELEGRAM_CHAT_ID in $ENV_FILE (non-empty)."
  exit 1
fi

if [[ ! -f "$REPO_ROOT/target/release/supervisor" ]]; then
  echo "Build supervisor first: cd $REPO_ROOT && cargo build --release --bin supervisor"
  exit 1
fi

if [[ ! -f "$UNIT_SRC" ]]; then
  echo "Missing unit file: $UNIT_SRC"
  exit 1
fi

U="$(whoami)"
sudo cp "$UNIT_SRC" "$UNIT_DST"
sudo sed -i "s|/home/ubuntu|$HOME|g" "$UNIT_DST"
sudo sed -i "s|^User=.*|User=$U|" "$UNIT_DST"

if systemctl is-active --quiet polymarket-bot 2>/dev/null; then
  echo ""
  echo "NOTE: polymarket-bot.service is running. Running two full bots is usually wrong."
  echo "      Stop/disable the trader if you only want supervisor + /startbot:"
  echo "      sudo systemctl disable --now polymarket-bot"
  echo ""
fi

sudo systemctl daemon-reload
sudo systemctl enable --now "$SERVICE_NAME"
sudo systemctl --no-pager status "$SERVICE_NAME" || true
echo ""
echo "Enabled $SERVICE_NAME (starts on boot, Restart=always). Logs: journalctl -u $SERVICE_NAME -f"
