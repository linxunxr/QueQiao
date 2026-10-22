@echo off
rem QueQiao upgrade deploy (DSH/SnowLuma already installed) - double-click to run.
chcp 65001 >nul
cd /d "%~dp0"
echo ============================================
echo   QueQiao Upgrade Deploy
echo ============================================
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0scripts\setup-dsh.ps1" %*
if errorlevel 1 (
    echo.
    echo Deploy failed - please report the red error above at:
    echo   https://github.com/linxunxr/QueQiao/issues
)
echo.
pause
