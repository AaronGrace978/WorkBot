@echo off
cd /d "%~dp0"
set "APP=%~dp0src-tauri\target\release\tauri-app.exe"

tasklist /FI "IMAGENAME eq tauri-app.exe" /NH 2>nul | find /I "tauri-app.exe" >nul
if not errorlevel 1 exit /b 0

if exist "%APP%" (
  start "" "%APP%"
  exit /b 0
)

where npm >nul 2>&1
if errorlevel 1 (
  echo npm was not found. Install Node.js, then run this again.
  pause
  exit /b 1
)

if not exist node_modules (
  echo Installing dependencies...
  call npm install
  if errorlevel 1 (
    echo npm install failed.
    pause
    exit /b 1
  )
)

echo Building Gemma Work Bot for the first run...
echo This can take a few minutes once. Later starts will be instant.
call npm run tauri build
if errorlevel 1 (
  echo Build failed.
  pause
  exit /b 1
)

if exist "%APP%" (
  start "" "%APP%"
  exit /b 0
)

echo The build finished but the app executable was not found.
pause
exit /b 1
