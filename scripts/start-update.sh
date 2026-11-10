#!/usr/bin/env bash

# 启动 simx-update-server：脚本所在目录即程序目录（release.txt / <版本号> / 日志都在这里）；
# 已在运行则跳过（cron 到点重复触发不冲突）。crontab 示例：0 8 * * * /path/to/start-update.sh
cd "$(dirname "$0")" && pgrep -f "simx-update-server --listen" >/dev/null || (nohup ./simx-update-server --listen 0.0.0.0:9860 >> update-server.log 2>&1 &)
