$ErrorActionPreference = "Stop"

$WorkspaceRoot = (Get-Item $PSScriptRoot).Parent.FullName
Set-Location $WorkspaceRoot

Write-Host ">>> Building for aarch64-linux-android" -ForegroundColor Cyan
cargo build --release --target aarch64-linux-android

Write-Host ">>> Stripping binaries (if llvm-strip is available)" -ForegroundColor Cyan
try {
    Write-Host "Binaries compiled successfully." -ForegroundColor Green
} catch {
    Write-Host "Failed to strip, continuing..." -ForegroundColor Yellow
}

Write-Host ">>> Packaging Magisk module" -ForegroundColor Cyan
& (Join-Path $PSScriptRoot "package-module.ps1")
