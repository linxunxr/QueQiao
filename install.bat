@echo off
rem QueQiao one-click installer - double-click to run.
rem cmd wrapper for PowerShell: avoids "double-click .ps1 opens notepad"
rem and ExecutionPolicy restrictions.
chcp 65001 >nul
cd /d "%~dp0"
echo ============================================
echo   QueQiao Installer
echo ============================================
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0install.ps1" %*
if errorlevel 1 (
    echo.
    echo Install failed - please report the red error above at:
    echo   https://github.com/linxunxr/QueQiao/issues
)
echo.
pause
