$ErrorActionPreference = "Stop"

$WorkspaceRoot = (Get-Item $PSScriptRoot).Parent.FullName
$ModuleDir = Join-Path $WorkspaceRoot "magisk-module"
$TargetDir = Join-Path $WorkspaceRoot "target\aarch64-linux-android\release"

Write-Host ">>> Memperbaiki CRLF -> LF (Unix style) untuk script .sh..." -ForegroundColor Cyan
Get-ChildItem -Path $ModuleDir -Filter "*.sh" -Recurse | ForEach-Object {
    $text = [IO.File]::ReadAllText($_.FullName).Replace("`r`n", "`n")
    $utf8NoBom = New-Object System.Text.UTF8Encoding $false
    [IO.File]::WriteAllText($_.FullName, $text, $utf8NoBom)
}

Write-Host ">>> Mengecek koneksi ADB..." -ForegroundColor Cyan
adb wait-for-device
adb root 2>$null
adb wait-for-device

$RemoteModDir = "/data/adb/modules/charger-control-rs"

Write-Host ">>> Membuat direktori modul di device..." -ForegroundColor Cyan
adb shell "mkdir -p $RemoteModDir/system/bin"

Write-Host ">>> Mengunggah (Push) Binary Rust..." -ForegroundColor Cyan
adb push "$TargetDir\charger-daemon" "$RemoteModDir/system/bin/charger-daemon"
adb push "$TargetDir\charger-ctl" "$RemoteModDir/system/bin/charger-ctl"
adb shell "chmod 755 $RemoteModDir/system/bin/*"

Write-Host ">>> Mengunggah (Push) File Modul Magisk..." -ForegroundColor Cyan
adb push "$ModuleDir\module.prop" "$RemoteModDir/module.prop"
Get-ChildItem -Path $ModuleDir -Filter "*.sh" | ForEach-Object {
    adb push $_.FullName "$RemoteModDir/$($_.Name)"
}
adb shell "chmod 755 $RemoteModDir/*.sh"

Write-Host ">>> Merestart Background Daemon..." -ForegroundColor Cyan
# Hentikan daemon yang lama (jika ada) dan jalankan yang baru
adb shell "killall charger-daemon 2>/dev/null; nohup sh $RemoteModDir/ccrs_service.sh >/dev/null 2>&1 &"

Write-Host ">>> Deploy Manual Selesai! Modul sudah aktif." -ForegroundColor Green
