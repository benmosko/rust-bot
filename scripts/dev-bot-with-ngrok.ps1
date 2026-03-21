# Start ngrok (HTTPS tunnel) + polymarket-bot with TELEGRAM_WEBAPP_PUBLIC_URL set automatically.
# One-time: install ngrok (https://ngrok.com/download) and run: ngrok config add-authtoken YOUR_TOKEN
#
# Usage (from repo root):
#   .\scripts\dev-bot-with-ngrok.ps1
#
# Requires: ngrok in PATH, .env with TELEGRAM_BOT_TOKEN + TELEGRAM_CHAT_ID
$ErrorActionPreference = "Stop"
$ProjectRoot = Split-Path -Parent $PSScriptRoot
Set-Location $ProjectRoot

function Import-DotEnv {
    param([string]$Path)
    if (-not (Test-Path -LiteralPath $Path)) { return }
    Get-Content -LiteralPath $Path | ForEach-Object {
        $line = $_.Trim()
        if ($line -match '^\s*#' -or $line -eq '') { return }
        $i = $line.IndexOf('=')
        if ($i -lt 1) { return }
        $k = $line.Substring(0, $i).Trim()
        $v = $line.Substring($i + 1).Trim()
        [Environment]::SetEnvironmentVariable($k, $v, "Process")
    }
}

Import-DotEnv (Join-Path $ProjectRoot ".env")

$port = 8787
if ($env:TELEGRAM_WEBAPP_BIND) {
    if ($env:TELEGRAM_WEBAPP_BIND -match ':(\d+)\s*$') {
        $port = [int]$Matches[1]
    }
}
$env:TELEGRAM_WEBAPP_BIND = "127.0.0.1:$port"

$ngrok = Get-Command ngrok -ErrorAction SilentlyContinue
if (-not $ngrok) {
    Write-Host "ngrok not found in PATH." -ForegroundColor Red
    Write-Host "Install: https://ngrok.com/download  or: winget install ngrok.ngrok"
    Write-Host "Then: ngrok config add-authtoken <token>   (free account: https://dashboard.ngrok.com )"
    exit 1
}

Write-Host "Starting ngrok -> http 127.0.0.1:$port ..." -ForegroundColor Cyan
$ngrokProc = Start-Process -FilePath "ngrok" -ArgumentList @("http", "$port") -PassThru -WindowStyle Hidden
if (-not $ngrokProc) { exit 1 }

try {
    $publicUrl = $null
    for ($t = 0; $t -lt 40; $t++) {
        Start-Sleep -Milliseconds 500
        try {
            $api = Invoke-RestMethod -Uri "http://127.0.0.1:4040/api/tunnels" -TimeoutSec 2
            foreach ($tu in $api.tunnels) {
                if ($tu.public_url -like "https://*") {
                    $publicUrl = $tu.public_url.TrimEnd('/')
                    break
                }
            }
        } catch { }
        if ($publicUrl) { break }
    }
    if (-not $publicUrl) {
        Write-Host 'Could not read ngrok HTTPS URL from http://127.0.0.1:4040 - is ngrok authtoken configured?' -ForegroundColor Red
        exit 1
    }
    $env:TELEGRAM_WEBAPP_PUBLIC_URL = $publicUrl
    Write-Host "TELEGRAM_WEBAPP_PUBLIC_URL = $publicUrl" -ForegroundColor Green
    Write-Host "TELEGRAM_WEBAPP_BIND       = $($env:TELEGRAM_WEBAPP_BIND)" -ForegroundColor Green
    Write-Host "`nStarting polymarket-bot - Ctrl+C stops bot, then we stop ngrok...`n" -ForegroundColor Cyan

    $exe = Join-Path $ProjectRoot "target\release-fast\polymarket-bot.exe"
    if (-not (Test-Path -LiteralPath $exe)) {
        Write-Host "Building release-fast..." -ForegroundColor Yellow
        & cargo build --profile release-fast --bin polymarket-bot
        if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    }
    & $exe
} finally {
    if ($ngrokProc -and -not $ngrokProc.HasExited) {
        Stop-Process -Id $ngrokProc.Id -Force -ErrorAction SilentlyContinue
    }
    Write-Host "`nngrok stopped." -ForegroundColor DarkGray
}
