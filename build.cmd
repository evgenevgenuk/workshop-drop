@echo off
rem Builds WorkshopDrop. Adds cargo and (if installed via winget) WinLibs MinGW to PATH.
set "PATH=%USERPROFILE%\.cargo\bin;%PATH%"
for /d %%D in ("%LOCALAPPDATA%\Microsoft\WinGet\Packages\BrechtSanders.WinLibs*") do set "PATH=%%D\mingw64\bin;%PATH%"
cd /d "%~dp0"
cargo build --release || (pause & exit /b 1)
echo.
echo Done: %~dp0target\release\WorkshopDrop.exe
pause
