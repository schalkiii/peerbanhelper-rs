# 9093 连接失败延迟精确测量（3 次）
foreach ($i in 1..3) {
    $c = New-Object System.Net.Sockets.TcpClient
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    try {
        $ok = $c.ConnectAsync('127.0.0.1', 9093).Wait(3000)
        $sw.Stop()
        Write-Output ("  第$i 次: ok=$ok  " + $sw.Elapsed.TotalMilliseconds + "ms")
    } catch {
        $sw.Stop()
        Write-Output ("  第$i 次: 异常 " + $sw.Elapsed.TotalMilliseconds + "ms  " + $_.Exception.Message)
    } finally { $c.Close() }
}

Write-Output '=== Rust 日志中与 9093 相关的行 ==='
Select-String -Path 'd:\workspace\peerbanhelper-rs\target\live\longrun_pbh.log' -Pattern '9093' |
    Select-Object -Last 5 | ForEach-Object { $_.Line -replace '\u001b\[[0-9;]*m', '' }
