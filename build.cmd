@echo off
rem ask-with-snip build script: run frontend logic tests, then tauri build (NSIS installer).
rem Usage: build.cmd [extra tauri args, e.g. --no-bundle]
setlocal
cd /d "%~dp0"

echo [1/3] Frontend logic tests...
call npm run test:frontend
if errorlevel 1 (
  echo.
  echo Frontend tests FAILED - build aborted. See FAIL lines above.
  exit /b 1
)

where rustc >nul 2>nul
if errorlevel 1 (
  echo.
  echo [ERROR] rustc not found. Install the Rust toolchain first: https://rustup.dev
  exit /b 1
)

rem A running copy (tray) locks the exe and Windows refuses to overwrite it.
rem Use the absolute path: under Git Bash the bare name hits GNU find instead.
%SystemRoot%\System32\tasklist.exe | %SystemRoot%\System32\find.exe /I "ask-with-snip.exe" >nul
if not errorlevel 1 (
  echo Old ask-with-snip.exe is running - stopping it so the new exe can replace it...
  taskkill /IM ask-with-snip.exe /F >nul 2>nul
  %SystemRoot%\System32\timeout.exe /t 1 /nobreak >nul
)

echo [2/3] Tauri build (release + NSIS installer, first run compiles for a while)...
call npm run tauri -- build %*
if errorlevel 1 (
  echo.
  echo [ERROR] tauri build failed. See cargo output above.
  exit /b 1
)

echo [3/3] Done. Artifacts:
echo   app        src-tauri\target\release\ask-with-snip.exe
echo   installer  src-tauri\target\release\bundle\nsis\ask-with-snip_0.1.0_x64-setup.exe
endlocal
