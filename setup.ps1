$projectDir = $PSScriptRoot
Write-Host "Building release-fast..." -ForegroundColor Cyan
cargo build --profile release-fast --bin polymarket-bot
if ($LASTEXITCODE -ne 0) { Write-Host "Build failed." -ForegroundColor Red; exit 1 }

$profileDir = Split-Path $PROFILE
if (!(Test-Path $profileDir)) { New-Item -ItemType Directory -Path $profileDir -Force | Out-Null }
if (!(Test-Path $PROFILE)) { New-Item -ItemType File -Path $PROFILE -Force | Out-Null }

$polybotBody = "Set-Location '$projectDir'; & '$projectDir\target\release-fast\polymarket-bot.exe'"
$funcBlock = "`n# Polymarket Bot`nfunction polybot { $polybotBody }"

$profileContent = Get-Content $PROFILE -Raw -ErrorAction SilentlyContinue
if ($profileContent -notmatch 'function polybot') {
    Add-Content -Path $PROFILE -Value $funcBlock
    Write-Host "Added 'polybot' command to PowerShell profile." -ForegroundColor Green
} else {
    # One-line polybot: refresh path to release-fast if they ran an older setup.ps1
    $updated = $profileContent -replace 'function polybot\s*\{[^}]*\}', "function polybot { $polybotBody }"
    if ($updated -ne $profileContent) {
        Set-Content -Path $PROFILE -Value $updated
        Write-Host "Updated existing 'polybot' to use release-fast binary." -ForegroundColor Green
    } else {
        Write-Host "'polybot' already in profile (no change needed)." -ForegroundColor Yellow
    }
}

Write-Host "`nDone. Restart PowerShell (or: . `$PROFILE), then type 'polybot' to launch." -ForegroundColor Green
Write-Host "Binary: target\release-fast\polymarket-bot.exe" -ForegroundColor DarkGray
