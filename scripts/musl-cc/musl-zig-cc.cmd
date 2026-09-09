@echo off
node "%~dp0musl-zig-cc.cjs" %*
exit /b %ERRORLEVEL%
