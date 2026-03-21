# From Windows: one-shot deploy over SSH (git pull, release build, systemd restart).
# Prereq: repo at ~/polymarket-bot, rustup; polymarket-supervisor.service (always-on) and/or polymarket-bot.service.
#
# Usage:
#   .\scripts\deploy-from-local.ps1 -HostName ec2-...compute-1.amazonaws.com -KeyPath C:\keys\my.pem
param(
    [Parameter(Mandatory = $true)]
    [string] $HostName,
    [Parameter(Mandatory = $true)]
    [string] $KeyPath,
    [string] $UserName = "ubuntu"
)

$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$remote = Join-Path $here "deploy-remote.sh"
Get-Content -Raw $remote | ssh -i $KeyPath -o StrictHostKeyChecking=accept-new "${UserName}@${HostName}" "bash -s"
