# 一键部署：release 构建 -> 停服务 -> 复制 -> 验证健康
# 根治「改完代码忘记重跑 cargo build --release / 漏部署」的教训
param([switch]$SkipBuild)
$ErrorActionPreference = 'Stop'
$srcExe = 'd:\workspace\peerbanhelper-rs\target\release\pbh.exe'
$dstDir = 'C:\CommonTools\PeerBanHelper-rs'
$dstExe = Join-Path $dstDir 'pbh.exe'
$dataDir = Join-Path $dstDir 'data'
$port = 9898
Set-Location 'd:\workspace\peerbanhelper-rs'
if (-not $SkipBuild) {
    Write-Output '[1/4] cargo build --release -p pbh'
    cargo build --release -p pbh 2>&1 | Select-Object -Last 1
    if ($LASTEXITCODE -ne 0) { Write-Error '构建失败'; exit 1 }
} else { Write-Output '[1/4] 跳过构建' }
Write-Output '[2/4] 停止现有实例'
Get-Process pbh -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Seconds 2
Write-Output '[3/4] 复制产物'
Copy-Item $srcExe $dstExe -Force
Write-Output ('  已部署: ' + (Get-Item $dstExe).LastWriteTime)
Write-Output '[4/4] 启动 GUI（监督线程拉起 pbh）'
Start-Process (Join-Path $dstDir 'pbh-gui.exe') -ArgumentList @('--pbh-path', $dstExe, '--data-dir', $dataDir, '--port', "$port")
$ready = $false
for ($i = 0; $i -lt 20; $i++) {
    Start-Sleep -Seconds 2
    try { $null = Invoke-WebRequest -Uri "http://127.0.0.1:$port/health" -UseBasicParsing -TimeoutSec 3; $ready = $true; break } catch { }
}
if (-not $ready) { Write-Error 'health 未就绪，检查 data\pbh-gui.log'; exit 1 }
Write-Output ('部署完成，health 200')
exit 0
