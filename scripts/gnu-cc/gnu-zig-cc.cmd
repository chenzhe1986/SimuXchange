@echo off
node "%~dp0gnu-zig-cc.cjs" %*
exit /b %ERRORLEVEL%
