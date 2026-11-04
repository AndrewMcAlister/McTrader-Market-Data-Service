# McTrader_Market_Data_Service — run and operate
#
#   .\run.ps1              start the service on 127.0.0.1:8003
#   .\run.ps1 -Daily       run the daily job once (bulk pull + indicator rebuild)
#   .\run.ps1 -Test        run the test suite (spends no vendor units)
#   .\run.ps1 -Schema      apply sql/001_schema.sql and report row counts
#   .\run.ps1 -Seed        seed history from BarHourly, then rebuild indicators
#   .\run.ps1 -RawClose    fill RawClose from the 5-minute tape (no vendor units)
#
# The release binaries under target\release are used; build them once with
# `cargo build --release`. The service needs no Python environment.

param(
    [switch]$Daily,
    [switch]$Test,
    [switch]$Schema,
    [switch]$Seed,
    [int]$SeedDays = 90,
    [switch]$RawClose,
    [int]$RawCloseDays = 800,
    [string]$Since
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $MyInvocation.MyCommand.Path
$release = Join-Path $root 'target\release'
$service = Join-Path $release 'mmds-service.exe'
$daily = Join-Path $release 'mmds-daily.exe'
$tool = Join-Path $release 'mmds-tool.exe'

function Require-Binary([string]$Path) {
    if (-not (Test-Path $Path)) {
        Write-Host "No binary at $Path" -ForegroundColor Red
        Write-Host "Build the binaries once with:" -ForegroundColor Yellow
        Write-Host "  cargo build --release"
        exit 1
    }
}

function Find-Cargo {
    $command = Get-Command cargo -ErrorAction SilentlyContinue
    if ($command) { return $command.Source }
    $fallback = Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe'
    if (Test-Path $fallback) { return $fallback }
    Write-Host 'cargo was not found on PATH or in ~\.cargo\bin.' -ForegroundColor Red
    Write-Host 'Install it from https://rustup.rs and reopen the shell.' -ForegroundColor Yellow
    exit 1
}

Push-Location $root
try {
    if ($Test) {
        & (Find-Cargo) test --all-targets
        exit $LASTEXITCODE
    }
    if ($Schema) {
        Require-Binary $tool
        & $tool apply-schema
        exit $LASTEXITCODE
    }
    if ($Seed) {
        Require-Binary $daily
        & $daily --seed-from-barhourly --days $SeedDays --rebuild-indicators
        exit $LASTEXITCODE
    }
    if ($RawClose) {
        Require-Binary $daily
        & $daily --fill-raw-close --raw-close-days $RawCloseDays --skip-indicators
        exit $LASTEXITCODE
    }
    if ($Daily) {
        Require-Binary $daily
        if ($Since) {
            & $daily --since $Since
        } else {
            & $daily
        }
        exit $LASTEXITCODE
    }

    Require-Binary $service
    Write-Host 'McTrader_Market_Data_Service on http://127.0.0.1:8003  (docs at /docs, health at /healthz)' -ForegroundColor Cyan
    & $service
    exit $LASTEXITCODE
}
finally {
    Pop-Location
}
