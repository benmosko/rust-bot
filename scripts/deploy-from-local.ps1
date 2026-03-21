# From Windows: one-shot deploy over SSH (git pull, release build, systemd restart).
# Prereq: repo at ~/polymarket-bot, rustup, polymarket-bot.service enabled.
#
# Usage:
#   .\scripts\deploy-from-local.ps1 -HostName ec2-...compute-1.amazonaws.com -KeyPath C:\keys\my.pem
param(
    [Parameter(Mandatory = $true)]
    [string] $HostName,
    [Parameter(Mandatory = $true)]
    [string] $KeyPath,
    [string] $UserName = "ubuntu"
)

$cmd = "cd ~/polymarket-bot && git pull --ff-only && cargo build --release --locked --bin polymarket-bot && sudo systemctl restart polymarket-bot && sudo systemctl is-active polymarket-bot"
ssh -i $KeyPath -o StrictHostKeyChecking=accept-new "${UserName}@${HostName}" "bash -lc '$cmd'"
