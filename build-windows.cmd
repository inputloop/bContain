@echo off
@REM SPDX-FileCopyrightText: 2026 Bryant Frankford <nathanielfrankford@gmail.com> and Contributors
@REM SPDX-License-Identifier: GPL-3.0-or-later OR LGPL-3.0-or-later
REM Build bContain.exe (one file, QEMU inside). Double-click this file.
REM Needs: Rust (rustup) and 7-Zip. Offers to install them with winget.
setlocal
cd /d "%~dp0"
title Building bContain.exe

where cargo >nul 2>nul
if errorlevel 1 (
  echo Rust is not installed.
  echo.
  echo bContain is built with Rust. The installer also offers to set up the
  echo Visual Studio C++ build tools Rust needs on Windows: accept that option.
  choice /M "Install Rust now with winget"
  if errorlevel 2 goto :need_rust
  winget install --id Rustlang.Rustup -e --accept-package-agreements --accept-source-agreements
  echo.
  echo Rust installed. Close this window and double-click build-windows.cmd again
  echo so the new PATH is picked up.
  pause
  exit /b 1
)

if not exist "%ProgramFiles%\7-Zip\7z.exe" (
  where 7z >nul 2>nul
  if errorlevel 1 (
    echo 7-Zip is not installed. It is used to unpack the QEMU installer without running it.
    choice /M "Install 7-Zip now with winget"
    if errorlevel 2 goto :need_7zip
    winget install --id 7zip.7zip -e --accept-package-agreements --accept-source-agreements
  )
)

powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0scripts\bundle-qemu-windows.ps1"
if errorlevel 1 (
  echo.
  echo Build failed; see the messages above.
  pause
  exit /b 1
)
echo.
echo Done: dist\bContain.exe
explorer /select,"%~dp0dist\bContain.exe"
pause
exit /b 0

:need_rust
echo Install Rust from https://rustup.rs and run this again.
pause
exit /b 1

:need_7zip
echo Install 7-Zip from https://www.7-zip.org and run this again.
pause
exit /b 1
