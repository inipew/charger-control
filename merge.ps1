$sourceDir = "D:\any\charger-control-rs\crates\charger-daemon\src\monitor"
$outputFile = Join-Path $sourceDir "merged_monitor.txt"

# Reset file output jika sudah ada
if (Test-Path $outputFile) { Remove-Item $outputFile }

# Ambil semua file .rs dan gabungkan
Get-ChildItem -Path $sourceDir -Filter *.rs | Sort-Object Name | ForEach-Object {
    "// $($_.Name)" | Out-File -FilePath $outputFile -Append -Encoding utf8
    Get-Content $_.FullName -Raw | Out-File -FilePath $outputFile -Append -Encoding utf8
    "`n------------------" | Out-File -FilePath $outputFile -Append -Encoding utf8
}

Write-Host "Berhasil menggabungkan file ke: $outputFile"