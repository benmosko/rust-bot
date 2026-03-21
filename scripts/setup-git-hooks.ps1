# Run once per clone: use repo .githooks for pre-commit secret checks.
Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
Set-Location $repoRoot
git config core.hooksPath .githooks
Write-Host "Set core.hooksPath to .githooks (pre-commit blocks private keys / Telegram tokens in commits)."
