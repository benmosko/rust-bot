# From Windows: one-shot deploy over SSH (git pull, release build, systemd restart).
# Prereq: repo at ~/polymarket-bot, rustup; polymarket-supervisor.service (always-on) and/or polymarket-bot.service.
#
# Defaults match scripts/verify-server-env.ps1 (override if your instance changes).
# Key: Polybot.pem in repo root (gitignored) — copy from Desktop once if missing.
#
# Usage:
#   .\scripts\deploy-from-local.ps1
#   .\scripts\deploy-from-local.ps1 -HostName ec2-....amazonaws.com -KeyPath C:\path\Polybot.pem
param(
    [string] $HostName = "ec2-3-81-202-238.compute-1.amazonaws.com",
    [string] $KeyPath = "",
    [string] $UserName = "ubuntu"
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$repoRoot = (Resolve-Path (Join-Path $here "..")).Path
if (-not $KeyPath) {
    $KeyPath = Join-Path $repoRoot "Polybot.pem"
}
if (-not (Test-Path -LiteralPath $KeyPath)) {
    Write-Error "SSH key not found: $KeyPath (copy Desktop\Polybot.pem to repo root or pass -KeyPath)"
}

$remote = Join-Path $here "deploy-remote.sh"
Get-Content -Raw $remote | ssh -i $KeyPath -o BatchMode=yes -o StrictHostKeyChecking=accept-new -o ConnectTimeout=30 "${UserName}@${HostName}" "bash -s"
exit $LASTEXITCODE
