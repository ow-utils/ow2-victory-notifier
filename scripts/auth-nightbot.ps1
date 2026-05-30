[CmdletBinding()]
param(
    [string]$Account = "default",
    [string]$ClientId,
    [string]$ClientSecretEnv = "NIGHTBOT_CLIENT_SECRET",
    [string]$ExePath,
    [string]$Config,
    [switch]$NoBrowser
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
$plainSecret = [System.Net.NetworkCredential]::new("", $secret).Password

if ([string]::IsNullOrEmpty($plainSecret)) {
    throw "Client Secret is required."
}

$psi = [System.Diagnostics.ProcessStartInfo]::new()
$psi.FileName = $ExePath
$psi.WorkingDirectory = Split-Path -Parent $ExePath
$psi.UseShellExecute = $false
$psi.RedirectStandardOutput = $true
$psi.StandardOutputEncoding = [System.Text.Encoding]::UTF8
$psi.Environment[$ClientSecretEnv] = $plainSecret

foreach ($arg in @(
    "auth",
    "nightbot",
    "--account",
    $Account,
    "--config",
    $Config,
    "--client-id",
    $ClientId,
    "--client-secret-env",
    $ClientSecretEnv
)) {
    $psi.ArgumentList.Add($arg)
}

$openedAuthUrl = $false
$process = [System.Diagnostics.Process]::new()
$process.StartInfo = $psi

if (-not $process.Start()) {
    throw "Failed to start '$ExePath'."
}

while (-not $process.StandardOutput.EndOfStream) {
    $line = $process.StandardOutput.ReadLine()
    [Console]::Out.WriteLine($line)

    if (-not $NoBrowser -and
        -not $openedAuthUrl -and
        $line -match '^https://api\.nightbot\.tv/oauth2/authorize\?') {
        $openedAuthUrl = $true
        try {
            Start-Process $line
        }
        catch {
            [Console]::Error.WriteLine("Failed to open browser automatically: $($_.Exception.Message)")
        }
    }
}

$process.WaitForExit()

if ($process.ExitCode -ne 0) {
    exit $process.ExitCode
}
