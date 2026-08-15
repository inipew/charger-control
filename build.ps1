# Build script for Windows to compile and package the Magisk module
# Requirements: cargo, rustup target add aarch64-linux-android, Android NDK

$ErrorActionPreference = "Stop"
$ProjectDir = $PSScriptRoot
$ModuleDir = "$ProjectDir\magisk-module"
$OutDir = "$ProjectDir\build"

# Optional: compile for android (requires cross or NDK setup)
# This will build for the host by default if aarch64 isn't setup.
# In a real scenario, use: cargo build --target aarch64-linux-android --release
Write-Host "Building Rust binary (release)..."
cargo build --release --target aarch64-linux-android

# Ensure output directory exists
if (-not (Test-Path $OutDir)) {
    New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
}

# Ensure system/bin directory exists in module
$BinDir = "$ModuleDir\system\bin"
if (-not (Test-Path $BinDir)) {
    New-Item -ItemType Directory -Force -Path $BinDir | Out-Null
}

# Copy binaries to module structure
$Binaries = @("charger-daemon", "charger-ctl")

foreach ($BinName in $Binaries) {
    $BinSource = "$ProjectDir\target\aarch64-linux-android\release\$BinName"
    if (-not (Test-Path $BinSource)) {
        $BinSource = "$ProjectDir\target\release\$BinName"
    }
    if (Test-Path $BinSource) {
        Copy-Item -Force $BinSource "$BinDir\$BinName"
        Write-Host "Copied $BinName to $BinDir"
    } else {
        Write-Error "Binary $BinName not found at $BinSource"
    }
}

# Zip the module
$ZipName = "charger-control-module.zip"
$ZipPath = "$OutDir\$ZipName"
if (Test-Path $ZipPath) {
    Remove-Item $ZipPath
}

Write-Host "Zipping Magisk Module..."
Compress-Archive -Path "$ModuleDir\*" -DestinationPath $ZipPath

Write-Host "Build complete! Flash $ZipPath in Magisk / KernelSU."
