# 生成应用占位图标 app-icon.png（1024x1024），供 `tauri icon` 生成全套图标
Add-Type -AssemblyName System.Drawing

$size = 1024
$bmp = New-Object System.Drawing.Bitmap($size, $size)
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.SmoothingMode = 'AntiAlias'
$g.TextRenderingHint = 'AntiAliasGridFit'

# 背景渐变
$rect = New-Object System.Drawing.Rectangle(0, 0, $size, $size)
$brush = New-Object System.Drawing.Drawing2D.LinearGradientBrush(
    $rect,
    [System.Drawing.Color]::FromArgb(255, 34, 82, 190),
    [System.Drawing.Color]::FromArgb(255, 12, 168, 184),
    45)
$g.FillRectangle($brush, $rect)

# 圆角裁剪
$gp = New-Object System.Drawing.Drawing2D.GraphicsPath
$gp.AddEllipse(-200, -200, $size + 400, $size + 400)  # 占位，不裁剪
$g.DrawEllipse([System.Drawing.Pens]::Transparent, 0, 0, 1, 1)

# 中心文字
$font = New-Object System.Drawing.Font('Segoe UI', 300, [System.Drawing.FontStyle]::Bold)
$fmt = New-Object System.Drawing.StringFormat
$fmt.Alignment = 'Center'
$fmt.LineAlignment = 'Center'
$textBrush = New-Object System.Drawing.SolidBrush([System.Drawing.Color]::White)
$g.DrawString('CP', $font, $textBrush, (New-Object System.Drawing.RectangleF(0, -20, $size, $size)), $fmt)

$out = Join-Path $PSScriptRoot 'app-icon.png'
$bmp.Save($out, [System.Drawing.Imaging.ImageFormat]::Png)
$g.Dispose(); $bmp.Dispose()
Write-Output "saved: $out"
