$projectDir = $PSScriptRoot
Write-Host "Building release-fast..." -ForegroundColor Cyan
cargo build --profile release-fast --bin polymarket-bot
if ($LASTEXITCODE -ne 0) { Write-Host "Build failed." -ForegroundColor Red; exit 1 }

$profileDir = Split-Path $PROFILE
if (!(Test-Path $profileDir)) { New-Item -ItemType Directory -Path $profileDir -Force | Out-Null }
if (!(Test-Path $PROFILE)) { New-Item -ItemType File -Path $PROFILE -Force | Out-Null }

$polybotLine = '. "' + (Join-Path $projectDir "scripts\polybot.ps1") + '"'
$profileContent = Get-Content $PROFILE -Raw -ErrorAction SilentlyContinue
if ($profileContent -notmatch 'scripts\\polybot\.ps1') {
    Add-Content -Path $PROFILE -Value "`n# Polymarket Bot — polybot: ngrok + Telegram Mini App (see scripts\polybot.ps1)`n$polybotLine`n"
    Write-Host "Added profile line to dot-source scripts\polybot.ps1" -ForegroundColor Green
} else {
    Write-Host "Profile already dot-sources scripts\polybot.ps1 — nothing to do." -ForegroundColor Yellow
}

Write-Host "`nDone. Restart PowerShell (or: . `$PROFILE), then type polybot for live Telegram dashboard." -ForegroundColor Green
Write-Host "If an old 'function polybot { ... }' exists in your profile, remove it so only the dot-source line remains." -ForegroundColor DarkGray
