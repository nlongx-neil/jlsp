# ==============================================================================
# jlsp Windows Packaging Script (PowerShell)
# ==============================================================================
# Usage:
#   .\scripts\package.ps1 [-Target <target>] [-OutDir <dir>]
#
# Examples:
#   .\scripts\package.ps1
#   .\scripts\package.ps1 -Target x86_64-pc-windows-msvc
#   .\scripts\package.ps1 -Target x86_64-pc-windows-gnu
# ==============================================================================

param(
    [string]$Target = "x86_64-pc-windows-msvc",
    [string]$OutDir = "dist"
)

$ErrorActionPreference = "Stop"

$ProjectRoot = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
Set-Location $ProjectRoot

# Extract version from Cargo.toml
$CargoToml = Get-Content "Cargo.toml" -Raw
if ($CargoToml -match 'version\s*=\s*"([^"]+)"') {
    $Version = $matches[1]
} else {
    $Version = "0.1.0"
}

Write-Host "=======================================================================" -ForegroundColor Cyan
Write-Host "📦 Packaging jlsp v$Version for $Target (Windows)" -ForegroundColor Cyan
Write-Host "======================================================================="

# Ensure target installed
rustup target add $Target

# Compile in release mode
Write-Host "🚀 Compiling release binary with cargo..." -ForegroundColor Yellow
cargo build --release --target $Target

# Locate build artifact
$CargoMetadata = cargo metadata --format-version 1 --no-deps | ConvertFrom-Json
$TargetBaseDir = $CargoMetadata.target_directory
if (-not $TargetBaseDir) {
    $TargetBaseDir = Join-Path $ProjectRoot "target"
}

$BinSrc = Join-Path (Join-Path (Join-Path $TargetBaseDir $Target) "release") "jlsp.exe"
if (-not (Test-Path $BinSrc)) {
    $BinSrc = Join-Path (Join-Path (Join-Path $ProjectRoot "target") $Target) "release\jlsp.exe"
}

if (-not (Test-Path $BinSrc)) {
    Write-Error "❌ Could not find binary at $BinSrc"
    exit 1
}

$OutputDirFull = Join-Path $ProjectRoot $OutDir
if (-not (Test-Path $OutputDirFull)) {
    New-Item -ItemType Directory -Path $OutputDirFull | Out-Null
}

$StageDir = Join-Path $OutputDirFull "stage_$Target"
if (Test-Path $StageDir) {
    Remove-Item -Recurse -Force $StageDir
}
New-Item -ItemType Directory -Path $StageDir | Out-Null

Copy-Item $BinSrc -Destination $StageDir
if (Test-Path "README.md") { Copy-Item "README.md" -Destination $StageDir }
if (Test-Path "LICENSE") { Copy-Item "LICENSE" -Destination $StageDir }

$ArchiveName = "jlsp-v$Version-$Target.zip"
$ArchivePath = Join-Path $OutputDirFull $ArchiveName

Write-Host "📦 Compressing into $ArchiveName..." -ForegroundColor Yellow
if (Test-Path $ArchivePath) { Remove-Item -Force $ArchivePath }
Compress-Archive -Path "$StageDir\*" -DestinationPath $ArchivePath -CompressionLevel Optimal

Remove-Item -Recurse -Force $StageDir

# SHA-256 Checksum
Write-Host "🔑 Generating SHA256..." -ForegroundColor Yellow
$Hash = (Get-FileHash -Algorithm SHA256 $ArchivePath).Hash.ToLower()
$HashFile = "$ArchivePath.sha256"
"$Hash  $ArchiveName" | Set-Content -Path $HashFile

Write-Host "=======================================================================" -ForegroundColor Green
Write-Host "🎉 Package created successfully: $ArchivePath" -ForegroundColor Green
Write-Host "   SHA256: $Hash" -ForegroundColor Green
Write-Host "======================================================================="
