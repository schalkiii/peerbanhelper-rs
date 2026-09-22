Set-Location 'd:\workspace\peerbanhelper-rs'
$cfg = 'd:\workspace\peerbanhelper-rs\target\live\rust\config\config.yml'
Write-Output '=== 顶层段 ==='
Get-Content $cfg | Where-Object { $_ -match '^\S' } | ForEach-Object { Write-Output ('  ' + $_) }
Write-Output ''
Write-Output '=== client 段条目（凭据掩码）==='
$inClient = $false
$cur = ''
foreach ($l in (Get-Content $cfg)) {
    if ($l -match '^client:') { $inClient = $true; continue }
    if ($l -match '^\S') { $inClient = $false }
    if (-not $inClient) { continue }
    if ($l -match '^  (\S+):') { $cur = $Matches[1]; Write-Output ('  id=' + $cur) }
    if ($l -match '^\s+(type|endpoint|username|rpc-url):\s*(.*)$') {
        $k = $Matches[1]; $v = $Matches[2]
        Write-Output ('      ' + $k + '=' + $v)
    }
    if ($l -match '^\s+(password|token|api-key):') { Write-Output ('      ' + $Matches[1] + '=<masked>') }
}
