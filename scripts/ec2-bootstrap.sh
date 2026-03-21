#!/bin/bash
# EC2 user-data: tools + rustup + repo clone. Does NOT build or start the bot.
set -euo pipefail
exec > >(tee /var/log/polymarket-bot-bootstrap.log) 2>&1
export DEBIAN_FRONTEND=noninteractive
apt-get update -y
apt-get install -y git curl build-essential pkg-config libssl-dev

if [[ ! -f /swapfile ]]; then
  fallocate -l 2G /swapfile 2>/dev/null || dd if=/dev/zero of=/swapfile bs=1M count=2048
  chmod 600 /swapfile
  mkswap /swapfile
  swapon /swapfile
  echo '/swapfile none swap sw 0 0' >> /etc/fstab
fi

sudo -u ubuntu bash <<'EOS'
set -euo pipefail
export HOME=/home/ubuntu
cd "$HOME"
if [[ ! -d "$HOME/.cargo" ]]; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
fi
# shellcheck source=/dev/null
source "$HOME/.cargo/env"
if [[ ! -d "$HOME/polymarket-bot/.git" ]]; then
  git clone https://github.com/benmosko/rust-bot.git "$HOME/polymarket-bot"
else
  cd "$HOME/polymarket-bot" && git pull --ff-only
fi
date -u > "$HOME/polymarket-bot-bootstrap-done.txt"
EOS

chown ubuntu:ubuntu /home/ubuntu/polymarket-bot-bootstrap-done.txt 2>/dev/null || true
