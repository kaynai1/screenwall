@echo off
REM Build the kaypanel sidecar (server management module).
REM
REM Output: third_party\kaypanel\dist\kaypanel.exe
REM Bundled by Tauri as an external binary and spawned as a child process.
REM
REM Requires Go 1.23+. If "go" is not on PATH, set GO_BIN to the Go bin dir.
setlocal

set ROOT=%~dp0..
set SRC=%ROOT%\third_party\kaypanel
set OUT=%SRC%\dist\kaypanel.exe

if not exist "%SRC%\go.mod" (
  echo [kaypanel] source dir not found: %SRC%
  exit /b 1
)

if defined GO_BIN set "PATH=%GO_BIN%;%PATH%"

where go >nul 2>&1
if errorlevel 1 (
  echo [kaypanel] Go toolchain not found. Install Go 1.23+ or set GO_BIN.
  exit /b 1
)

if not exist "%SRC%\dist" mkdir "%SRC%\dist"

REM Use a mirror; proxy.golang.org is unreachable on some networks.
if not defined GOPROXY set "GOPROXY=https://goproxy.cn,direct"

pushd "%SRC%"
echo [kaypanel] go mod verify
go mod verify
if errorlevel 1 goto fail

echo [kaypanel] building %OUT%
REM -H windowsgui: no console window (this is a headless background process).
REM -trimpath: strip local absolute paths so the build is reproducible.
go build -trimpath -ldflags "-s -w -H windowsgui -X main.version=0.14.1" -o "%OUT%" .
if errorlevel 1 goto fail
popd

echo [kaypanel] done: %OUT%
exit /b 0

:fail
popd
echo [kaypanel] build failed
exit /b 1
