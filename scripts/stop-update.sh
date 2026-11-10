#!/usr/bin/env bash

# 停止 simx-update-server：先 TERM 优雅退出，最多等 10 秒仍未退出再 KILL；
# 日志文件 update-server.log 保留，便于事后排查。
cd "$(dirname "$0")"
pkill -f "simx-update-server --listen"
for i in {1..10}; do
    sleep 1
    if ! pgrep -f "simx-update-server --listen" > /dev/null; then
        break  # 进程已正常退出，不再等待
    fi
done
pkill -9 -f "simx-update-server --listen" || true
