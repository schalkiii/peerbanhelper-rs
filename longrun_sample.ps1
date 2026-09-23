# 长时对跑采样脚本（longrun_sample.ps1）
#
# 用途：数小时~数天规模的 Java/Rust 双跑观测。周期采样两侧进程内存/CPU、SQLite 文件大小、
#       磁盘水位、/health 状态，追加 JSONL 供事后绘图；Rust 侧崩溃自动拉起（对账游标存 DB，重启无损）；
#       周期对两侧 DB 做**共享读快照**（SQLite 主库 + -wal + -shm，可在运行中安全拷贝），
#       跑完用 `cargo run -p pbh-db --bin compare_dualrun -- <java快照> <rust快照>` 离线对账。
#
# 在线逐条比对刻意不做（时钟漂移 + 重试时序差异会产生假阳性），以离线 DB 对账为准。
#
# 用法示例：
#   .\longrun_sample.ps1 -JavaPid 12345 -RustExe .\target\release\pbh.exe `
#       -RustDataDir D:\pbh-rs\data -RustPort 9899 -RustTag rust-longrun `
#       -JavaDb C:\...\data\persist\peerbanhelper-nt.db `
#       -RustDb D:\pbh-rs\data\persist\peerbanhelper-nt.db `
#       -SnapshotDir .\target\live\snapshots -IntervalSec 300
#
# 停止：Ctrl+C（JSONL 每行即时落盘；退出时做最终快照并结束 Rust 子进程，除非 -NoStopRustOnExit）。

param(
    [string]$JavaPid = "",
    [string]$JavaProcessName = "java",
    # Java 进程命令行正则：设置后**每轮动态解析** PID（PBH 重启/多实例收敛后采样不中断），
    # 优先于 -JavaPid / -JavaProcessName
    [string]$JavaMatch = "",
    [string]$RustExe = ".\target\release\pbh.exe",
    # Rust 子进程参数（结构化，避免 Start-Process 对含空格字符串的引号歧义）
    [string]$RustDataDir = ".\data",
    [bool]$RustDryRun = $true,
    [string]$RustTag = "rust-longrun",
    [string[]]$RustExtraArgs = @(),
    [string]$RustWorkDir = ".",
    [int]$RustPort = 9899,
    [int]$JavaPort = 9898,
    [string]$JavaDb = "",
    [string]$RustDb = "",
    [int]$IntervalSec = 300,
    # 快照目录：周期把两侧 DB（含 -wal/-shm）共享读拷贝到此，供离线对账
    [string]$SnapshotDir = "",
    # 每 N 轮做一次快照（0 = 仅退出时快照一次）
    [int]$SnapshotEveryRounds = 12,
    # 周期快照保留份数（最新 N 份；退出时的 r0-* 最终快照永不清理）
    [int]$KeepSnapshots = 12,
    # 磁盘剩余空间低于该值（GB）时告警并在 JSONL 打 flag
    [double]$DiskFreeWarnGb = 5.0,
    # 单库超过该值（GB）时告警（天级 history 增长的磁盘水位）
    [double]$DbSizeWarnGb = 2.0,
    [string]$DiskPath = "D:",
    [switch]$NoAutoRestart,
    [switch]$NoStopRustOnExit,
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
    $total = 0.0
    $found = $false
    foreach ($p in @($path, "$path-wal")) {
        if ($p -and (Test-Path $p)) {
            $total += (Get-Item $p).Length / 1MB
            $found = $true
        }
    }
    if ($found) { return [math]::Round($total, 2) }
    return $null
}

# 共享读拷贝（FileShare.ReadWrite）：Java 的 SQLite 在运行中也能安全取证；
# 主库 + -wal + -shm 一并拷贝，快照可被 SQLite 正常恢复后打开。
function Copy-Shared([string]$src, [string]$dst) {
    if (-not $src -or -not (Test-Path $src)) { return $false }
    try {
        $in = [System.IO.File]::Open($src, [System.IO.FileMode]::Open,
            [System.IO.FileAccess]::Read, [System.IO.FileShare]::ReadWrite)
        try {
            $out = [System.IO.File]::Create($dst)
            try { $in.CopyTo($out) } finally { $out.Close() }
        } finally { $in.Close() }
        return $true
    } catch {
        Write-Host "[longrun] 快照失败 $src => $dst : $($_.Exception.Message)"
        return $false
    }
}

function Snapshot-Dbs([int]$round) {
    if (-not $SnapshotDir) { return }
    $stamp = Get-Date -Format "yyyyMMdd-HHmmss"
    $dir = Join-Path $SnapshotDir "r$round-$stamp"
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    foreach ($pair in @(@("java", $JavaDb), @("rust", $RustDb))) {
        $side = $pair[0]; $db = $pair[1]
        if (-not $db) { continue }
        $base = Split-Path -Leaf $db
        $ok = Copy-Shared $db (Join-Path $dir "$side-$base")
        foreach ($suffix in @("-wal", "-shm")) {
            $null = Copy-Shared "$db$suffix" (Join-Path $dir "$side-$base$suffix")
        }
        Write-Host "[longrun] 快照 $side ($ok) => $dir"
    }
    # 保留策略：周期快照只留最新 N 份（r0-* 最终快照不清理），约束多天运行的磁盘占用
    if ($KeepSnapshots -gt 0) {
        $periodic = Get-ChildItem $SnapshotDir -Directory |
            Where-Object { $_.Name -notlike 'r0-*' } |
            Sort-Object LastWriteTime
        if ($periodic.Count -gt $KeepSnapshots) {
            $periodic | Select-Object -First ($periodic.Count - $KeepSnapshots) | ForEach-Object {
                Write-Host "[longrun] 清理旧快照 $($_.Name)"
                Remove-Item $_.FullName -Recurse -Force -ErrorAction SilentlyContinue
            }
        }
    }
}

