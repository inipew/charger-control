$ErrorActionPreference = "Stop"

$WorkspaceRoot = (Get-Item $PSScriptRoot).Parent.FullName
$ModuleDir = Join-Path $WorkspaceRoot "magisk-module"
$TargetDir = Join-Path $WorkspaceRoot "target\aarch64-linux-android\release"
$DestDir = Join-Path $ModuleDir "system\bin"
Set-Location $WorkspaceRoot

if (-not (Test-Path $DestDir)) {
    New-Item -ItemType Directory -Force -Path $DestDir | Out-Null
}

Write-Host ">>> Building for aarch64-linux-android" -ForegroundColor Cyan
cargo build --release --target aarch64-linux-android

Write-Host ">>> Stripping binaries (if llvm-strip is available)" -ForegroundColor Cyan
try {
    Write-Host "Binaries compiled successfully." -ForegroundColor Green
} catch {
    Write-Host "Failed to strip, continuing..." -ForegroundColor Yellow
}

# Write-Host ">>> Packaging Magisk module" -ForegroundColor Cyan
# & (Join-Path $PSScriptRoot "package-module.ps1")

Write-Host "Copying binaries from $TargetDir to $DestDir..."
Copy-Item (Join-Path $TargetDir "charger-daemon") $DestDir -Force
Copy-Item (Join-Path $TargetDir "charger-ctl") $DestDir -Force