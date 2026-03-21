# Run from your PC after the CloudWatch agent has published disk_used_percent metrics.
# Note: path dimension "/" must be passed as Value=%2F or AWS CLI misparses --dimensions.
param(
    [Parameter(Mandatory = $true)]
    [string] $InstanceId,
    [string] $Region = "us-east-1"
)

$metricsJson = aws cloudwatch list-metrics --region $Region --namespace CWAgent --metric-name disk_used_percent --output json
if ($LASTEXITCODE -ne 0) { throw "list-metrics failed" }

$m = $metricsJson | ConvertFrom-Json
$match = $m.Metrics | Where-Object {
    $hasId = $_.Dimensions | Where-Object { $_.Name -eq "InstanceId" -and $_.Value -eq $InstanceId }
    $root = $_.Dimensions | Where-Object { $_.Name -eq "path" -and $_.Value -eq "/" }
    $hasId -and $root
} | Select-Object -First 1

if (-not $match) {
    throw "No CWAgent disk_used_percent (path /) for $InstanceId yet. Wait for the agent, then retry."
}

$dimParts = @()
foreach ($d in $match.Dimensions) {
    $v = $d.Value
    if ($v -eq "/") { $v = "%2F" }
    $dimParts += "Name=$($d.Name),Value=$v"
}
$dimArg = $dimParts -join " "

$alarmName = "polymarket-bot-disk-used-high-$InstanceId"
aws cloudwatch put-metric-alarm `
  --region $Region `
  --alarm-name $alarmName `
  --alarm-description "EC2 disk used percent (CWAgent root /)" `
  --metric-name disk_used_percent `
  --namespace CWAgent `
  --statistic Average `
  --period 300 `
  --evaluation-periods 2 `
  --threshold 85 `
  --comparison-operator GreaterThanThreshold `
  --treat-missing-data notBreaching `
  --dimensions $dimArg

if ($LASTEXITCODE -ne 0) { throw "put-metric-alarm failed" }
Write-Host "Created or updated alarm: $alarmName"
