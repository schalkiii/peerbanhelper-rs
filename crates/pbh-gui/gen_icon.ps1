# 生成 Tauri 所需的最小图标集（32x32 纯色 + 简单图案），输出到 crates/pbh-gui/icons/
# 仅需运行一次；产物已提交仓库，改图标时重新运行。
$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Drawing

$dir = Join-Path $PSScriptRoot "icons"
New-Item -ItemType Directory -Force -Path $dir | Out-Null

$bmp = New-Object System.Drawing.Bitmap 32, 32
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.Clear([System.Drawing.Color]::FromArgb(255, 24, 144, 255))   # ArcoDesign 蓝
$g.FillEllipse([System.Drawing.Brushes]::White, 8, 8, 16, 16)   # 中心圆
$g.FillEllipse([System.Drawing.Brushes]::Red, 13, 13, 6, 6)     # 中心红点（封禁）
$g.Dispose()

# PNG（托盘与 bundle 用）
$bmp.Save((Join-Path $dir "icon.png"), [System.Drawing.Imaging.ImageFormat]::Png)
$bmp.Save((Join-Path $dir "32x32.png"), [System.Drawing.Imaging.ImageFormat]::Png)

# ICO（Windows 窗口/托盘用）
$icon = [System.Drawing.Icon]::FromHandle($bmp.GetHicon())
$stream = [System.IO.File]::Create((Join-Path $dir "icon.ico"))
$icon.Save($stream)
$stream.Close()

$bmp.Dispose()
$icon.Dispose()
Write-Host "图标已生成：$dir\{icon.png, 32x32.png, icon.ico}"
