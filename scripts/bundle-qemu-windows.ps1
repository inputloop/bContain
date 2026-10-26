# SPDX-FileCopyrightText: 2026 Bryant Frankford <nathanielfrankford@gmail.com> and Contributors
# SPDX-License-Identifier: GPL-3.0-or-later OR LGPL-3.0-or-later
# Build bContain for Windows as ONE file you can double-click:
#   dist\bContain.exe   (QEMU embedded inside; unpacked once on first run)
#
# Usage (PowerShell, from the repo root):
#   .\scripts\bundle-qemu-windows.ps1 [-QemuInstaller path\to\qemu-w64-setup.exe]
#
# Without -QemuInstaller the latest build is downloaded from
# https://qemu.weilnetz.de/w64/ (Stefan Weil's Windows builds of QEMU, which
# QEMU's own download page links to). Needs 7-Zip to unpack the NSIS
# installer without running it, plus Rust (cargo) and Windows' built-in tar.
#
# Licensing: QEMU is GPL-2.0 and stays a separate program that bContain runs.
# Its COPYING/LICENSE files travel inside the payload, and the installer name
# (which pins the exact upstream build and its source) is recorded in
# QEMU-SOURCE.txt next to them.
param([string]$QemuInstaller = "")
$ErrorActionPreference = "Stop"

$root  = Split-Path -Parent $PSScriptRoot
$work  = Join-Path $root "dist\qemu-work"
$qemu  = Join-Path $work "qemu"
$payload = Join-Path $work "qemu-payload.tar.gz"

$7z = (Get-Command 7z.exe -ErrorAction SilentlyContinue).Source
if (-not $7z) { $7z = "C:\Program Files\7-Zip\7z.exe" }
if (-not (Test-Path $7z)) { throw "7-Zip not found; install it from https://www.7-zip.org/" }

if (-not $QemuInstaller) {
    $index = Invoke-WebRequest -UseBasicParsing "https://qemu.weilnetz.de/w64/"
    $latest = ($index.Links.href | Where-Object { $_ -match '^qemu-w64-setup-\d+\.exe$' } | Sort-Object)[-1]
    if (-not $latest) { throw "could not find an installer on qemu.weilnetz.de/w64/" }
    $QemuInstaller = Join-Path $env:TEMP $latest
    if (-not (Test-Path $QemuInstaller)) {
        Write-Host "downloading $latest"
        Invoke-WebRequest -UseBasicParsing "https://qemu.weilnetz.de/w64/$latest" -OutFile $QemuInstaller
    }
}

Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force $qemu | Out-Null
& $7z x -y "-o$qemu" $QemuInstaller | Out-Null
if (-not (Test-Path (Join-Path $qemu "qemu-system-x86_64.exe"))) { throw "qemu-system-x86_64.exe missing after extraction" }

# Keep only what an x86_64 host needs: drop the other ~30 system emulators,
# installer leftovers, docs and other architectures' firmware.
Remove-Item -Recurse -Force (Join-Path $qemu '$PLUGINSDIR'), (Join-Path $qemu 'qemu-uninstall.exe') -ErrorAction SilentlyContinue
Get-ChildItem $qemu -Filter "qemu-system-*.exe" | Where-Object { $_.Name -ne "qemu-system-x86_64.exe" } | Remove-Item -Force
Get-ChildItem $qemu -Directory | Where-Object { $_.Name -in @("doc", "docs", "include") } | Remove-Item -Recurse -Force
$share = Join-Path $qemu "share"
if (Test-Path $share) {
    $foreign = 'aarch64|arm|riscv|loongarch|s390|ppc|hppa|openbios|opensbi|u-boot|skiboot|slof|palcode|qemu_vga\.ndrv|petalogix|bamboo|canyonlands|npcm|sparc|m68k|mips|microblaze|alpha|xtensa'
    Get-ChildItem $share -File | Where-Object { $_.Name -match $foreign } | Remove-Item -Force
}
"Built from $(Split-Path -Leaf $QemuInstaller) (https://qemu.weilnetz.de/w64/). QEMU source: https://www.qemu.org/download/#source" |
    Set-Content (Join-Path $qemu "QEMU-SOURCE.txt")

tar -czf $payload -C $qemu .
if ($LASTEXITCODE -ne 0) { throw "tar failed" }
Write-Host ("QEMU payload: {0:N0} MB" -f ((Get-Item $payload).Length / 1MB))

$env:BCONTAIN_EMBED_QEMU = $payload
Push-Location $root
try { cargo build --release; if ($LASTEXITCODE -ne 0) { throw "cargo build failed" } }
finally { Pop-Location; Remove-Item Env:\BCONTAIN_EMBED_QEMU }

$out = Join-Path $root "dist\bContain.exe"
Copy-Item (Join-Path $root "target\release\bcontain.exe") $out -Force
& $out doctor
Write-Host ("wrote dist\bContain.exe ({0:N0} MB): double-click it to start" -f ((Get-Item $out).Length / 1MB))
