# Dot-source from PowerShell profile so `polybot` starts the full Telegram experience:
#   . "C:\path\to\rust bot\scripts\polybot.ps1"
#
# Does everything needed for a live dashboard in Telegram (Mini App over HTTPS):
#   1) Validates .env + ngrok (see scripts\polybot-env.ps1)
#   2) Stops local ngrok / supervisor / polymarket-bot (clean tunnel + free getUpdates for this token)
#   3) Starts ngrok -> sets TELEGRAM_WEBAPP_PUBLIC_URL + TELEGRAM_WEBAPP_BIND
#   4) Builds (if needed) and runs polymarket-bot (HTTP on 127.0.0.1:8787 + Telegram polling + Mini App)
#
# Not compatible with another machine polling the same bot (e.g. EC2 supervisor) — stop that first.
# For supervisor-only (no Mini App): run  target\release-fast\supervisor.exe  or  cargo run --profile release-fast --bin supervisor
$script:PolybotRoot = Split-Path $PSScriptRoot -Parent
. "$PSScriptRoot\polybot-env.ps1"

function polybot {
    Push-Location $script:PolybotRoot
    try {
        if (-not (Test-PolybotPrerequisites -ProjectRoot $script:PolybotRoot)) {
            return
        }
        Write-Host "polybot: stopping local ngrok / supervisor / polymarket-bot (clean tunnel + free Telegram getUpdates)..." -ForegroundColor Cyan
        & "$PSScriptRoot\stop-telegram-pollers.ps1"
        Start-Sleep -Seconds 3
        & "$PSScriptRoot\dev-bot-with-ngrok.ps1"
    } finally {
        Pop-Location
    }
}
