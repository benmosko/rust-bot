#!/usr/bin/env bash
# Run on the EC2 host (Ubuntu, ubuntu user). Requires IAM instance profile with CloudWatch agent policy.
set -euo pipefail

CONFIG_SRC="${1:-/home/ubuntu/polymarket-bot/scripts/cloudwatch/amazon-cloudwatch-agent.json}"
CONFIG_DST="/opt/aws/amazon-cloudwatch-agent/etc/amazon-cloudwatch-agent.json"

if [[ $EUID -ne 0 ]]; then
  echo "Run with: sudo bash $0"
  exit 1
fi

if [[ ! -f "$CONFIG_SRC" ]]; then
  echo "Missing agent config at $CONFIG_SRC (clone repo first or pass path to amazon-cloudwatch-agent.json)"
  exit 1
fi

apt-get update -y
apt-get install -y curl jq awscli

DEB="/tmp/amazon-cloudwatch-agent.deb"
curl -fsSL -o "$DEB" "https://s3.amazonaws.com/amazoncloudwatch-agent/ubuntu/amd64/latest/amazon-cloudwatch-agent.deb"
dpkg -i -E "$DEB" || apt-get install -y -f

install -d /opt/aws/amazon-cloudwatch-agent/etc
cp "$CONFIG_SRC" "$CONFIG_DST"
chmod 644 "$CONFIG_DST"

/opt/aws/amazon-cloudwatch-agent/bin/amazon-cloudwatch-agent-ctl \
  -a fetch-config -m ec2 -s -c "file:${CONFIG_DST}"

echo "CloudWatch agent started. Logs: /ec2/polymarket-bot/app (after bot creates log files under ~/polymarket-bot/logs/)."
echo "Run: sudo bash /home/ubuntu/polymarket-bot/scripts/cloudwatch/create-disk-alarm-on-server.sh"
