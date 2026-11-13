@echo off
rem 双击启动工具桥接（Windows）。关掉窗口即停止。
setlocal
cd /d "%~dp0"

if "%BRIDGE_PORT%"=="" set "BRIDGE_PORT=8124"
if "%UPSTREAM_BASE%"=="" set "UPSTREAM_BASE=http://127.0.0.1:3065/v1"

echo [tool-bridge] 监听 http://127.0.0.1:%BRIDGE_PORT%/v1
echo [tool-bridge] 上游 %UPSTREAM_BASE%
echo [tool-bridge] Cursor 里把 Override OpenAI Base URL 指到上面的地址
echo.

node bridge.mjs
echo.
echo [tool-bridge] 已退出。
pause
