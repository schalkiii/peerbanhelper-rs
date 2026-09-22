Set-Location 'd:\workspace\peerbanhelper-rs'
$root = 'd:\workspace\peerbanhelper-rs\target\live'
$data = "$root\rust"
$javaLog = 'C:\CommonTools\PeerBanHelper\data\logs\latest.log'
$port = '9897'
$runSeconds = 600   # 实机 check-interval=120s，跑 10 分钟取约 5 轮样本

function Read-LogShared($path) {
    # 用户的 PBH 独占打开日志，必须用 FileShare.ReadWrite 才能读
    $fs = [System.IO.File]::Open($path, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read, [System.IO.FileShare]::ReadWrite)
    $sr = New-Object System.IO.StreamReader($fs)
    $txt = $sr.ReadToEnd()
    $sr.Close(); $fs.Close()
    return $txt
}

$token = ''
$inServer = $false
foreach ($l in (Get-Content "$data\config\config.yml")) {
    if ($l -match '^server:') { $inServer = $true; continue }
    if ($l -match '^\S') { $inServer = $false }
    if ($inServer -and $l -match '^\s*token:\s*(.*)$') { $token = $Matches[1].Trim().Trim("'").Trim('"'); break }
}

$javaProc = Get-Process javaw -ErrorAction SilentlyContinue | Select-Object -First 1
$t0 = Get-Date

$rp = Start-Process -FilePath 'target\release\pbh.exe' `
    -ArgumentList '--dry-run', '--data', $data, '--port', $port `
    -RedirectStandardOutput "$root\rust.log" -RedirectStandardError "$root\rust.err" -PassThru

Start-Sleep -Seconds 150
$rssEarly = (Get-Process -Id $rp.Id -ErrorAction SilentlyContinue).WorkingSet64
Start-Sleep -Seconds ($runSeconds - 150)
$rssLate = (Get-Process -Id $rp.Id -ErrorAction SilentlyContinue).WorkingSet64

$rustBans = @()
try {
    $resp = Invoke-RestMethod -Uri "http://127.0.0.1:$port/api/bans" -Headers @{ Authorization = "Bearer $token" } -TimeoutSec 15
    $resp.data | ConvertTo-Json -Depth 8 | Set-Content "$root\rust_bans.json" -Encoding utf8
    $rustBans = @($resp.data)
}
catch { Write-Output ('bans api failed: ' + $_.Exception.Message) }

$javaRss = 0
if ($javaProc) { $javaRss = (Get-Process -Id $javaProc.Id -ErrorAction SilentlyContinue).WorkingSet64 }
Stop-Process -Id $rp.Id -Force -ErrorAction SilentlyContinue
Start-Sleep -Seconds 2
$t1 = Get-Date

$javaText = Read-LogShared $javaLog
Set-Content "$root\java_full.log" -Value $javaText -Encoding utf8

# 按时间窗过滤（用户 PBH 可能随时重启/截断日志，字节偏移不可靠）
$t0s = $t0.TimeOfDay; $t1s = $t1.TimeOfDay
$windowLines = @()
foreach ($line in ($javaText -split "`n")) {
    if ($line -match '^\[(\d{2}):(\d{2}):(\d{2})\]') {
        $ts = [TimeSpan]::FromHours([int]$Matches[1]) + [TimeSpan]::FromMinutes([int]$Matches[2]) + [TimeSpan]::FromSeconds([int]$Matches[3])
        if ($t1s -ge $t0s) { $ok = ($ts -ge $t0s) -and ($ts -le $t1s) }
        else { $ok = ($ts -ge $t0s) -or ($ts -le $t1s) }
        if ($ok) { $windowLines += $line }
    }
}
Set-Content "$root\java_window.log" -Value ($windowLines -join "`n") -Encoding utf8

$rustTimes = @(Select-String -Path "$root\rust.log" -Pattern 'wave#' | ForEach-Object {
        if ($_.Line -match '耗时=(\d+)ms') { [int]$Matches[1] }
    })
$javaTimes = @(Select-String -Path "$root\java_window.log" -Pattern '已检查' | ForEach-Object {
        if ($_.Line -match '\((\d+)ms\)') { [int]$Matches[1] }
    })
function Med($arr) { if (-not $arr -or $arr.Count -eq 0) { return $null }; $s = $arr | Sort-Object; return $s[[math]::Floor($s.Count / 2)] }

Write-Output ('=== 对跑窗口 ' + $t0.ToString('HH:mm:ss') + ' ~ ' + $t1.ToString('HH:mm:ss') + ' （Java PID ' + $javaProc.Id + '）===')
Write-Output ('Rust wave 耗时 : ' + ($rustTimes -join ', ') + '  [n=' + $rustTimes.Count + ', 中位 ' + (Med $rustTimes) + ' ms]')
Write-Output ('Java wave 耗时 : ' + ($javaTimes -join ', ') + '  [n=' + $javaTimes.Count + ', 中位 ' + (Med $javaTimes) + ' ms]')
Write-Output ('Rust RSS       : ' + [math]::Round($rssEarly / 1MB) + ' MB -> ' + [math]::Round($rssLate / 1MB) + ' MB')
Write-Output ('Java RSS(实机) : ' + [math]::Round($javaRss / 1MB) + ' MB')

$rustIps = @($rustBans | ForEach-Object { if ($_.ip) { $_.ip } elseif ($_.address -and $_.address.ip) { $_.address.ip } } | Where-Object { $_ } | Sort-Object -Unique)
$javaIps = @(Select-String -Path "$root\java_window.log" -Pattern '\[封禁\]' | ForEach-Object {
        if ($_.Line -match 'ip=([0-9a-fA-F\.:]+)') { $Matches[1] }
    } | Sort-Object -Unique)
Write-Output ('Rust 封禁 IP 数 : ' + $rustIps.Count)
Write-Output ('Java 封禁 IP 数 : ' + $javaIps.Count)
Write-Output '--- IP 集合 diff ---'
Compare-Object -ReferenceObject $javaIps -DifferenceObject $rustIps -IncludeEqual | ForEach-Object {
    $mark = switch ($_.SideIndicator) { '<=' { '仅 Java' } '=>' { '仅 Rust' } '==' { '共有  ' } }
    "$mark  $($_.InputObject)"
}
