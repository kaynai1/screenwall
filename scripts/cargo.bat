@echo off
rem Run cargo inside the MSVC environment.
rem Git Bash ships a coreutils link.exe in /usr/bin, which shadows the MSVC linker.
setlocal
set "VSINSTALL="
for /f "usebackq tokens=*" %%i in (`"%ProgramFiles(x86)%\Microsoft Visual Studio\Installer\vswhere.exe" -latest -products * -property installationPath`) do set "VSINSTALL=%%i"
if not defined VSINSTALL set "VSINSTALL=%ProgramFiles(x86)%\Microsoft Visual Studio\2022\BuildTools"
if exist "%VSINSTALL%\VC\Auxiliary\Build\vcvars64.bat" call "%VSINSTALL%\VC\Auxiliary\Build\vcvars64.bat" >nul
set "PATH=%USERPROFILE%\.cargo\bin;%PATH%"
cd /d "%~dp0.."
cargo %*