# 启动 Rust 子进程（崩溃自动拉起）
$rustProc = $null
$rustCpuPrev = 0.0
function Start-Rust {
    $argList = @('--data', $RustDataDir, '--port', "$RustPort")
    if ($RustDryRun) { $argList += '--dry-run' }
    if ($RustTag) { $argList += @('--tag', $RustTag) }
    if ($RustExtraArgs.Count -gt 0) { $argList += $RustExtraArgs }
    Write-Host "[longrun] 启动 Rust：$RustExe $($argList -join ' ')（工作目录 $RustWorkDir）"
    $script:rustProc = Start-Process -FilePath $RustExe -ArgumentList $argList `
        -WorkingDirectory $RustWorkDir -PassThru -WindowStyle Hidden `
        -RedirectStandardOutput $RustLog -RedirectStandardError "$RustLog.err"
}

if (-not $NoAutoRestart) { Start-Rust }

$round = 0
try {
    while ($true) {
        $round += 1
        $now = (Get-Date).ToUniversalTime().ToString("o")

        # --- Java 进程（-JavaMatch 时每轮按命令行动态解析，PBH 重启不影响采样）---
        $javaProc = $null
        if ($JavaMatch) {
            $hit = Get-CimInstance Win32_Process -Filter "Name='javaw.exe' or Name='java.exe'" -ErrorAction SilentlyContinue |
                Where-Object { $_.CommandLine -and $_.CommandLine -match $JavaMatch } |
                Sort-Object CreationDate | Select-Object -First 1
            if ($hit) { $javaProc = Get-Process -Id $hit.ProcessId -ErrorAction SilentlyContinue }
        } elseif ($JavaPid) {
            $javaProc = Get-Process -Id $JavaPid -ErrorAction SilentlyContinue
        } else {
            $javaProc = Get-Process -Name $JavaProcessName -ErrorAction SilentlyContinue | Select-Object -First 1
        }
        $javaSample = Get-ProcSample $javaProc 0.0

        # --- Rust 进程（崩溃自动拉起） ---
        if ($rustProc -and $rustProc.HasExited) {
            Write-Host "[longrun] 警告：Rust 进程已退出（exit=$($rustProc.ExitCode)）"
            if (-not $NoAutoRestart) { Start-Rust }
        }
        $rustSample = Get-ProcSample $rustProc $rustCpuPrev
        if ($rustSample) { $rustCpuPrev = $rustSample.cpu_sec }

        # --- 磁盘水位 ---
        $disk = $null
        $freeGb = $null
        try {
            $d = Get-PSDrive -Name ($DiskPath.TrimEnd(':')) -ErrorAction Stop
            $freeGb = [math]::Round($d.Free / 1GB, 2)
            $disk = @{
                drive       = $DiskPath
                free_gb     = $freeGb
                low         = ($freeGb -lt $DiskFreeWarnGb)
            }
            if ($freeGb -lt $DiskFreeWarnGb) {
                Write-Host "[longrun] 警告：磁盘剩余 ${freeGb}GB 低于阈值 ${DiskFreeWarnGb}GB"
            }
        } catch { }

        $javaDbMb = Get-DbSize $JavaDb
        $rustDbMb = Get-DbSize $RustDb
        $dbWarn = @()
        if ($javaDbMb -and $javaDbMb -gt ($DbSizeWarnGb * 1024)) { $dbWarn += "java" }
        if ($rustDbMb -and $rustDbMb -gt ($DbSizeWarnGb * 1024)) { $dbWarn += "rust" }
        if ($dbWarn.Count -gt 0) {
            Write-Host "[longrun] 警告：数据库体积超阈值（$($dbWarn -join ',')，> ${DbSizeWarnGb}GB）"
        }

        # --- DB 大小 / 健康 ---
        $record = [ordered]@{
            ts          = $now
            round       = $round
            java        = $javaSample
            rust        = $rustSample
            java_health = Get-Health $JavaPort
            rust_health = Get-Health $RustPort
            java_db_mb  = $javaDbMb
            rust_db_mb  = $rustDbMb
            disk        = $disk
            db_warn     = $dbWarn
        }
        $record | ConvertTo-Json -Compress | Add-Content -Path $OutFile
        Write-Host ("[longrun] #{0} java={1}MB rust={2}MB free={3}GB health java={4} rust={5}" -f `
                $round, $record.java_db_mb, $record.rust_db_mb, $freeGb, $record.java_health, $record.rust_health)

        if ($SnapshotDir -and $SnapshotEveryRounds -gt 0 -and ($round % $SnapshotEveryRounds) -eq 0) {
            Snapshot-Dbs $round
        }

        Start-Sleep -Seconds $IntervalSec
    }
} finally {
    Write-Host "[longrun] 退出：做最终快照"
    Snapshot-Dbs 0
    if (-not $NoStopRustOnExit -and $rustProc -and -not $rustProc.HasExited) {
        Write-Host "[longrun] 结束 Rust 子进程 pid=$($rustProc.Id)"
        Stop-Process -Id $rustProc.Id -Force -ErrorAction SilentlyContinue
    }
}
