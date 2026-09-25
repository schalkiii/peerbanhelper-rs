param(
    # fixture 文件名（crates\pbh-mockqb\fixtures\ 下）；录制文件按 Tag 区分
    [string]$Fixture = 'sample.json',
    [string]$Tag = 'sample'
)
Set-Location 'd:\workspace\peerbanhelper-rs'
$java = 'C:\CommonTools\PeerBanHelper\jre\bin\java.exe'
$jar = 'C:\CommonTools\PeerBanHelper\PeerBanHelper.jar'
$root = 'd:\workspace\peerbanhelper-rs\target\dualrun'
$dir = "$root\java"
$qbPort = '18080'
# 9899 已被长跑 Rust 实例占用，Java 临时实例改用 9896
$webPort = '9896'
$record = "$root\bans-java-$Tag.txt"
Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue
Remove-Item -Force $record -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $dir | Out-Null

# 1) 首次启动：让 PBH 生成默认 config.yml / profile.yml
$p1 = Start-Process -FilePath $java -ArgumentList "-Dpbh.datadir=$dir", '-jar', $jar `
    -WorkingDirectory $dir -RedirectStandardOutput "$dir\boot1.log" -RedirectStandardError "$dir\boot1.err" -PassThru
Start-Sleep -Seconds 20
Stop-Process -Id $p1.Id -Force -ErrorAction SilentlyContinue
Start-Sleep -Seconds 2

# 2) 对跑改造：
#    - 关闭 GeoIP 自动更新（本机到 GitHub 仅约 15KB/s，20MB 的 mmdb 会把启动拖死）
#    - WebUI 改到 9899：9898 已被本机另一个【已初始化】的 PBH 实例占用，
#      而 OOBE 路由只在“未初始化”时注册，打到它上面会返回“路由不可用”
$cfgPath = "$dir\config\config.yml"
$cfg = Get-Content $cfgPath -Raw
$cfg = $cfg -replace 'auto-update: true', 'auto-update: false'
$cfg = $cfg -replace 'http://127.0.0.1:9898', "http://127.0.0.1:$webPort"
$cfg = $cfg -replace 'http: 9898', "http: $webPort"
Set-Content $cfgPath -Value $cfg -Encoding utf8
Remove-Item -Recurse -Force "$dir\ipdb" -ErrorAction SilentlyContinue
# 2.5) 注入确定性回归规则集（ip 黑名单 CIDR/单 IP/端口、城市、client-name REGEX；与 Rust 侧一致）
# Java 慢启动时 20 秒可能尚未生成 profile.yml，先等待再注入，失败即中止（避免静默丢规则）
$deadline = (Get-Date).AddSeconds(30)
while (-not (Test-Path "$dir\config\profile.yml") -and (Get-Date) -lt $deadline) { Start-Sleep -Seconds 2 }
if (-not (Test-Path "$dir\config\profile.yml")) { throw "首启 30 秒内未生成 profile.yml，无法注入测试规则" }
python crates\pbh-mockqb\inject_test_profile.py "$cfgPath" "$dir\config\profile.yml"
if ($LASTEXITCODE -ne 0) { throw "规则注入失败" }
# 复用 Java 现役 GeoIP 库（避免首启触发缓慢的在线下载；两侧 GeoIP 输入保持一致）
$geoSrc = 'C:\CommonTools\PeerBanHelper\data\ipdb\geoip'
if (Test-Path $geoSrc) {
    New-Item -ItemType Directory -Force -Path "$dir\ipdb\geoip" | Out-Null
    Copy-Item "$geoSrc\*.mmdb" "$dir\ipdb\geoip" -Force
}

# 3) 启动 mock qB（录制收到的封禁下发）
$mock = Start-Process -FilePath 'd:\workspace\peerbanhelper-rs\target\debug\mockqb.exe' `
    -ArgumentList '--port', $qbPort, '--fixture', "crates\pbh-mockqb\fixtures\$Fixture", '--record', $record `
    -RedirectStandardOutput "$root\mock.log" -PassThru
Start-Sleep -Seconds 2

# 4) 启动 Java PBH 并等待 WebUI 就绪
$j = Start-Process -FilePath $java -ArgumentList "-Dpbh.datadir=$dir", '-jar', $jar `
    -WorkingDirectory $dir -RedirectStandardOutput "$dir\stdout.log" -RedirectStandardError "$dir\stderr.log" -PassThru
Start-Sleep -Seconds 20

# 5) OOBE 初始化：设置 server.token 并注册指向 mock 的下载器
$body = @{
    token      = 'dualrun-token'
    downloader = @{
        id     = 'mockqb'
        config = @{
            name             = 'mockqb'
            type             = 'qbittorrent'
            endpoint         = "http://127.0.0.1:$qbPort"
            username         = 'admin'
            password         = 'adminadmin'
            'ignore-private' = $true
            'increment-ban'  = $true
        }
    }
} | ConvertTo-Json -Depth 5
try {
    $resp = Invoke-RestMethod -Uri "http://127.0.0.1:$webPort/api/oobe/init" -Method Post -Body $body -ContentType 'application/json'
    Write-Output '=== OOBE response ==='
    $resp | ConvertTo-Json -Depth 5
}
catch {
    Write-Output "OOBE failed: $_"
}

# 6) 让 ban wave 跑若干轮
Start-Sleep -Seconds 45
Stop-Process -Id $j.Id -Force -ErrorAction SilentlyContinue
Stop-Process -Id $mock.Id -Force -ErrorAction SilentlyContinue
Start-Sleep -Seconds 2

Write-Output ''
Write-Output '=== bans-java.txt (去重排序) ==='
if (Test-Path $record) { Get-Content $record | Where-Object { $_.Trim() } | Sort-Object -Unique } else { Write-Output '(未录制到任何封禁)' }
Write-Output ''
Write-Output '=== java stdout tail ==='
Get-Content "$dir\stdout.log" -Tail 25
