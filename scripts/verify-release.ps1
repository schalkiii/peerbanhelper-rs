# 发布产物本地校验：sha256 + 包内文件齐全性
# 用法：pwsh -NoProfile -File scripts/verify-release.ps1 -Zip <zip> [-ExpectedSha256 <hex>]
param(
    [Parameter(Mandatory = $true)][string]$Zip,
    [string]$ExpectedSha256
)
$ErrorActionPreference = 'Stop'
if (-not (Test-Path $Zip)) { Write-Error "找不到产物: $Zip"; exit 1 }

$hash = (Get-FileHash $Zip -Algorithm SHA256).Hash.ToLower()
Write-Output "SHA256: $hash"
if ($ExpectedSha256) {
    if ($hash -ne $ExpectedSha256.ToLower()) {
        Write-Error "sha256 不匹配（期望 $ExpectedSha256）"
        exit 1
    }
    Write-Output 'sha256 校验通过'
}

Add-Type -AssemblyName System.IO.Compression.FileSystem
$archive = [IO.Compression.ZipFile]::OpenRead((Resolve-Path $Zip))
try { $names = @($archive.Entries.FullName) } finally { $archive.Dispose() }
foreach ($required in 'pbh.exe', 'pbh-gui.exe', 'WebView2Loader.dll') {
    if ($names -notcontains $required) { Write-Error "包内缺少 $required"; exit 1 }
}
Write-Output '包内文件齐全: pbh.exe / pbh-gui.exe / WebView2Loader.dll'
exit 0
