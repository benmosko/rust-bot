# Dot-source from PowerShell profile so `polybot` starts the Telegram supervisor:
#   . "C:\path\to\rust bot\scripts\polybot.ps1"
$script:PolybotRoot = Split-Path $PSScriptRoot -Parent

function polybot {
    $exe = Join-Path $script:PolybotRoot "target\release-fast\supervisor.exe"
    if (Test-Path -LiteralPath $exe) {
        & $exe @args
    } else {
        Push-Location $script:PolybotRoot
        try {
            & cargo run --profile release-fast --bin supervisor -- @args
        } finally {
            Pop-Location
        }
    }
}
