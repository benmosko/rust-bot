# One-time: add polybot function to current user's PowerShell profile.
$ErrorActionPreference = "Stop"
$line = '. "' + (Join-Path $PSScriptRoot "polybot.ps1") + '"'
$profilePath = $PROFILE
$cur = if (Test-Path -LiteralPath $profilePath) {
    Get-Content -LiteralPath $profilePath -Raw
} else {
    ""
}
if ($cur -match 'scripts\\polybot\.ps1') {
    Write-Host "Profile already references scripts\polybot.ps1 - nothing to do."
    exit 0
}
$dir = Split-Path -Parent $profilePath
if (-not (Test-Path -LiteralPath $dir)) {
    New-Item -ItemType Directory -Path $dir -Force | Out-Null
}
Add-Content -LiteralPath $profilePath -Value "`n# polymarket-bot: polybot = ngrok + Telegram Mini App dashboard`n$line`n"
Write-Host ("Appended to: " + $profilePath)
Write-Host "Restart PowerShell, or dot-source polybot.ps1 once in this session."
