param(
    # fixture 文件名（crates\pbh-mockqb\fixtures\ 下）；需与 Java 侧跑同一个 fixture，
    # 录制文件按 Tag 区分（默认 sample 对应 bans-java-sample.txt）
    [string]$Fixture = 'sample.json',
    [string]$Tag = 'sample'
)
Set-Location 'd:\workspace\peerbanhelper-rs'
$root = 'd:\workspace\peerbanhelper-rs\target\dualrun'
$rustDir = "$root\rust"
$qbPort = '18080'
$record = "$root\bans-rust-$Tag.txt"
$javaRecord = "$root\bans-java-$Tag.txt"
Remove-Item -Recurse -Force $rustDir -ErrorAction SilentlyContinue
Remove-Item -Force $record -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $rustDir | Out-Null

# 与 Java 侧对齐的两处改造：关 GeoIP 自动更新、下载器指向 mock
$cfg = Get-Content 'crates\pbh\src\default-config.yml' -Raw
$cfg = $cfg -replace 'auto-update: true', 'auto-update: false'
$cfg = $cfg -replace 'http://127.0.0.1:8080', "http://127.0.0.1:$qbPort"
Set-Content "$rustDir\config.yml" -Value $cfg -Encoding utf8

$mock = Start-Process -FilePath 'd:\workspace\peerbanhelper-rs\target\debug\mockqb.exe' `
    -ArgumentList '--port', $qbPort, '--fixture', "crates\pbh-mockqb\fixtures\$Fixture", '--record', $record `
    -RedirectStandardOutput "$root\mock-rust.log" -PassThru

# 复用 Java 现役 GeoIP 库：auto-update=false 时缺失的库仍会触发补下（阻塞启动），
# 直接铺一份现成 mmdb，两侧 GeoIP 输入也保持一致
$geoSrc = 'C:\CommonTools\PeerBanHelper\data\ipdb\geoip'
$geoDst = "$rustDir\ipdb\geoip"
if (Test-Path $geoSrc) {
    New-Item -ItemType Directory -Force -Path $geoDst | Out-Null
    Copy-Item "$geoSrc\*.mmdb" $geoDst -Force
}
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
# IP 归一化：去掉增量下发 raw_ip 里的 ":port" 后缀（Java 走全量时不带端口，
# Rust 走增量时带端口；两侧语义等价，归一后消除显示噪声）
function Normalize-Ip([string]$s) { return ($s -replace ':\d+$', '') }
$javaSet = if (Test-Path $javaRecord) { Get-Content $javaRecord | Where-Object { $_.Trim() } | ForEach-Object { Normalize-Ip $_.Trim() } | Sort-Object -Unique } else { @() }
$rustSet = if (Test-Path $record) { Get-Content $record | Where-Object { $_.Trim() } | ForEach-Object { Normalize-Ip $_.Trim() } | Sort-Object -Unique } else { @() }
Compare-Object -ReferenceObject $javaSet -DifferenceObject $rustSet -IncludeEqual | ForEach-Object {
    $mark = switch ($_.SideIndicator) { '<=' { '仅 Java' } '=>' { '仅 Rust' } '==' { '共有  ' } }
    "$mark  $($_.InputObject)"
}
