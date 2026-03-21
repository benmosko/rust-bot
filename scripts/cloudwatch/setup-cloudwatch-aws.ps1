# Run from your PC (AWS CLI logged in). Creates IAM role + instance profile, log group + retention,
# attaches profile to the bot EC2 instance. On-server alarm script needs this role + inline alarm policy.
param(
    [Parameter(Mandatory = $true)]
    [string] $InstanceId,
    [string] $Region = "us-east-1",
    [string] $LogGroupName = "/ec2/polymarket-bot/app",
    [int] $RetentionDays = 30,
    [string] $RoleName = "PolymarketBotEC2CloudWatch",
    [string] $InstanceProfileName = "PolymarketBotEC2Profile"
)

$ErrorActionPreference = "Stop"
$trust = (Resolve-Path (Join-Path $PSScriptRoot "trust-policy-ec2.json")).Path
$alarmPolicy = (Resolve-Path (Join-Path $PSScriptRoot "alarm-policy.json")).Path

function Run-Aws {
    param([string[]] $CliArgs)
    & aws @CliArgs
    return $LASTEXITCODE
}

if ((Run-Aws @("iam", "create-role", "--role-name", $RoleName, "--assume-role-policy-document", "file://$trust")) -ne 0) {
    $exists = aws iam get-role --role-name $RoleName 2>$null
    if (-not $exists) { throw "iam create-role failed" }
}

if ((Run-Aws @("iam", "attach-role-policy", "--role-name", $RoleName, "--policy-arn", "arn:aws:iam::aws:policy/CloudWatchAgentServerPolicy")) -ne 0) { throw "attach CloudWatchAgentServerPolicy failed" }

if ((Run-Aws @("iam", "put-role-policy", "--role-name", $RoleName, "--policy-name", "PolymarketBotCloudWatchAlarms", "--policy-document", "file://$alarmPolicy")) -ne 0) { throw "put-role-policy failed" }

if ((Run-Aws @("iam", "create-instance-profile", "--instance-profile-name", $InstanceProfileName)) -ne 0) {
    aws iam get-instance-profile --instance-profile-name $InstanceProfileName 2>$null | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "create-instance-profile failed" }
}

if ((Run-Aws @("iam", "add-role-to-instance-profile", "--instance-profile-name", $InstanceProfileName, "--role-name", $RoleName)) -ne 0) {
    Write-Host "(add-role-to-instance-profile may already be satisfied)"
}

if ((Run-Aws @("logs", "create-log-group", "--log-group-name", $LogGroupName, "--region", $Region)) -ne 0) {
    Write-Host "(log group may already exist)"
}

if ((Run-Aws @("logs", "put-retention-policy", "--log-group-name", $LogGroupName, "--retention-in-days", $RetentionDays.ToString(), "--region", $Region)) -ne 0) { throw "put-retention-policy failed" }

$assocJson = aws ec2 describe-iam-instance-profile-associations --region $Region --filters "Name=instance-id,Values=$InstanceId" --output json
$assoc = $assocJson | ConvertFrom-Json
if ($assoc.IamInstanceProfileAssociations.Count -eq 0) {
    $ec = Run-Aws @("ec2", "associate-iam-instance-profile", "--region", $Region, "--instance-id", $InstanceId, "--iam-instance-profile", "Name=$InstanceProfileName")
    if ($ec -ne 0) {
        $again = aws ec2 describe-iam-instance-profile-associations --region $Region --filters "Name=instance-id,Values=$InstanceId" --output json | ConvertFrom-Json
        if ($again.IamInstanceProfileAssociations.Count -eq 0) { throw "associate-iam-instance-profile failed" }
        Write-Host "Association may have raced; profile is now attached."
    } else {
        Write-Host "Attached instance profile $InstanceProfileName to $InstanceId."
    }
} else {
    Write-Host "Instance already has an IAM instance profile; not replacing. Verify it includes CloudWatch + alarm permissions."
}

Write-Host "Done. On server: git pull, then: sudo bash ~/polymarket-bot/scripts/cloudwatch/install-cloudwatch-agent-ubuntu.sh"
