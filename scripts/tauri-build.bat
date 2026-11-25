@echo off
rem Build the Tauri client and NSIS installer inside the MSVC environment.
rem Mirrors scripts\cargo.bat: Git Bash's link.exe must not shadow the MSVC linker.
setlocal
set "VSINSTALL="
for /f "usebackq tokens=*" %%i in (`"%ProgramFiles(x86)%\Microsoft Visual Studio\Installer\vswhere.exe" -latest -products * -property installationPath`) do set "VSINSTALL=%%i"
if not defined VSINSTALL set "VSINSTALL=%ProgramFiles(x86)%\Microsoft Visual Studio\2022\BuildTools"
if exist "%VSINSTALL%\VC\Auxiliary\Build\vcvars64.bat" call "%VSINSTALL%\VC\Auxiliary\Build\vcvars64.bat" >nul
set "PATH=%USERPROFILE%\.cargo\bin;%PATH%"

rem The bundle references third_party\kaypanel\dist\kaypanel.exe. Build it when
rem missing so a fresh checkout can go straight to `tauri-build.bat build`.
if not exist "%~dp0..\third_party\kaypanel\dist\kaypanel.exe" (
  echo [tauri-build] kaypanel sidecar missing, building it first...
  call "%~dp0build-kaypanel.bat"
  if errorlevel 1 (
    echo [tauri-build] kaypanel sidecar build failed
    exit /b 1
  )
)

cd /d "%~dp0..\crates\client"
call "..\..\ui\node_modules\.bin\tauri.cmd" %*
