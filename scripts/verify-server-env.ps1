# SSH to EC2 and report which required env vars are present/non-empty.
# Never prints secret values. Defaults: key on Desktop, host from AWS (override if IP changes).
#
# Usage:
#   .\scripts\verify-server-env.ps1
#   .\scripts\verify-server-env.ps1 -HostName ec2-....amazonaws.com
param(
    [string] $HostName = "ec2-3-81-202-238.compute-1.amazonaws.com",
    [string] $KeyPath = (Join-Path $env:USERPROFILE "Desktop\polymarket-bot-key.pem"),
    [string] $UserName = "ubuntu"
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

if (-not (Test-Path -LiteralPath $KeyPath)) {
    Write-Error "SSH key not found: $KeyPath (copy polymarket-bot-key.pem to Desktop or pass -KeyPath)"
}

$remoteSh = Join-Path $PSScriptRoot "verify-server-env.remote.sh"
if (-not (Test-Path -LiteralPath $remoteSh)) {
    Write-Error "Missing $remoteSh"
}

$remoteTmp = "/tmp/verify-server-env.sh"
Write-Host "Connecting ${UserName}@${HostName} (key: $KeyPath) ..."
scp -i $KeyPath -o BatchMode=yes -o StrictHostKeyChecking=accept-new -o ConnectTimeout=25 `
    $remoteSh "${UserName}@${HostName}:${remoteTmp}"

$remoteCmd = @'
chmod +x /tmp/verify-server-env.sh && bash /tmp/verify-server-env.sh; ec=$?; rm -f /tmp/verify-server-env.sh; exit $ec
'@
ssh -i $KeyPath -o BatchMode=yes -o StrictHostKeyChecking=accept-new -o ConnectTimeout=25 `
    "${UserName}@${HostName}" $remoteCmd
exit $LASTEXITCODE
