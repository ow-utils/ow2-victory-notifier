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
$plainSecret = $null

try {
    $plainSecret = [System.Net.NetworkCredential]::new("", $secret).Password

    if ([string]::IsNullOrEmpty($plainSecret)) {
        throw "Client Secret is required."
    }

    $psi = [System.Diagnostics.ProcessStartInfo]::new()
    $psi.FileName = $ExePath
    $psi.WorkingDirectory = Split-Path -Parent $ExePath
    $psi.UseShellExecute = $false
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
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

    $script:AuthNightbotNoBrowser = [bool]$NoBrowser
    $script:AuthNightbotOpenedAuthUrl = $false
    $process = [System.Diagnostics.Process]::new()
    $process.StartInfo = $psi
    $process.add_OutputDataReceived({
        param($sender, $eventArgs)

        if ($null -eq $eventArgs.Data) {
            return
        }

        [Console]::Out.WriteLine($eventArgs.Data)

        if (-not $script:AuthNightbotNoBrowser -and
            -not $script:AuthNightbotOpenedAuthUrl -and
            $eventArgs.Data -match '^https://api\.nightbot\.tv/oauth2/authorize\?') {
            $script:AuthNightbotOpenedAuthUrl = $true
            try {
                Start-Process $eventArgs.Data
            }
            catch {
                [Console]::Error.WriteLine("Failed to open browser automatically: $($_.Exception.Message)")
            }
        }
    })
    $process.add_ErrorDataReceived({
        param($sender, $eventArgs)

        if ($null -ne $eventArgs.Data) {
            [Console]::Error.WriteLine($eventArgs.Data)
        }
    })

    if (-not $process.Start()) {
        throw "Failed to start '$ExePath'."
    }

    $process.BeginOutputReadLine()
    $process.BeginErrorReadLine()
    $process.WaitForExit()

    if ($process.ExitCode -ne 0) {
        exit $process.ExitCode
    }
}
finally {
    Remove-Variable -Name AuthNightbotNoBrowser -Scope Script -ErrorAction SilentlyContinue
    Remove-Variable -Name AuthNightbotOpenedAuthUrl -Scope Script -ErrorAction SilentlyContinue
}
