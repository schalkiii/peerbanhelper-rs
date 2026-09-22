Set-Location 'd:\workspace\peerbanhelper-rs'
$torrentCount = 20
$peersPerTorrent = 100
$torrents = New-Object System.Collections.ArrayList
$peersMap = [ordered]@{}

for ($t = 0; $t -lt $torrentCount; $t++) {
    $hash = 'BB' + $t.ToString('D38')
    $null = $torrents.Add([ordered]@{
            hash        = $hash
            name        = "bench torrent $t"
            progress    = 1.0
            total_size  = 4876000000
            piece_size  = 16384
            pieces_have = 297241
            dlspeed     = 0
            upspeed     = 0
            is_private  = $false
        })

    $peers = New-Object System.Collections.ArrayList
    for ($p = 0; $p -lt $peersPerTorrent; $p++) {
        # 198.18.x.y 不在 ignore-peers-from-addresses 内，确保全部进入判定；
        # 第三字节随 p 变化，使每个 /24 只有 1 个 peer，避免整片被多拨规则命中
        $oct3 = ($t * 5 + $p) % 250
        $ip = "198.18.$oct3.1"
        # 约 5% 的 peer 使用 -HP 前缀，命中 peer-id 黑名单，让封禁路径也被基准覆盖
        $isBad = ($p % 20 -eq 0)
        if ($isBad) {
            $peerId = "-HP001-bench$t-$p"
            $client = 'BitComet 2.0'
        }
        else {
            $peerId = "-qB4550-bench$t-$p"
            $client = 'qBittorrent/4.5.2'
        }
        $null = $peers.Add([ordered]@{
                ip             = $ip
                port           = 6881
                client         = $client
                peer_id_client = $peerId
                progress       = 0.3
                flags          = ''
                dl_speed       = 1000
                downloaded     = 100000
                up_speed       = 2000
                uploaded       = 200000
                connection     = 'TCP'
            })
    }
    $peersMap[$hash] = $peers
}

$obj = [ordered]@{
    version     = '5.0.0'
    buildinfo   = [ordered]@{ libtorrent = '1.2.19.0'; qt = '6.5.0' }
    preferences = [ordered]@{ enable_multi_connections_from_same_ip = $true }
    torrents    = $torrents
    peers       = $peersMap
}
$obj | ConvertTo-Json -Depth 8 | Set-Content 'crates\pbh-mockqb\fixtures\bench.json' -Encoding utf8
Write-Output "已生成 bench.json：$torrentCount 个 torrent / $($torrentCount * $peersPerTorrent) 个 peer"
