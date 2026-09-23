# 配置刷新 + 长跑以新 Java PID 重启（epoch 2）
Set-Location 'd:\workspace\peerbanhelper-rs'

$src = 'C:\CommonTools\PeerBanHelper\data'
$data = 'd:\workspace\peerbanhelper-rs\target\live\rust'

Write-Output '=== 1) 刷新副本配置（仅 config.yml，带安全改写）==='
Copy-Item "$src\config\config.yml" "$data\config\config.yml" -Force
$f = "$data\config\config.yml"
$lines = Get-Content $f
$out = New-Object System.Collections.ArrayList
$skip = $false
foreach ($l in $lines) {
    if ($l -match '^(\s*)submit:\s*true\s*$') {
        $null = $out.Add(($l -replace 'submit:\s*true', 'submit: false')); continue
    }
    if ($l -match '^(\s*)auto-update:\s*true\s*$') {
        $null = $out.Add(($l -replace 'auto-update:\s*true', 'auto-update: false')); continue
    }
    if ($l -match '^(push-notification):') {
        $null = $out.Add(($l -split ':')[0] + ': {}')
        $skip = $true; continue
    }
    if ($skip) {
        if ($l -match '^\S') { $skip = $false } else { continue }
    }
    $null = $out.Add($l)
}
Set-Content $f -Value $out -Encoding utf8
Write-Output '安全改写核验:'
Select-String -Path $f -Pattern 'submit:|auto-update:|^push-notification:' |
    ForEach-Object { Write-Output ('  ' + $_.LineNumber + ': ' + $_.Line.Trim()) }

Write-Output '=== 2) 停旧采样（触发最终快照 + 停 Rust）==='
$old = Get-CimInstance Win32_Process -Filter "Name='pwsh.exe'" |
    Where-Object { $_.CommandLine -and $_.CommandLine -match 'longrun_sample' }
foreach ($p in $old) {
    Write-Output ("  停止采样 PID " + $p.ProcessId)
    Stop-Process -Id $p.ProcessId -Force -ErrorAction SilentlyContinue
}
Start-Sleep -Seconds 3
# 采样脚本被强杀可能来不及做 finally；兜底停 Rust 子进程
Get-Process pbh -ErrorAction SilentlyContinue | ForEach-Object {
    Write-Output ("  停止 Rust PID " + $_.Id)
    Stop-Process -Id $_.Id -Force -ErrorAction SilentlyContinue
}
Start-Sleep -Seconds 2

Write-Output '=== 3) 记录 epoch ==='
$epoch = @"
epoch2: $(Get-Date -Format 'yyyy-MM-ddTHH:mm:sszzz')
  说明: qB 假活（20:40~21:0x 本地）恢复后，用户重启 Java PBH（新 PID 176436，21:04:33）。
        Rust 侧同一数据目录继续（历史累计），本 epoch 起两侧同时在线，可作对账窗口起点。
"@
Add-Content -Path 'd:\workspace\peerbanhelper-rs\target\live\longrun_epochs.txt' -Value $epoch

Write-Output '=== 4) 以新 Java PID 启动长跑 ==='
Start-Process -FilePath 'pwsh.exe' -ArgumentList @(
    '-NoProfile', '-File', 'd:\workspace\peerbanhelper-rs\longrun_sample.ps1',
    '-JavaPid', '176436',
    '-RustExe', 'd:\workspace\peerbanhelper-rs\target\release\pbh.exe',
    '-RustDataDir', 'd:\workspace\peerbanhelper-rs\target\live\rust',
    '-RustPort', '9899',
    '-RustTag', 'rust-longrun',
    '-RustWorkDir', 'd:\workspace\peerbanhelper-rs',
    '-JavaPort', '9898',
    '-JavaDb', 'C:\CommonTools\PeerBanHelper\data\persist\peerbanhelper-nt.db',
    '-RustDb', 'd:\workspace\peerbanhelper-rs\target\live\rust\persist\peerbanhelper-nt.db',
    '-SnapshotDir', 'd:\workspace\peerbanhelper-rs\target\live\snapshots',
    '-SnapshotEveryRounds', '3',
    '-KeepSnapshots', '16',
    '-IntervalSec', '300',
    '-OutFile', 'd:\workspace\peerbanhelper-rs\target\live\longrun_samples.jsonl',
    '-RustLog', 'd:\workspace\peerbanhelper-rs\target\live\longrun_pbh.log'
) -WindowStyle Hidden
Write-Output '已启动；等待 45 秒核查'
Start-Sleep -Seconds 45

Write-Output '=== 5) 核查 ==='
Get-Process pbh -ErrorAction SilentlyContinue |
    Select-Object Id, @{n = 'RSS_MB'; e = { [math]::Round($_.WorkingSet64 / 1MB, 1) } } |
    Format-Table -AutoSize | Out-String
foreach ($p in @(9898, 9899)) {
    try { Write-Output ("  :$p => " + (Invoke-WebRequest -Uri "http://127.0.0.1:$p/health" -UseBasicParsing -TimeoutSec 5).StatusCode) }
    catch { Write-Output "  :$p => down" }
}
Write-Output '=== Rust wave ==='
Select-String -Path 'd:\workspace\peerbanhelper-rs\target\live\longrun_pbh.log' -Pattern 'wave#' |
    Select-Object -Last 2 | ForEach-Object { $_.Line -replace '\u001b\[[0-9;]*m', '' }
Write-Output '=== 采样 ==='
Get-Content 'd:\workspace\peerbanhelper-rs\target\live\longrun_samples.jsonl' -Tail 2
