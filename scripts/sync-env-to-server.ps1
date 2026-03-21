# Copy local project .env to ~/polymarket-bot/.env on EC2 (chmod 600). Does NOT commit anything.
# Usage: .\scripts\sync-env-to-server.ps1
param(
    [string] $HostName = "ec2-3-81-202-238.compute-1.amazonaws.com",
    [string] $KeyPath = (Join-Path $env:USERPROFILE "Desktop\polymarket-bot-key.pem"),
    [string] $UserName = "ubuntu",
    [string] $LocalEnv = (Join-Path (Split-Path -Parent $PSScriptRoot) ".env")
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

if (-not (Test-Path -LiteralPath $LocalEnv)) {
    Write-Error "Local .env not found: $LocalEnv"
}
if (-not (Test-Path -LiteralPath $KeyPath)) {
    Write-Error "SSH key not found: $KeyPath"
}

$remotePath = "/home/ubuntu/polymarket-bot/.env"
Write-Host "scp $LocalEnv -> ${UserName}@${HostName}:$remotePath"
scp -i $KeyPath -o BatchMode=yes -o StrictHostKeyChecking=accept-new -o ConnectTimeout=25 `
    $LocalEnv "${UserName}@${HostName}:$remotePath"

ssh -i $KeyPath -o BatchMode=yes -o StrictHostKeyChecking=accept-new -o ConnectTimeout=25 `
    "${UserName}@${HostName}" "chmod 600 $remotePath && echo OK"

Write-Host "Done. Run: .\scripts\verify-server-env.ps1"
