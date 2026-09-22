Set-Location 'd:\workspace\peerbanhelper-rs'
$root = 'd:\workspace\peerbanhelper-rs\target\dualrun'
$rustDir = "$root\rust"
$qbPort = '18080'
$record = "$root\bans-rust.txt"
Remove-Item -Recurse -Force $rustDir -ErrorAction SilentlyContinue
Remove-Item -Force $record -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $rustDir | Out-Null

# 与 Java 侧对齐的两处改造：关 GeoIP 自动更新、下载器指向 mock
$cfg = Get-Content 'crates\pbh\src\default-config.yml' -Raw
$cfg = $cfg -replace 'auto-update: true', 'auto-update: false'
$cfg = $cfg -replace 'http://127.0.0.1:8080', "http://127.0.0.1:$qbPort"
Set-Content "$rustDir\config.yml" -Value $cfg -Encoding utf8

$mock = Start-Process -FilePath 'd:\workspace\peerbanhelper-rs\target\debug\mockqb.exe' `
    -ArgumentList '--port', $qbPort, '--fixture', 'crates\pbh-mockqb\fixtures\sample.json', '--record', $record `
    -RedirectStandardOutput "$root\mock-rust.log" -PassThru
Start-Sleep -Seconds 2

# 非 dry-run：让封禁真实下发到 mock，才能与 Java 侧录制到的集合做对等比较
$p = Start-Process -FilePath 'd:\workspace\peerbanhelper-rs\target\debug\pbh.exe' `
    -ArgumentList '--data', $rustDir, '--port', '9897' `
    -RedirectStandardOutput "$rustDir\stdout.log" -RedirectStandardError "$rustDir\stderr.log" -PassThru
Start-Sleep -Seconds 45
Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue
Stop-Process -Id $mock.Id -Force -ErrorAction SilentlyContinue
Start-Sleep -Seconds 2

Write-Output '=== bans-rust.txt (去重排序) ==='
if (Test-Path $record) { Get-Content $record | Where-Object { $_.Trim() } | Sort-Object -Unique } else { Write-Output '(未录制到任何封禁)' }
Write-Output ''
Write-Output '=== pbh wave 汇总 ==='
Select-String -Path "$rustDir\stdout.log" -Pattern 'wave#' | Select-Object -Last 3 | ForEach-Object { $_.Line }
Write-Output ''
Write-Output '=== Java vs Rust 封禁集合 diff (<= 仅 Java, => 仅 Rust, == 共有) ==='
$javaSet = if (Test-Path "$root\bans-java.txt") { Get-Content "$root\bans-java.txt" | Where-Object { $_.Trim() } | Sort-Object -Unique } else { @() }
$rustSet = if (Test-Path $record) { Get-Content $record | Where-Object { $_.Trim() } | Sort-Object -Unique } else { @() }
Compare-Object -ReferenceObject $javaSet -DifferenceObject $rustSet -IncludeEqual | ForEach-Object {
    $mark = switch ($_.SideIndicator) { '<=' { '仅 Java' } '=>' { '仅 Rust' } '==' { '共有  ' } }
    "$mark  $($_.InputObject)"
}
