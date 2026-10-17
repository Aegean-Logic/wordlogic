<#
.SYNOPSIS
  Renders the Windows installer's branding: banner.svg to banner.bmp (24-bit, as Windows
  Installer dialogs need).

.DESCRIPTION
  Run after changing banner.svg or the Aegean Logic brand files it uses; commit both. Needs resvg
  (cargo install resvg) and Windows (System.Drawing does the PNG to BMP step).

.EXAMPLE
  pwsh packaging/windows/branding/generate.ps1
#>
$ErrorActionPreference = 'Stop'
$here = $PSScriptRoot
if (-not (Get-Command resvg -ErrorAction SilentlyContinue)) { throw 'resvg not found (cargo install resvg)' }
Add-Type -AssemblyName System.Drawing

foreach ($name in 'banner') {
  $png = Join-Path ([IO.Path]::GetTempPath()) "wordlogic-$name.png"
  # Fonts come from the system (Segoe UI on Windows).
  & resvg --resources-dir $here (Join-Path $here "$name.svg") $png
  if ($LASTEXITCODE -ne 0) { throw "resvg failed on $name.svg" }
  $source = [System.Drawing.Image]::FromFile($png)
  try {
    # Flattened onto 24-bit RGB: Windows Installer doesn't take alpha.
    $bmp = New-Object System.Drawing.Bitmap $source.Width, $source.Height, ([System.Drawing.Imaging.PixelFormat]::Format24bppRgb)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.Clear([System.Drawing.Color]::FromArgb(0x0A, 0x2A, 0x3B))
    $g.DrawImage($source, 0, 0, $source.Width, $source.Height)
    $g.Dispose()
    $bmp.Save((Join-Path $here "$name.bmp"), [System.Drawing.Imaging.ImageFormat]::Bmp)
    $bmp.Dispose()
  } finally {
    $source.Dispose()
    Remove-Item -Force $png -ErrorAction SilentlyContinue
  }
  Write-Output "wrote $name.bmp"
}
