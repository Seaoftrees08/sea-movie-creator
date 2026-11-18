<#
.SYNOPSIS
    Runs all pre-PR code quality checks for sea-movie-creator.
.DESCRIPTION
    Executes formatting, Clippy, Rustdoc warning check, full workspace build,
    full workspace tests (including tests/quality_gates.rs), and cargo-deny if installed.
#>
[CmdletBinding()]
param(
    [switch]$RequireDeny,
    [switch]$SkipBuildAndTests
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
Push-Location $RepoRoot
$HasPreviousRustdocFlags = Test-Path Env:RUSTDOCFLAGS
$PreviousRustdocFlags = if ($HasPreviousRustdocFlags) { $env:RUSTDOCFLAGS } else { $null }

try {
    Write-Host "==> Working directory: $RepoRoot"

    Write-Host "[1/6] Checking formatting (cargo fmt --all --check)..."
    cargo fmt --all --check
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

    Write-Host "[2/6] Running Clippy (cargo clippy --workspace --all-targets --all-features -- -D warnings)..."
    cargo clippy --workspace --all-targets --all-features -- -D warnings
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

    Write-Host "[3/6] Checking documentation warnings (cargo doc --workspace --no-deps)..."
    $env:RUSTDOCFLAGS = "-D warnings"
    cargo doc --workspace --no-deps
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    if ($HasPreviousRustdocFlags) {
        $env:RUSTDOCFLAGS = $PreviousRustdocFlags
    } else {
        Remove-Item Env:RUSTDOCFLAGS -ErrorAction SilentlyContinue
    }

    if (-not $SkipBuildAndTests) {
        Write-Host "[4/6] Building all workspace targets (cargo build --workspace --all-targets)..."
        cargo build --workspace --all-targets
        if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

        Write-Host "[5/6] Running workspace test suite (cargo test --workspace --all-targets --all-features)..."
        cargo test --workspace --all-targets --all-features
        if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    } else {
        Write-Host "[4/6] Skipped build (-SkipBuildAndTests)."
        Write-Host "[5/6] Skipped tests (-SkipBuildAndTests)."
    }

    Write-Host "[6/6] Checking dependencies and licenses (cargo deny check)..."
    $CargoDenyCmd = Get-Command cargo-deny -ErrorAction SilentlyContinue
    if ($null -ne $CargoDenyCmd) {
        cargo deny check
        if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    } elseif ($RequireDeny) {
        Write-Error "cargo-deny is not installed, and -RequireDeny was specified. Install with: cargo install --locked cargo-deny"
        exit 1
    } else {
        Write-Warning "cargo-deny is not installed locally; skipping local dependency/license audit (enforced on CI). Install with: cargo install --locked cargo-deny"
    }

    Write-Host "All pre-PR quality gates passed."
}
finally {
    if ($HasPreviousRustdocFlags) {
        $env:RUSTDOCFLAGS = $PreviousRustdocFlags
    } else {
        Remove-Item Env:RUSTDOCFLAGS -ErrorAction SilentlyContinue
    }
    Pop-Location
}
