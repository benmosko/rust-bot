#!/usr/bin/env bash
set -u
echo "=== Paths ==="
for f in "$HOME/polymarket-bot/.env" "$HOME/.env" "/etc/polymarket-bot.env" "/etc/default/polymarket-bot"; do
  if [ -f "$f" ]; then echo "FOUND $f"; else echo "absent $f"; fi
done
echo "=== systemd (polymarket) ==="
if systemctl list-unit-files 2>/dev/null | grep -q polymarket-bot; then
  systemctl cat polymarket-bot 2>/dev/null || true
else
  echo "(no polymarket-bot.service)"
fi
echo "=== Check vars in first existing env file ==="
ENVFILE=""
for f in "$HOME/polymarket-bot/.env" "$HOME/.env" "/etc/polymarket-bot.env" "/etc/default/polymarket-bot"; do
  if [ -f "$f" ]; then ENVFILE="$f"; break; fi
done
if [ -z "${ENVFILE:-}" ]; then
  echo "ENV_FILE=MISSING (no .env in usual locations - copy from PC: scp .env ubuntu@HOST:~/polymarket-bot/.env)"
  exit 1
fi
echo "ENV_FILE=OK path=$ENVFILE"
for k in POLYMARKET_PRIVATE_KEY PK TELEGRAM_BOT_TOKEN TELEGRAM_CHAT_ID POLYGON_RPC_URL FUNDER_ADDRESS; do
  line=$(grep -E "^${k}=" "$ENVFILE" 2>/dev/null | head -1 || true)
  if [ -z "$line" ]; then
    echo "${k}=MISSING"
    continue
  fi
  val="${line#*=}"
  val=$(printf '%s' "$val" | tr -d '\r\n' | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')
  if [ -z "$val" ]; then
    echo "${k}=EMPTY"
  else
    echo "${k}=SET"
  fi
done
