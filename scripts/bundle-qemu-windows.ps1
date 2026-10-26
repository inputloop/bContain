# Build a standalone bContain folder for Windows:
#   dist\bcontain-windows\bcontain.exe
#   dist\bcontain-windows\qemu\qemu-system-x86_64.exe (+ DLLs, firmware)
#
# Usage (PowerShell, from the repo root):
#   cargo build --release
#   .\scripts\bundle-qemu-windows.ps1 [-QemuInstaller path\to\qemu-w64-setup.exe]
#
# Without -QemuInstaller the latest build is downloaded from
# https://qemu.weilnetz.de/w64/ (Stefan Weil's official Windows builds).
# Needs 7-Zip (7z.exe) on PATH or in "C:\Program Files\7-Zip" to unpack the
# NSIS installer without running it.
param([string]$QemuInstaller = "")
$ErrorActionPreference = "Stop"

$root = Split-Path -Parent $PSScriptRoot
$dist = Join-Path $root "dist\bcontain-windows"
$exe  = Join-Path $root "target\release\bcontain.exe"
if (-not (Test-Path $exe)) { throw "build first: cargo build --release" }

$7z = (Get-Command 7z.exe -ErrorAction SilentlyContinue).Source
if (-not $7z) { $7z = "C:\Program Files\7-Zip\7z.exe" }
if (-not (Test-Path $7z)) { throw "7-Zip not found; install it from https://www.7-zip.org/" }

if (-not $QemuInstaller) {
    $index = Invoke-WebRequest -UseBasicParsing "https://qemu.weilnetz.de/w64/"
    $latest = ($index.Links.href | Where-Object { $_ -match '^qemu-w64-setup-\d+\.exe$' } | Sort-Object)[-1]
    if (-not $latest) { throw "could not find an installer on qemu.weilnetz.de/w64/" }
    $QemuInstaller = Join-Path $env:TEMP $latest
    Write-Host "downloading $latest"
    Invoke-WebRequest -UseBasicParsing "https://qemu.weilnetz.de/w64/$latest" -OutFile $QemuInstaller
}

Remove-Item -Recurse -Force $dist -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force (Join-Path $dist "qemu") | Out-Null
& $7z x -y "-o$(Join-Path $dist 'qemu')" $QemuInstaller | Out-Null
# The NSIS payload carries installer leftovers we do not need.
Remove-Item -Recurse -Force (Join-Path $dist 'qemu\$PLUGINSDIR'), (Join-Path $dist 'qemu\qemu-uninstall.exe') -ErrorAction SilentlyContinue
if (-not (Test-Path (Join-Path $dist "qemu\qemu-system-x86_64.exe"))) { throw "qemu-system-x86_64.exe missing after extraction" }

Copy-Item $exe $dist
Copy-Item (Join-Path $root "README.md") $dist
& (Join-Path $dist "bcontain.exe") doctor
Compress-Archive -Force -Path $dist -DestinationPath (Join-Path $root "dist\bcontain-windows.zip")
Write-Host "wrote dist\bcontain-windows.zip"
