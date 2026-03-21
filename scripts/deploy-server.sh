#!/usr/bin/env bash
# Run ON the EC2 host (Ubuntu). Clones or updates repo, builds release binary, restarts service.
set -euo pipefail

REPO_URL="${REPO_URL:-}"
INSTALL_DIR="${INSTALL_DIR:-$HOME/polymarket-bot}"
SERVICE_NAME="${SERVICE_NAME:-polymarket-bot}"

cd "$HOME"

if [[ ! -d "$INSTALL_DIR/.git" ]]; then
  if [[ -z "$REPO_URL" ]]; then
    echo "Set REPO_URL to your git remote (HTTPS or SSH), e.g.:"
    echo "  export REPO_URL=https://github.com/you/polymarket-bot.git"
    echo "  curl -fsSL .../deploy-server.sh | bash"
    exit 1
  fi
  git clone "$REPO_URL" "$INSTALL_DIR"
fi

cd "$INSTALL_DIR"
git pull --ff-only

# Rust toolchain must be installed on the server (rustup).
command -v cargo >/dev/null || { echo "Install rust: https://rustup.rs"; exit 1; }

cargo build --release --locked --bin polymarket-bot

if systemctl is-enabled "$SERVICE_NAME" &>/dev/null; then
  sudo systemctl restart "$SERVICE_NAME"
  sudo systemctl --no-pager status "$SERVICE_NAME" || true
else
  echo "Service not installed yet. One-time setup:"
  echo "  sudo cp scripts/polymarket-bot.service /etc/systemd/system/"
  echo "  sudo sed -i \"s|/home/ubuntu|$HOME|g\" /etc/systemd/system/polymarket-bot.service"
  echo "  sudo systemctl daemon-reload && sudo systemctl enable --now polymarket-bot"
fi
