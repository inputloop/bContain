# SPDX-FileCopyrightText: 2026 Bryant Frankford <nathanielfrankford@gmail.com> and Contributors
# SPDX-License-Identifier: GPL-3.0-or-later OR LGPL-3.0-or-later
# Build bContain for Windows as ONE file you can double-click:
#   dist\bContain.exe   (QEMU embedded inside; unpacked once on first run)
#
# Easiest: double-click build-windows.cmd in the repo root, which checks the
# prerequisites and runs this. Or from PowerShell in the repo root:
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
param(
    [string]$QemuInstaller = "",
    # Where to fetch QEMU's Windows installer; change it if this site is slow or blocked for you.
    [string]$Mirror = "https://qemu.weilnetz.de/w64/"
)
if (-not $Mirror.EndsWith("/")) { $Mirror += "/" }
$ErrorActionPreference = "Stop"

$root  = Split-Path -Parent $PSScriptRoot
$work  = Join-Path $root "dist\qemu-work"
$qemu  = Join-Path $work "qemu"
$payload = Join-Path $work "qemu-payload.tar.gz"

$7z = (Get-Command 7z.exe -ErrorAction SilentlyContinue).Source
if (-not $7z) { $7z = "C:\Program Files\7-Zip\7z.exe" }
if (-not (Test-Path $7z)) { throw "7-Zip not found; install it from https://www.7-zip.org/" }

# Invoke-WebRequest renders a progress bar per chunk on Windows PowerShell 5.1,
# which makes large downloads crawl; downloads below use curl.exe instead.
$ProgressPreference = "SilentlyContinue"
$cache = Join-Path $env:LOCALAPPDATA "bcontain\build-cache"
New-Item -ItemType Directory -Force $cache | Out-Null

function Get-Installer([string]$name) {
    $url = "$Mirror$name"
    $dest = Join-Path $cache $name
    $part = "$dest.partial"
    # The size the server reports, to tell a complete file from a cut-off one.
    $expected = 0
    try { $expected = [int64]@((Invoke-WebRequest -UseBasicParsing -Method Head $url).Headers["Content-Length"])[0] } catch { }
    if ((Test-Path $dest) -and ($expected -eq 0 -or (Get-Item $dest).Length -eq $expected)) { return $dest }
    Remove-Item -Force $dest -ErrorAction SilentlyContinue
    Write-Host "      downloading $name ($([math]::Round($expected / 1MB)) MB); interrupted downloads resume"
    # curl.exe ships with Windows 10 1803+. -C - resumes the .partial, so a
    # connection that drops mid-transfer just continues on the next attempt.
    $done = { (Test-Path $part) -and $expected -gt 0 -and (Get-Item $part).Length -eq $expected }
    for ($attempt = 1; $attempt -le 5 -and -not (& $done); $attempt++) {
        curl.exe -# -L --fail --retry 3 --retry-delay 3 -C - -o $part $url
        if ($LASTEXITCODE -eq 0) { break }
        if ($attempt -eq 5) { throw "download failed (curl exit $LASTEXITCODE): $url" }
        Write-Host "      connection dropped (curl exit $LASTEXITCODE); resuming, attempt $($attempt + 1) of 5"
        Start-Sleep -Seconds 3
    }
    $got = (Get-Item $part).Length
    if ($expected -gt 0 -and $got -ne $expected) {
        Remove-Item -Force $part
        throw "download incomplete: got $got of $expected bytes; run the build again"
    }
    Move-Item -Force $part $dest
    return $dest
}

function Expand-Installer([string]$installer) {
    Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
    New-Item -ItemType Directory -Force $qemu | Out-Null
    # 7-Zip reports damage on stderr; under "Stop", Windows PowerShell would
    # turn its first line into an exception before we can retry.
    $ErrorActionPreference = "Continue"
    $log = & $7z x -y "-o$qemu" $installer 2>&1 | ForEach-Object { "$_" }
    return ($LASTEXITCODE -eq 0 -and (Test-Path (Join-Path $qemu "qemu-system-x86_64.exe"))), ($log -join "`n")
}

Write-Host "[1/4] Getting QEMU for Windows"
$downloaded = -not $QemuInstaller
if ($downloaded) {
    $index = Invoke-WebRequest -UseBasicParsing $Mirror
    $latest = @($index.Links.href | Where-Object { $_ -match '^qemu-w64-setup-\d+\.exe$' } | Sort-Object)[-1]
    if (-not $latest) { throw "could not find a qemu-w64-setup-*.exe installer at $Mirror" }
    $QemuInstaller = Get-Installer $latest
}

Write-Host "[2/4] Unpacking QEMU (without running its installer)"
$ok, $log = Expand-Installer $QemuInstaller
if (-not $ok -and $downloaded) {
    # A damaged cached file ("Unexpected end of archive"): fetch it fresh, once.
    Write-Host "      the cached installer is damaged; downloading it again"
    Remove-Item -Force $QemuInstaller
    $QemuInstaller = Get-Installer $latest
    $ok, $log = Expand-Installer $QemuInstaller
}
if (-not $ok) {
    throw "could not unpack $QemuInstaller with 7-Zip:`n$log`nDelete that file and run the build again."
}

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
"Built from $(Split-Path -Leaf $QemuInstaller) ($Mirror). QEMU source: https://www.qemu.org/download/#source" |
    Set-Content (Join-Path $qemu "QEMU-SOURCE.txt")

Write-Host "[3/4] Packing QEMU into the payload"
tar -czf $payload -C $qemu .
if ($LASTEXITCODE -ne 0) { throw "tar failed" }
Write-Host ("QEMU payload: {0:N0} MB" -f ((Get-Item $payload).Length / 1MB))

Write-Host "[4/4] Compiling bContain.exe with QEMU inside (a few minutes the first time)"
$env:BCONTAIN_EMBED_QEMU = $payload
Push-Location $root
try {
    cargo build --release
    if ($LASTEXITCODE -ne 0) {
        throw "cargo build failed. If the error mentions link.exe, install the C++ build tools: " +
              "winget install Microsoft.VisualStudio.2022.BuildTools --override " +
              "`"--quiet --wait --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended`""
    }
}
finally { Pop-Location; Remove-Item Env:\BCONTAIN_EMBED_QEMU }

$out = Join-Path $root "dist\bContain.exe"
Copy-Item (Join-Path $root "target\release\bcontain.exe") $out -Force
& $out doctor
Write-Host ("wrote dist\bContain.exe ({0:N0} MB): double-click it to start" -f ((Get-Item $out).Length / 1MB))
