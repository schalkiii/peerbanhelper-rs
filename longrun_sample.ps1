# 长时对跑采样脚本（longrun_sample.ps1）
#
# 用途：数小时~数天规模的 Java/Rust 双跑观测。周期采样两侧进程内存/CPU、SQLite 文件大小、
#       /health 状态，追加 JSONL 供事后绘图；Rust 侧崩溃自动拉起（对账游标存 DB，重启无损）。
#       在线逐条比对刻意不做（时钟漂移 + 重试时序差异会产生假阳性），跑完用
#       `cargo run -p pbh-db --bin compare_dualrun -- <java.db> <rust.db>` 离线对账。
#
# 用法示例：
#   .\longrun_sample.ps1 -JavaPid 12345 -RustExe .\target\release\pbh.exe `
#       -RustArgs "--data D:\pbh-rs\data --dry-run --port 9899" `
#       -RustPort 9899 -JavaDb D:\pbh-java\data\database.sqlite `
#       -RustDb D:\pbh-rs\data\database.sqlite -IntervalSec 300
#
# 停止：Ctrl+C（JSONL 每行即时落盘，随时可中断）。

param(
    [string]$JavaPid = "",
    [string]$JavaProcessName = "java",
    [string]$RustExe = ".\target\release\pbh.exe",
    [string]$RustArgs = "--data .\data --dry-run --port 9899",
    [string]$RustWorkDir = ".",
    [int]$RustPort = 9899,
    [int]$JavaPort = 9898,
    [string]$JavaDb = "",
    [string]$RustDb = "",
    [int]$IntervalSec = 300,
    [switch]$NoAutoRestart,
    [string]$OutFile = "longrun_samples.jsonl",
    [string]$RustLog = "longrun_pbh.log"
)

$ErrorActionPreference = "Continue"
Write-Host "[longrun] 输出：$OutFile；采样间隔 ${IntervalSec}s；Ctrl+C 停止"

function Get-ProcSample([System.Diagnostics.Process]$p, [double]$PrevCpu) {
    if ($null -eq $p) { return $null }
    try {
        $p.Refresh()
        return @{
            pid        = $p.Id
            rss_mb     = [math]::Round($p.WorkingSet64 / 1MB, 1)
            private_mb = [math]::Round($p.PrivateMemorySize64 / 1MB, 1)
            cpu_sec    = [math]::Round($p.TotalProcessorTime.TotalSeconds, 1)
            cpu_delta  = [math]::Round($p.TotalProcessorTime.TotalSeconds - $PrevCpu, 1)
            alive      = -not $p.HasExited
        }
    } catch { return $null }
}

function Get-Health($port) {
    try {
        $resp = Invoke-WebRequest -Uri "http://127.0.0.1:$port/health" -UseBasicParsing -TimeoutSec 5
        return "ok($($resp.StatusCode))"
    } catch { return "down" }
}

function Get-DbSize($path) {
    if ($path -and (Test-Path $path)) { return [math]::Round((Get-Item $path).Length / 1MB, 2) }
    return $null
}

# 启动 Rust 子进程（崩溃自动拉起）
$rustProc = $null
$rustCpuPrev = 0.0
function Start-Rust {
    Write-Host "[longrun] 启动 Rust：$RustExe $RustArgs（工作目录 $RustWorkDir）"
    $script:rustProc = Start-Process -FilePath $RustExe -ArgumentList $RustArgs `
        -WorkingDirectory $RustWorkDir -PassThru -WindowStyle Hidden `
        -RedirectStandardOutput $RustLog -RedirectStandardError "$RustLog.err"
}

if (-not $NoAutoRestart) { Start-Rust }

$round = 0
while ($true) {
    $round += 1
    $now = (Get-Date).ToUniversalTime().ToString("o")

    # --- Java 进程 ---
    $javaProc = $null
    if ($JavaPid) { $javaProc = Get-Process -Id $JavaPid -ErrorAction SilentlyContinue }
    else { $javaProc = Get-Process -Name $JavaProcessName -ErrorAction SilentlyContinue | Select-Object -First 1 }
    $javaSample = Get-ProcSample $javaProc 0.0

    # --- Rust 进程（崩溃自动拉起） ---
    if ($rustProc -and $rustProc.HasExited) {
        Write-Host "[longrun] 警告：Rust 进程已退出（exit=$($rustProc.ExitCode)）"
        if (-not $NoAutoRestart) { Start-Rust }
    }
    $rustSample = Get-ProcSample $rustProc $rustCpuPrev
    if ($rustSample) { $rustCpuPrev = $rustSample.cpu_sec }

    # --- DB 大小 / 健康 ---
    $record = [ordered]@{
        ts          = $now
        round       = $round
        java        = $javaSample
        rust        = $rustSample
        java_health = Get-Health $JavaPort
        rust_health = Get-Health $RustPort
        java_db_mb  = Get-DbSize $JavaDb
        rust_db_mb  = Get-DbSize $RustDb
    }
    $record | ConvertTo-Json -Compress | Add-Content -Path $OutFile
    Write-Host ("[longrun] #{0} java={1}MB rust={2}MB health java={3} rust={4}" -f `
            $round, $record.java_db_mb, $record.rust_db_mb, $record.java_health, $record.rust_health)

    Start-Sleep -Seconds $IntervalSec
}
