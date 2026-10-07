# PBH 看门狗：探测 WebUI 健康端点 + 日志心跳，冻结时重启 GUI/pbh。
#
# 背景：长跑中观察到进程冻结（整进程 CPU 归零但 web 线程仍响应），根因分析中
# （见 docs/TESTING.md pending-6）。在根因定位前用外部看门狗兜底：
#   - 健康端点不响应，或
#   - 日志最后修改时间超过 MaxStaleSec（判定“心跳停止”）
# 任一命中即判定冻结 → 结束 pbh-gui / pbh → 重新拉起 GUI（GUI 负责拉起 pbh 子进程）。
#
# 用法（需有桌面会话的账户运行 GUI）：
#   pwsh -NoProfile -File scripts\pbh-watchdog.ps1 -GuiExe <pbh-gui.exe> -PbhPath <pbh.exe> -DataDir <data> -Port 9898
#
# 常驻：用「任务计划程序」在计算机启动时运行上面命令（勾选“不管用户是否登录都要运行”并
# 指定有桌面会话的账户），或用 NSSM/WinSW 包装。

param(
    [string]$GuiExe = 'C:\CommonTools\PeerBanHelper-rs\pbh-gui.exe',
    [string]$PbhPath = 'C:\CommonTools\PeerBanHelper-rs\pbh.exe',
    [string]$DataDir = 'C:\CommonTools\PeerBanHelper-rs\data',
    [int]$Port = 9898,
    # wave 完成心跳：pbh 日志中「主循环返回」= ban wave 完成（main 线程存活标志）。
    # 生产 wave 间隔 120s（config.yml），阈值取 300s（含启动初始化 ~20s 与一个完整间隔）。
    # 注意不能用进程 CPU 增量：wave 间隔内 main 线程空闲、仅 BTN 线程每 5s 打日志，
    # 30s 窗口 CPU 增量恒 < 0.5s，会对健康进程误杀（2026-10-07 教训）。
    [int]$WaveStaleSec = 300,
    [int]$IntervalSec = 30,
    [int]$HealthTimeoutSec = 5
)

$HealthUrl = "http://127.0.0.1:$Port/health"
$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$LogFile = Join-Path $ScriptDir 'watchdog.log'
$LogCandidates = @(
    (Join-Path $DataDir 'pbh-gui.log'),
    (Join-Path (Split-Path -Parent $GuiExe) 'pbh-gui.log')
)

function Write-Log([string]$msg) {
    $line = "{0} {1}" -f (Get-Date).ToString('s'), $msg
    Add-Content -Path $LogFile -Value $line -Encoding utf8
    Write-Output $line
}

function Get-HeartbeatFile {
    foreach ($p in $LogCandidates) {
        if (Test-Path $p) { return Get-Item $p }
    }
    return $null
}

# 从 pbh 日志尾部提取最近一次 wave 完成的时间（UTC）。
# wave 完成标志：`主循环返回`（pbh main.rs 的 debug! 打点）。
# 返回 [DateTimeOffset] 或 $null（日志不存在/无完成记录）。
function Get-LastWaveDone {
    $f = Get-HeartbeatFile
    if (-not $f) { return $null }
    try {
        $fs = [System.IO.File]::Open($f.FullName, 'Open', 'Read', 'ReadWrite')
        try {
            $len = $fs.Length
            $bufSize = [Math]::Min(65536, $len)
            $fs.Seek(-$bufSize, 'End') | Out-Null
            $buf = New-Object byte[] $bufSize
            $null = $fs.Read($buf, 0, $bufSize)
            $text = [System.Text.Encoding]::UTF8.GetString($buf)
        } finally { $fs.Close() }
    } catch { return $null }
    # 逆序找最后一条「主循环返回」行的时间戳
    $lines = $text -split "`n"
    for ($i = $lines.Count - 1; $i -ge 0; $i--) {
        if ($lines[$i] -match '主循环返回' -and $lines[$i] -match '(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?)Z') {
            try { return [DateTimeOffset]::Parse($Matches[1] + 'Z') } catch { return $null }
        }
    }
    return $null
}

function Test-Healthy {
    try {
        $resp = Invoke-WebRequest -Uri $HealthUrl -UseBasicParsing -TimeoutSec $HealthTimeoutSec
        if ($resp.StatusCode -ne 200) { return $false, "health=$($resp.StatusCode)" }
    } catch {
        return $false, 'health 请求失败'
    }
    return $true, 'ok'
}

function Restart-Pbh {
    Write-Log '判定冻结：结束现有进程并重启'
    Get-Process pbh-gui, pbh -ErrorAction SilentlyContinue | ForEach-Object {
        Write-Log ("结束进程 {0} (PID {1})" -f $_.ProcessName, $_.Id)
        Stop-Process -Id $_.Id -Force -ErrorAction SilentlyContinue
    }
    Start-Sleep -Seconds 3
    # GUI 在端口空闲时会拉起 pbh 子进程；端口被占用时为附加模式
    Write-Log ("启动 GUI: {0} --pbh-path {1} --data-dir {2} --port {3}" -f $GuiExe, $PbhPath, $DataDir, $Port)
    Start-Process -FilePath $GuiExe -ArgumentList @('--pbh-path', "`"$PbhPath`"", '--data-dir', "`"$DataDir`"", '--port', "$Port")
    Start-Sleep -Seconds 15
}

Write-Log "看门狗启动（health=$HealthUrl, wave-stale=${WaveStaleSec}s, interval=${IntervalSec}s）"
# 重启/脚本启动后的容忍期：等新实例完成初始化 + 首轮 wave，其间不做 wave 心跳判定
$skipWaveCheckUntil = (Get-Date).ToUniversalTime().AddSeconds(60)

while ($true) {
    $ok, $reason = Test-Healthy
    if (-not $ok) {
        Write-Log "异常：$reason"
        Restart-Pbh
        $skipWaveCheckUntil = (Get-Date).ToUniversalTime().AddSeconds($WaveStaleSec)
        Start-Sleep -Seconds $IntervalSec
        continue
    }

    # wave 完成心跳：main 线程每 120s 一轮 wave，完成即打「主循环返回」日志。
    # 该日志停滞超阈值 = wave 循环冻结（BTN 线程仍每 5s 打日志，不能证明 main 存活）
    if ((Get-Date).ToUniversalTime() -ge $skipWaveCheckUntil) {
        $lastWave = Get-LastWaveDone
        if ($null -ne $lastWave) {
            $age = ((Get-Date).ToUniversalTime() - $lastWave).TotalSeconds
            if ($age -gt $WaveStaleSec) {
                Write-Log ("异常：wave 心跳停滞 {0:N0}s > {1}s（最后完成 {2:HH:mm:ss}Z）" -f $age, $WaveStaleSec, $lastWave.DateTime)
                Restart-Pbh
                $skipWaveCheckUntil = (Get-Date).ToUniversalTime().AddSeconds($WaveStaleSec)
                Start-Sleep -Seconds $IntervalSec
                continue
            }
        }
    }

    Start-Sleep -Seconds $IntervalSec
}
