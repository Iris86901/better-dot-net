<#
.SYNOPSIS
  Build (optional), package, upload, and deploy Dot-Proxy using `.env.local` / `.env`.
.EXAMPLE
  powershell -File deploy/push-deploy.ps1
  powershell -File deploy/push-deploy.ps1 -Build
#>
param(
  [switch]$Build,
  [switch]$SkipSrcSync
)

$ErrorActionPreference = "Stop"
$ProjectDir = Split-Path -Parent $PSScriptRoot
Set-Location $ProjectDir

# Load `.env.local` then `.env` without overwriting existing process env vars
foreach ($file in @(".env.local", ".env")) {
  $full = Join-Path $ProjectDir $file
  if (-not (Test-Path $full)) { continue }
  foreach ($rawLine in Get-Content $full) {
    $line = $rawLine.Trim()
    if (-not $line -or $line.StartsWith("#")) { continue }
    $idx = $line.IndexOf("=")
    if ($idx -le 0) { continue }
    $k = $line.Substring(0, $idx).Trim()
    $v = $line.Substring($idx + 1).Trim().Trim('"').Trim("'")
    $cur = [Environment]::GetEnvironmentVariable($k, "Process")
    if ([string]::IsNullOrEmpty($cur)) {
      [Environment]::SetEnvironmentVariable($k, $v, "Process")
    }
  }
}

$VpsHost = $env:VPS_SSH_HOST
if (-not $VpsHost) {
  throw "VPS_SSH_HOST is not set. Copy .env.example to .env.local and configure VPS_SSH_HOST."
}

if ($Build) {
  $wslPath = ($ProjectDir -replace '\\', '/') -replace '^([A-Za-z]):', { '/mnt/' + $_.Groups[1].Value.ToLower() }
  wsl -e bash -lc "bash '$wslPath/deploy/build-wsl.sh'"
  if ($LASTEXITCODE -ne 0) { throw "WSL build failed (exit $LASTEXITCODE)" }
}

if (-not (Test-Path "bin/dot-proxy")) {
  throw "bin/dot-proxy not found. Run with -Build first."
}

$bundle = Join-Path $env:TEMP "dot-proxy-bundle.tar.gz"
$srcTar = Join-Path $env:TEMP "dot-proxy-src.tar.gz"
Remove-Item -Force -ErrorAction SilentlyContinue $bundle, $srcTar

tar -czf $bundle bin web deploy
if ($LASTEXITCODE -ne 0) { throw "Failed to pack runtime bundle" }

$syncSrc = (-not $SkipSrcSync) -and (-not [string]::IsNullOrEmpty($env:SYNC_SRC_DIR))
if ($syncSrc) {
  tar --exclude="./bin" --exclude="./target" --exclude="./node_modules" --exclude="./.env" --exclude="./.env.local" --exclude="./.env.*.local" --exclude="./*.local.md" --exclude="./*.tar.gz" -czf $srcTar .
  if ($LASTEXITCODE -ne 0) { throw "Failed to pack source archive" }
}

$sshOpts = @(
  "-o", "IPQoS=none",
  "-o", "KexAlgorithms=curve25519-sha256",
  "-o", "StrictHostKeyChecking=no",
  "-o", "BatchMode=yes",
  "-o", "ConnectTimeout=12"
)

function Invoke-WithRetry([scriptblock]$Action, [string]$Label) {
  for ($i = 1; $i -le 3; $i++) {
    & $Action
    if ($LASTEXITCODE -eq 0) { return }
    if ($i -eq 3) { throw "$Label failed after 3 attempts (exit $LASTEXITCODE)" }
    Start-Sleep -Seconds 2
  }
}

Invoke-WithRetry { scp @sshOpts $bundle "${VpsHost}:/tmp/dot-proxy-bundle.tar.gz" } "scp bundle"
if ($syncSrc) {
  Invoke-WithRetry { scp @sshOpts $srcTar "${VpsHost}:/tmp/dot-proxy-src.tar.gz" } "scp src"
}
Invoke-WithRetry { scp @sshOpts "deploy/deploy-rust.sh" "${VpsHost}:/tmp/deploy-rust.sh" } "scp deploy script"

$remoteEnv = @()
if ($env:DOT_PROXY_DOMAIN) { $remoteEnv += "DOT_PROXY_DOMAIN='$($env:DOT_PROXY_DOMAIN)'" }
if ($syncSrc -and $env:SYNC_SRC_DIR) { $remoteEnv += "SYNC_SRC_DIR='$($env:SYNC_SRC_DIR)'" }
if ($syncSrc -and $env:SYNC_SRC_OWNER) { $remoteEnv += "SYNC_SRC_OWNER='$($env:SYNC_SRC_OWNER)'" }
if ($env:EXTRA_CHECK_SERVICES) { $remoteEnv += "EXTRA_CHECK_SERVICES='$($env:EXTRA_CHECK_SERVICES)'" }
$envPrefix = $remoteEnv -join " "

Invoke-WithRetry {
  ssh @sshOpts $VpsHost "sed -i 's/\r$//' /tmp/deploy-rust.sh && sudo $envPrefix bash /tmp/deploy-rust.sh && rm -f /tmp/deploy-rust.sh"
} "remote deploy"

Remove-Item -Force -ErrorAction SilentlyContinue $bundle, $srcTar
Write-Output "DEPLOY_OK ($VpsHost)"
