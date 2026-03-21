# List processes that might be using the same Telegram bot token (helps debug HTTP 409 on getUpdates).
# Run from repo: .\scripts\scan-telegram-processes.ps1

Write-Host "=== By process name (supervisor / polymarket-bot) ===" -ForegroundColor Cyan
Get-Process -Name supervisor, polymarket-bot -ErrorAction SilentlyContinue |
    Format-Table Id, ProcessName, Path -AutoSize

Write-Host "`n=== Command lines mentioning polymarket, supervisor, or telegram (may be slow) ===" -ForegroundColor Cyan
Get-CimInstance Win32_Process |
    Where-Object {
        if ($_.Name -ieq 'Telegram.exe') { return $false }  # Telegram Desktop client — not Bot API
        $cl = $_.CommandLine
        if (-not $cl) { return $false }
        if ($cl -match 'scan-telegram-processes') { return $false }
        if ($cl -match 'WindowsApps.*Telegram|TelegramMessengerLLP') { return $false }
        return ($cl -match 'polymarket|supervisor' -or ($cl -match 'telegram' -and $cl -match 'bot|api\.telegram|getUpdates'))
    } |
    Select-Object ProcessId, Name, CommandLine |
    Format-List

Write-Host "Telegram Desktop (Telegram.exe) is ignored — it is not your bot and does not call getUpdates." -ForegroundColor DarkGray
Write-Host "If nothing else shows but you still get 409, the poller is elsewhere (EC2, etc.) or use @BotFather /revoke for a new token." -ForegroundColor Yellow
