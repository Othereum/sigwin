# Joins the PNGs of a directory, by name, into one labelled image: a whole scroll at one look.
# Crops to the conversation pane of the default 1000x680 window unless -Full.
param(
    [Parameter(Mandatory)][string]$Dir,
    [Parameter(Mandatory)][string]$Output,
    [int]$Columns = 4,
    [switch]$Full
)
$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Drawing

$files = @(Get-ChildItem -LiteralPath $Dir -Filter *.png | Sort-Object Name)
if ($files.Count -eq 0) { throw "no PNGs in $Dir" }
$cellW = 350
$cellH = 340
$rows = [math]::Ceiling($files.Count / $Columns)
$bmp = New-Object Drawing.Bitmap ($Columns * $cellW), ($rows * $cellH)
$g = [Drawing.Graphics]::FromImage($bmp)
$font = New-Object Drawing.Font 'Arial', 12
$i = 0
foreach ($f in $files) {
    $im = [Drawing.Image]::FromFile($f.FullName)
    $src = if ($Full) { New-Object Drawing.Rectangle 0, 0, $im.Width, $im.Height } else { New-Object Drawing.Rectangle 300, 100, 700, 530 }
    $dst = New-Object Drawing.Rectangle (($i % $Columns) * $cellW), ([math]::Floor($i / $Columns) * $cellH), $cellW, $cellH
    $g.DrawImage($im, $dst, $src, [Drawing.GraphicsUnit]::Pixel)
    $g.DrawString($f.BaseName, $font, [Drawing.Brushes]::Yellow, $dst.X, $dst.Y)
    $im.Dispose()
    $i++
}
$g.Dispose()
$bmp.Save($Output)
$bmp.Dispose()
