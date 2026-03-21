#!/usr/bin/env bash
# Run on EC2 after the agent has published metrics (~2–5 min). Needs: jq, aws CLI, IAM: PutMetricAlarm + ListMetrics.
set -euo pipefail

REGION=$(curl -s --noproxy '*' http://169.254.169.254/latest/meta-data/placement/region)
IID=$(curl -s --noproxy '*' http://169.254.169.254/latest/meta-data/instance-id)
ALARM_NAME="polymarket-bot-disk-used-high-${IID}"

echo "Waiting for CWAgent disk metrics (InstanceId=$IID, path /)..."
for _ in $(seq 1 40); do
  COUNT=$(aws cloudwatch list-metrics --region "$REGION" --namespace CWAgent --metric-name disk_used_percent --output json \
    | jq --arg iid "$IID" '[.Metrics[] | select([.Dimensions[]? | select(.Name=="InstanceId" and .Value==$iid)] | length > 0) and ([.Dimensions[]? | select(.Name=="path" and .Value=="/")] | length > 0)] | length')
  if [[ "${COUNT:-0}" -gt 0 ]]; then
    break
  fi
  sleep 5
done

DIMS=$(aws cloudwatch list-metrics --region "$REGION" --namespace CWAgent --metric-name disk_used_percent --output json \
  | jq -c --arg iid "$IID" '.Metrics[] | select([.Dimensions[]? | select(.Name=="InstanceId" and .Value==$iid)] | length > 0) and ([.Dimensions[]? | select(.Name=="path" and .Value=="/")] | length > 0) | .Dimensions' | head -1)

if [[ -z "$DIMS" || "$DIMS" == "null" ]]; then
  echo "No disk_used_percent (/) metric for this instance yet. Wait for the agent, then re-run."
  exit 1
fi

# AWS CLI misparses Value=/; use %2F for root path.
DIM_ARG=$(echo "$DIMS" | jq -r '.[] | if .Name == "path" and .Value == "/" then "Name=\(.Name),Value=%2F" else "Name=\(.Name),Value=\(.Value)" end' | paste -sd' ' -)

aws cloudwatch put-metric-alarm \
  --region "$REGION" \
  --alarm-name "$ALARM_NAME" \
  --alarm-description "EC2 disk used percent (CWAgent disk_used_percent /)" \
  --metric-name disk_used_percent \
  --namespace CWAgent \
  --statistic Average \
  --period 300 \
  --evaluation-periods 2 \
  --threshold 85 \
  --comparison-operator GreaterThanThreshold \
  --treat-missing-data notBreaching \
  --dimensions "$DIM_ARG"

echo "Alarm created: $ALARM_NAME"
