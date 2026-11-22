# Downloads a prebuilt PDFium DLL (bblanchon/pdfium-binaries) into third_party\pdfium.
# Set $env:PDFIUM_VERSION (e.g. 8165) to pin a release; default: latest.
$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '..')
$arch = if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { 'arm64' } else { 'x64' }
$asset = "pdfium-win-$arch.tgz"
$url = if ($env:PDFIUM_VERSION) {
    "https://github.com/bblanchon/pdfium-binaries/releases/download/chromium%2F$($env:PDFIUM_VERSION)/$asset"
} else {
    "https://github.com/bblanchon/pdfium-binaries/releases/latest/download/$asset"
}
$tmp = Join-Path ([IO.Path]::GetTempPath()) ([Guid]::NewGuid())
New-Item -ItemType Directory $tmp | Out-Null
Write-Host "Downloading $url"
Invoke-WebRequest $url -OutFile "$tmp\pdfium.tgz"
tar -xzf "$tmp\pdfium.tgz" -C $tmp
New-Item -ItemType Directory -Force third_party\pdfium | Out-Null
Copy-Item "$tmp\bin\pdfium.dll" third_party\pdfium\
Copy-Item "$tmp\LICENSE" third_party\pdfium\LICENSE
Remove-Item -Recurse -Force $tmp
Write-Host "PDFium installed to third_party\pdfium\pdfium.dll"
