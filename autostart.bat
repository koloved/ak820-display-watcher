@echo off
setlocal EnableExtensions EnableDelayedExpansion
title AK820 Display Watcher - Autostart Setup

rem ---------------------------------------------------------------------------
rem  Autostart setup for ak820-display-watcher.exe
rem
rem  Creates or removes a shortcut in the current user's Startup folder, so the
rem  watcher starts automatically when you sign in to Windows.
rem
rem  No administrator rights are needed: the shortcut is per-user.
rem
rem  Optional command-line arguments (skip the menu):
rem      autostart.bat enable
rem      autostart.bat disable
rem      autostart.bat status
rem ---------------------------------------------------------------------------

set "APP_NAME=AK820 Display Watcher"

rem The executable is either next to this script (how the release ships) or in the
rem Cargo output folder (how a source checkout looks).
set "EXE=%~dp0ak820-display-watcher.exe"
if not exist "%EXE%" if exist "%~dp0target\release\ak820-display-watcher.exe" set "EXE=%~dp0target\release\ak820-display-watcher.exe"
for %%I in ("%EXE%") do set "EXE_DIR=%%~dpI"

set "STARTUP_DIR=%APPDATA%\Microsoft\Windows\Start Menu\Programs\Startup"
set "SHORTCUT=%STARTUP_DIR%\ak820-display-watcher.lnk"

rem Arguments make the script usable from a terminal or another script.
rem They also suppress the "press any key" pauses, so the script can run unattended.
set "INTERACTIVE=0"
if /i "%~1"=="enable"  goto :enable
if /i "%~1"=="disable" goto :disable
if /i "%~1"=="status"  goto :status
set "INTERACTIVE=1"

:menu
cls
echo.
echo  ==========================================================
echo    %APP_NAME% - Autostart Setup
echo  ==========================================================
echo.
echo    Application : %EXE%
echo    Shortcut    : %SHORTCUT%
echo.
call :show_status
echo.
echo    [1] Enable autostart   (start with Windows)
echo    [2] Disable autostart  (remove from startup)
echo    [3] Exit
echo.
choice /c 123 /n /m "   Select an option [1-3]: "
set "OPTION=%errorlevel%"

if "%OPTION%"=="1" goto :enable
if "%OPTION%"=="2" goto :disable
if "%OPTION%"=="3" goto :end
goto :menu

rem ---------------------------------------------------------------------------
rem  Enable: create the shortcut in the Startup folder
rem ---------------------------------------------------------------------------
:enable
cls
echo.
echo  Enabling autostart...
echo.

if not exist "%EXE%" (
    echo  [ERROR] Executable not found:
    echo          %EXE%
    echo.
    echo          Put this script next to ak820-display-watcher.exe.
    goto :failed
)

if not exist "%STARTUP_DIR%" (
    echo  [ERROR] Startup folder not found:
    echo          %STARTUP_DIR%
    goto :failed
)

rem Pass paths through the environment to avoid quoting problems with spaces.
set "ENV_EXE=%EXE%"
set "ENV_DIR=%EXE_DIR%"
set "ENV_LNK=%SHORTCUT%"
powershell -NoProfile -ExecutionPolicy Bypass -Command ^
 "$ws = New-Object -ComObject WScript.Shell; $sc = $ws.CreateShortcut($env:ENV_LNK); $sc.TargetPath = $env:ENV_EXE; $sc.WorkingDirectory = $env:ENV_DIR; $sc.Description = 'AJAZZ AK820 backlight watcher'; $sc.Save()"
if errorlevel 1 (
    echo  [ERROR] Could not create the shortcut.
    goto :failed
)

if not exist "%SHORTCUT%" (
    echo  [ERROR] The shortcut was not created.
    goto :failed
)

echo  [OK] Autostart enabled.
echo.
echo       %SHORTCUT%
echo.
echo       The watcher will start automatically after you sign in.
echo       To start it right now, run the exe once by hand.
goto :pause_and_menu

rem ---------------------------------------------------------------------------
rem  Disable: remove the shortcut
rem ---------------------------------------------------------------------------
:disable
cls
echo.
echo  Disabling autostart...
echo.

if not exist "%SHORTCUT%" (
    echo  [OK] Autostart is already disabled - nothing to remove.
    goto :pause_and_menu
)

del /f /q "%SHORTCUT%" 2>nul
if exist "%SHORTCUT%" (
    echo  [ERROR] Could not delete the shortcut. Close any window showing it and retry.
    goto :failed
)

echo  [OK] Autostart disabled.
echo.
echo       Removed: %SHORTCUT%
echo.
echo       A running instance keeps working until you stop it
echo       or sign out of Windows.
goto :pause_and_menu

rem ---------------------------------------------------------------------------
rem  Status: report the current state without changing anything
rem ---------------------------------------------------------------------------
:status
echo.
if exist "%SHORTCUT%" (
    echo  Autostart: ENABLED
    echo  Shortcut : %SHORTCUT%
) else (
    echo  Autostart: DISABLED
)
echo.
exit /b 0

rem ---------------------------------------------------------------------------
rem  Helpers
rem ---------------------------------------------------------------------------
:show_status
if exist "%SHORTCUT%" (
    echo    Status      : ENABLED
) else (
    echo    Status      : DISABLED
)
exit /b 0

:pause_and_menu
if "%INTERACTIVE%"=="1" (
    echo.
    pause
    goto :menu
)
endlocal
exit /b 0

:failed
echo.
echo  Nothing was changed.
if "%INTERACTIVE%"=="1" (
    echo.
    pause
    goto :menu
)
endlocal
exit /b 1

:end
endlocal
exit /b 0
