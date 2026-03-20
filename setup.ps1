$projectDir = $PSScriptRoot
Write-Host "Building release..." -ForegroundColor Cyan
cargo build --release
if ($LASTEXITCODE -ne 0) { Write-Host "Build failed." -ForegroundColor Red; exit 1 }

$profileDir = Split-Path $PROFILE
if (!(Test-Path $profileDir)) { New-Item -ItemType Directory -Path $profileDir -Force | Out-Null }
if (!(Test-Path $PROFILE)) { New-Item -ItemType File -Path $PROFILE -Force | Out-Null }

$profileContent = Get-Content $PROFILE -Raw -ErrorAction SilentlyContinue
$funcBlock = "`n# Polymarket Bot`nfunction polybot { Set-Location '$projectDir'; .\target\release\polymarket-bot.exe }"

if ($profileContent -notmatch 'function polybot') {
    Add-Content -Path $PROFILE -Value $funcBlock
    Write-Host "Added 'polybot' command to PowerShell profile." -ForegroundColor Green
} else {
    Write-Host "'polybot' already in profile." -ForegroundColor Yellow
}

Write-Host "`nDone. Restart PowerShell, then type 'polybot' to launch." -ForegroundColor Green
