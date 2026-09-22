Set-Location 'd:\workspace\peerbanhelper-rs'
$src = 'C:\CommonTools\PeerBanHelper\data'
$dst = 'd:\workspace\peerbanhelper-rs\target\live\rust'
Remove-Item -Recurse -Force $dst -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path "$dst\config" | Out-Null

# 复制实机配置（上游布局：config/config.yml + config/profile.yml）
Copy-Item "$src\config\config.yml" "$dst\config\config.yml"
Copy-Item "$src\config\profile.yml" "$dst\config\profile.yml"
# 一并复制 GeoIP 库，保证地理维度判定依据与实机完全一致
if (Test-Path "$src\ipdb") { Copy-Item "$src\ipdb" "$dst\ipdb" -Recurse }

# 仅在我的副本上做两处安全改写，绝不改动用户原配置：
#  1) 关闭 BTN 上报——dry-run 不挡 BTN 提交，避免用用户凭据向 BTN 网络发数据
#  2) 关闭 GeoIP 自动更新——库已复制齐全，避免启动时联网下载 20MB mmdb
$c = Get-Content "$dst\config\config.yml" -Raw
$c = $c -replace '(?m)^(\s*)submit:\s*true', '$1submit: false'
$c = $c -replace '(?m)^(\s*)auto-update:\s*true', '$1auto-update: false'
Set-Content "$dst\config\config.yml" -Value $c -Encoding utf8

# 3) 清空推送渠道——dry-run 同样不挡告警推送，避免在对跑期间往用户真实渠道发通知
$lines = Get-Content "$dst\config\config.yml"
$out = New-Object System.Collections.ArrayList
$skip = $false
foreach ($l in $lines) {
    if ($l -match '^(push|push-notification):') {
        $null = $out.Add((($l -split ':')[0]) + ': {}')
        $skip = $true
        continue
    }
    if ($skip) {
        if ($l -match '^\S') { $skip = $false } else { continue }
    }
    $null = $out.Add($l)
}
Set-Content "$dst\config\config.yml" -Value $out -Encoding utf8

Write-Output '=== 已准备 Rust 对跑目录（用户原配置未改动）==='
Write-Output $dst
Get-ChildItem $dst -Recurse -Depth 2 | ForEach-Object { Write-Output ('  ' + $_.FullName.Replace($dst, '')) }

Write-Output ''
Write-Output '=== 配置摘要（敏感值已掩码）==='
$lines = Get-Content "$dst\config\config.yml"
$inBtn = $false
foreach ($line in $lines) {
    if ($line -match '^btn:') { $inBtn = $true; Write-Output 'btn:'; continue }
    if ($line -match '^\S') { $inBtn = $false }
    if ($inBtn -and $line -match '^\s+\S') {
        if ($line -match 'secret|app-id|installation-id') {
            Write-Output ('  ' + ($line -split ':')[0] + ': <masked>')
        }
        else { Write-Output ('  ' + $line) }
    }
}
Write-Output ('下载器条目数: ' + (Select-String -Path "$dst\config\config.yml" -Pattern '^\s{2}- name:' | Measure-Object).Count)
Write-Output ('下载器类型: ' + ((Select-String -Path "$dst\config\config.yml" -Pattern 'type: (qbittorrent|transmission|deluge|biglybt|bitcomet|aria2next)' | ForEach-Object { $_.Matches[0].Groups[1].Value }) -join ', '))
Write-Output ('push 渠道: ' + (Select-String -Path "$dst\config\config.yml" -Pattern '^\s{2}\S+:\s*$' | Measure-Object).Count)
