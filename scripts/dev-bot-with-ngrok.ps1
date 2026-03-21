# Start ngrok (HTTPS tunnel) + polymarket-bot with TELEGRAM_WEBAPP_PUBLIC_URL set automatically.
# One-time: install ngrok (https://ngrok.com/download) and run: ngrok config add-authtoken YOUR_TOKEN
#
# Usage (from repo root):
#   .\scripts\dev-bot-with-ngrok.ps1
# Or: polybot (loads polybot-env.ps1, same checks)
#
# Requires: ngrok in PATH, .env with TELEGRAM_BOT_TOKEN + TELEGRAM_CHAT_ID + PK
$ErrorActionPreference = "Stop"
$ProjectRoot = Split-Path -Parent $PSScriptRoot
Set-Location $ProjectRoot

. "$PSScriptRoot\polybot-env.ps1"
if (-not (Test-PolybotPrerequisites -ProjectRoot $ProjectRoot)) {
    exit 1
}

# Standalone run: Telegram long-poll + Mini App HTTP (not supervisor child).
$env:DISABLE_TELEGRAM_POLLING = "false"
Remove-Item Env:\POLYBOT_SUPERVISOR_CHILD -ErrorAction SilentlyContinue

New-Item -ItemType Directory -Force -Path (Join-Path $ProjectRoot "logs") | Out-Null

$port = 8787
if ($env:TELEGRAM_WEBAPP_BIND) {
    if ($env:TELEGRAM_WEBAPP_BIND -match ':(\d+)\s*$') {
        $port = [int]$Matches[1]
    }
}
$env:TELEGRAM_WEBAPP_BIND = "127.0.0.1:$port"

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
        Write-Host 'Could not read ngrok HTTPS URL from http://127.0.0.1:4040' -ForegroundColor Red
        Write-Host 'Try: ngrok update   (accounts often require agent 3.20+)' -ForegroundColor Yellow
        Write-Host 'Then: ngrok config add-authtoken YOUR_TOKEN if auth still fails' -ForegroundColor Yellow
        exit 1
    }
    $env:TELEGRAM_WEBAPP_PUBLIC_URL = $publicUrl
    Write-Host "TELEGRAM_WEBAPP_PUBLIC_URL = $publicUrl" -ForegroundColor Green
    Write-Host "TELEGRAM_WEBAPP_BIND       = $($env:TELEGRAM_WEBAPP_BIND)" -ForegroundColor Green
    if ($publicUrl -match 'ngrok') {
        Write-Host ''
        Write-Host 'Free ngrok: one-time Visit Site in Telegram Mini App; tap once. Paid ngrok or Cloudflare Tunnel skips that.' -ForegroundColor Yellow
    }
    Write-Host ''
    Write-Host 'Starting polymarket-bot (Ctrl+C stops bot, then ngrok stops)...' -ForegroundColor Cyan

    $exe = Join-Path $ProjectRoot "target\release-fast\polymarket-bot.exe"
    if (-not (Test-Path -LiteralPath $exe)) {
        Write-Host "Building release-fast..." -ForegroundColor Yellow
        & cargo build --profile release-fast --bin polymarket-bot
        if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    }
    # Rust dotenv does not overwrite existing process env -- drop file-backed keys so .env + polymarket_keys.env win.
    foreach ($k in @('PK','POLYMARKET_PRIVATE_KEY','FUNDER','FUNDER_ADDRESS','TELEGRAM_BOT_TOKEN','TELEGRAM_CHAT_ID')) {
        Remove-Item "Env:$k" -ErrorAction SilentlyContinue
    }
    & $exe
} finally {
    if ($ngrokProc -and -not $ngrokProc.HasExited) {
        Stop-Process -Id $ngrokProc.Id -Force -ErrorAction SilentlyContinue
    }
    Write-Host ''
    Write-Host 'ngrok stopped.' -ForegroundColor DarkGray
}
