[CmdletBinding()]
param(
    [string]$Account = "default",
    [string]$ClientId,
    [string]$ClientSecretEnv = "NIGHTBOT_CLIENT_SECRET",
    [string]$ExePath,
    [string]$Config
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$appRoot = $PSScriptRoot

if ((Split-Path -Leaf $PSScriptRoot) -eq "scripts") {
    $appRoot = Split-Path -Parent $appRoot
}

if ([string]::IsNullOrWhiteSpace($ExePath)) {
    $ExePath = Join-Path $appRoot "ow2-victory-notifier.exe"
}

if ([string]::IsNullOrWhiteSpace($Config)) {
    $Config = Join-Path $appRoot "config.toml"
}

if (-not (Test-Path -LiteralPath $ExePath -PathType Leaf)) {
    throw "Executable was not found at '$ExePath'. Place ow2-victory-notifier.exe next to config.toml, or pass -ExePath."
}

if (-not (Test-Path -LiteralPath $Config -PathType Leaf)) {
    throw "Config file was not found at '$Config'. Place config.toml next to ow2-victory-notifier.exe, or pass -Config."
}

if ([string]::IsNullOrWhiteSpace($ClientId)) {
    $ClientId = Read-Host "Nightbot Client ID"
}

if ([string]::IsNullOrWhiteSpace($ClientId)) {
    throw "Client ID is required."
}

$secret = Read-Host "Nightbot Client Secret" -AsSecureString
$plainSecret = $null

try {
    $plainSecret = [System.Net.NetworkCredential]::new("", $secret).Password

    if ([string]::IsNullOrEmpty($plainSecret)) {
        throw "Client Secret is required."
    }

    Set-Item -Path "Env:$ClientSecretEnv" -Value $plainSecret

    & $ExePath auth nightbot --account $Account `
        --config $Config `
        --client-id $ClientId `
        --client-secret-env $ClientSecretEnv

    if ($LASTEXITCODE -ne 0) {
        exit $LASTEXITCODE
    }
}
finally {
    Remove-Item -Path "Env:$ClientSecretEnv" -ErrorAction SilentlyContinue
}
