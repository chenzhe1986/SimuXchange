#!/usr/bin/env bash

# 启动 simx-server：脚本所在目录即数据目录（gateways.json / packets/ / server.log 都在这里）；
# 已在运行则跳过（cron 到点重复触发不冲突）。crontab 示例：0 8 * * * /path/to/start.sh
cd "$(dirname "$0")" && pgrep -f "simx-server --listen" >/dev/null || (nohup ./simx-server --listen 0.0.0.0:9800 --auto-start >> server.log 2>&1 &)
