# Hazar installer for Windows.
#   irm https://raw.githubusercontent.com/muzafferkadir/hazar/main/install.ps1 | iex
# Downloads the latest installer from GitHub Releases and runs it.

$ErrorActionPreference = 'Stop'
$repo = 'muzafferkadir/hazar'

Write-Host 'Fetching the latest Hazar release…'
$release = Invoke-RestMethod "https://api.github.com/repos/$repo/releases/latest" -Headers @{ 'User-Agent' = 'hazar-installer' }

$asset = $release.assets | Where-Object { $_.name -like '*x64-setup.exe' } | Select-Object -First 1
if (-not $asset) { throw 'No Windows installer (.exe) found in the latest release.' }

$out = Join-Path $env:TEMP $asset.name
Write-Host "Downloading $($asset.name)…"
Invoke-WebRequest $asset.browser_download_url -OutFile $out -UseBasicParsing

Write-Host 'Running the installer…'
Start-Process -FilePath $out -Wait
Write-Host 'Hazar installed.'
