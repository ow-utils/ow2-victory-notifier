[CmdletBinding()]
param(
    [string]$Account = "default",
    [string]$ClientId,
    [string]$ClientSecretEnv = "NIGHTBOT_CLIENT_SECRET"
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

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

    cargo run -- auth nightbot --account $Account `
        --client-id $ClientId `
        --client-secret-env $ClientSecretEnv

    if ($LASTEXITCODE -ne 0) {
        exit $LASTEXITCODE
    }
}
finally {
    Remove-Item -Path "Env:$ClientSecretEnv" -ErrorAction SilentlyContinue
}
