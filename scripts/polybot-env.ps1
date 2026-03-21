# Shared by polybot.ps1 and dev-bot-with-ngrok.ps1.
#
# IMPORTANT: Do NOT push .env into $env: before starting polymarket-bot.exe.
# Rust's dotenv does not overwrite existing vars -- wrong PowerShell-parsed values would win.
# We only read files here to validate; the binary loads .env + polymarket_keys.env from CWD.

function Merge-PolybotEnvFromFiles {
    param([string]$ProjectRoot)
    $map = @{}
    foreach ($rel in @(".env", "polymarket_keys.env")) {
        $path = Join-Path $ProjectRoot $rel
        if (-not (Test-Path -LiteralPath $path)) { continue }
        Get-Content -LiteralPath $path | ForEach-Object {
            $line = $_.Trim()
            if ($line -match '^\s*#' -or $line -eq '') { return }
            $i = $line.IndexOf('=')
            if ($i -lt 1) { return }
            $k = $line.Substring(0, $i).Trim()
            $v = $line.Substring($i + 1).Trim()
            $map[$k] = $v
        }
    }
    return $map
}

function Test-PolybotPrerequisites {
    param([string]$ProjectRoot)
    $envPath = Join-Path $ProjectRoot ".env"
    if (-not (Test-Path -LiteralPath $envPath)) {
        Write-Host 'polybot: Missing .env -- copy .env.example to .env and set TELEGRAM_BOT_TOKEN, TELEGRAM_CHAT_ID, PK (or POLYMARKET_PRIVATE_KEY).' -ForegroundColor Red
        return $false
    }
    $m = Merge-PolybotEnvFromFiles -ProjectRoot $ProjectRoot

    $pk = $m['PK']
    if (-not $pk) { $pk = $m['POLYMARKET_PRIVATE_KEY'] }
    if (-not $pk -or $pk.Trim() -eq '') {
        Write-Host 'polybot: Set PK or POLYMARKET_PRIVATE_KEY in .env or polymarket_keys.env.' -ForegroundColor Red
        return $false
    }
    if ($pk -match 'your_polygon|0x\.\.\.') {
        Write-Host 'polybot: Replace the PK / POLYMARKET_PRIVATE_KEY placeholder with your real key.' -ForegroundColor Red
        return $false
    }

    $tok = $m['TELEGRAM_BOT_TOKEN']
    if (-not $tok -or $tok.Trim() -eq '') {
        Write-Host 'polybot: Set TELEGRAM_BOT_TOKEN in .env (required for Telegram commands and Mini App).' -ForegroundColor Red
        return $false
    }
    $chat = $m['TELEGRAM_CHAT_ID']
    if (-not $chat -or $chat.Trim() -eq '') {
        Write-Host 'polybot: Set TELEGRAM_CHAT_ID in .env.' -ForegroundColor Red
        return $false
    }
    if (-not (Get-Command ngrok -ErrorAction SilentlyContinue)) {
        Write-Host 'polybot: ngrok not in PATH. Install: winget install ngrok.ngrok' -ForegroundColor Red
        Write-Host '       Then: ngrok config add-authtoken YOUR_TOKEN  (see dashboard.ngrok.com)' -ForegroundColor DarkGray
        return $false
    }
    return $true
}
