# Idempotent restart script for amp-proxy (Rust port) on Windows.
#
# What it does:
#   1. Kills any running amp-proxy.exe so the listening port is released.
#   2. (Default) Rebuilds in release mode via `cargo build --release`.
#      Skipped when -NoBuild is passed (boot-time path, see below).
#   3. Relaunches the freshly-built binary in the background, pointed at
#      .\config.local.yaml when present, else .\config.yaml.
#   4. Redirects stderr (where `tracing` writes by default) to .\run.log
#      and stdout to .\run.log.err, then tails the boot log.
#
# Flags:
#   -NoBuild   Skip `cargo build --release`. Used by the Startup-folder
#              shortcut so login is fast and doesn't require cargo to be
#              on PATH at logon. Falls back to the existing
#              .\target\release\amp-proxy.exe.
#
# Safe to run repeatedly: every step is idempotent and the script can be
# invoked from anywhere via `.\scripts\restart.ps1` thanks to Push-Location.

param(
    [switch]$NoBuild
)

# Stop any running amp-proxy process; ignore if none.
Stop-Process -Name amp-proxy -Force -ErrorAction SilentlyContinue

# Give the OS a moment to release the listening port.
Start-Sleep -Milliseconds 500

# Operate from the repository root (one level up from this script).
Push-Location (Join-Path $PSScriptRoot '..')
try {
    $binPath = '.\target\release\amp-proxy.exe'

    if (-not $NoBuild) {
        # Dev path: rebuild in release mode before launching.
        & cargo build --release
        if ($LASTEXITCODE -ne 0) {
            Write-Error "amp-proxy cargo build --release failed (exit code $LASTEXITCODE)"
            exit 1
        }
    }

    if (-not (Test-Path $binPath)) {
        if ($NoBuild) {
            Write-Error "Built binary not found at $binPath. Run .\scripts\restart.ps1 once (without -NoBuild) to compile, or run cargo build --release manually."
        } else {
            Write-Error "Built binary not found at $binPath"
        }
        exit 1
    }

    # Pick a config: prefer .\config.local.yaml, fall back to .\config.yaml.
    $configPath = $null
    if (Test-Path '.\config.local.yaml') {
        $configPath = '.\config.local.yaml'
    }
    elseif (Test-Path '.\config.yaml') {
        $configPath = '.\config.yaml'
    }
    else {
        Write-Error "No config file found (.\config.local.yaml or .\config.yaml)"
        exit 1
    }

    # Relaunch hidden. amp-proxy uses `tracing` which writes to stderr by
    # default, so we aim stderr at run.log (the file the README/NOTICE tell
    # the operator to tail). stdout goes to run.log.err as a safety net —
    # amp-proxy does not currently write anything to stdout.
    Start-Process -FilePath $binPath `
        -ArgumentList '--config', $configPath `
        -WindowStyle Hidden `
        -RedirectStandardOutput .\run.log.err `
        -RedirectStandardError  .\run.log

    # Let the process boot before tailing the log.
    Start-Sleep -Seconds 1

    $mode = if ($NoBuild) { 'no-build' } else { 'build+restart' }
    Write-Output "amp-proxy ($mode) launched with $configPath; see .\run.log"
    Get-Content .\run.log -Tail 10
}
finally {
    Pop-Location
}
