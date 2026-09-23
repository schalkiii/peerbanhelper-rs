# 准备 Rust 实机 dry-run 对跑的数据目录副本（绝不改动部署路径下的任何文件）。
# 用法：pwsh -NoProfile -File prepare_live.ps1
Set-Location 'd:\workspace\peerbanhelper-rs'

$src  = 'C:\CommonTools\PeerBanHelper\data'
$root = 'd:\workspace\peerbanhelper-rs\target\live'
$data = "$root\rust"

if (Test-Path $data) { Remove-Item $data -Recurse -Force }
New-Item -ItemType Directory -Force -Path "$data\config" | Out-Null
# persist/ 目录存在 ⇒ Rust 采用上游 DB 布局 `<data>/persist/peerbanhelper-nt.db`
# （与长时对跑/快照/对账工具链的路径假设一致）
New-Item -ItemType Directory -Force -Path "$data\persist" | Out-Null

# 1) 配置与 GeoIP 库（判定依据一致）
Copy-Item "$src\config\config.yml"  "$data\config\config.yml"  -Force
Copy-Item "$src\config\profile.yml" "$data\config\profile.yml" -Force
if (Test-Path "$src\ipdb")    { Copy-Item "$src\ipdb"    "$data\ipdb"    -Recurse -Force }
# 2) 社区表达式脚本（.av）——对跑行为一致性的关键输入
if (Test-Path "$src\scripts") { Copy-Item "$src\scripts" "$data\scripts" -Recurse -Force }

# 3) 仅在副本上做安全改写（dry-run 不挡 BTN 上报与告警推送）：
#    a) btn.submit: true -> false（不向 BTN 网络提交数据）
#    b) auto-update: true -> false（离线环境启动卡在下载 mmdb）
#    c) push-notification 段清空（不向真实渠道推送）
foreach ($f in @("$data\config\config.yml", "$data\config\profile.yml")) {
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
}

Write-Output '=== 副本就绪 ==='
Get-ChildItem $data -Recurse -Depth 1 -Name | ForEach-Object { Write-Output ('  ' + $_) }
Write-Output '=== 安全改写核验 ==='
Select-String -Path "$data\config\config.yml","$data\config\profile.yml" -Pattern 'submit:|auto-update:|^push-notification:' | ForEach-Object {
    Write-Output ('  ' + (Split-Path -Leaf $_.Path) + ':' + $_.LineNumber + '  ' + $_.Line.Trim())
}
Write-Output '=== 脚本目录 ==='
Get-ChildItem "$data\scripts" -Name -ErrorAction SilentlyContinue | ForEach-Object { Write-Output ('  ' + $_) }
