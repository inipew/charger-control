$ErrorActionPreference = "Stop"

$WorkspaceRoot = (Get-Item $PSScriptRoot).Parent.FullName
$ModuleDir = Join-Path $WorkspaceRoot "magisk-module"
$TargetDir = Join-Path $WorkspaceRoot "target\aarch64-linux-android\release"
$DestDir = Join-Path $ModuleDir "system\bin"

# Pastikan folder tujuan ada
if (-not (Test-Path $DestDir)) {
    New-Item -ItemType Directory -Force -Path $DestDir | Out-Null
}

Write-Host "Fixing CRLF to LF for shell scripts..."
Get-ChildItem -Path $ModuleDir -Filter "*.sh" -Recurse | ForEach-Object {
    $text = [IO.File]::ReadAllText($_.FullName).Replace("`r`n", "`n")
    $utf8NoBom = New-Object System.Text.UTF8Encoding $false
    [IO.File]::WriteAllText($_.FullName, $text, $utf8NoBom)
}

# Copy binaries
Write-Host "Copying binaries from $TargetDir to $DestDir..."
Copy-Item (Join-Path $TargetDir "charger-daemon") $DestDir -Force
Copy-Item (Join-Path $TargetDir "charger-ctl") $DestDir -Force

# Zip module
$DateStr = Get-Date -Format "yyyyMMdd-HHmmss"
$ZipName = Join-Path $WorkspaceRoot "charger-control-rs-$DateStr.zip"

Write-Host "Creating zip file: $ZipName"
if (Test-Path $ZipName) {
    Remove-Item $ZipName -Force
}

Compress-Archive -Path (Join-Path $ModuleDir "*") -DestinationPath $ZipName -Force
Write-Host ">>> Module zip ready! $ZipName" -ForegroundColor Green
