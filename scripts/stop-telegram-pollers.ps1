# Stops local Windows processes that may be holding Telegram getUpdates (fixes HTTP 409 Conflict)
# and ngrok so a new tunnel can bind cleanly.
# Does not stop remote copies (e.g. AWS EC2) — SSH there and stop the service or process.
$ErrorActionPreference = "SilentlyContinue"
foreach ($name in @("ngrok", "supervisor", "polymarket-bot")) {
    Get-Process -Name $name | ForEach-Object {
        Write-Host "Stopping PID $($_.Id) ($($_.ProcessName))"
        Stop-Process -Id $_.Id -Force
    }
}
Write-Host "Done. Wait ~5 seconds, then run polybot again. If 409 persists, stop the bot on other PCs or revoke the token in @BotFather."
