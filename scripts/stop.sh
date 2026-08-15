#!/usr/bin/env bash
# SimuXchange 后端停止脚本：晚上 10 点由 crontab 调用（也可手动执行）。
#
# 优雅停止：先发 TERM 信号让进程自行收尾，最多等 10 秒，
# 仍未退出再发 KILL 强制结束（一般不会走到这一步）。

if ! pgrep -f "simx-server --listen" > /dev/null 2>&1; then
    echo "[$(date '+%F %T')] simx-server 未在运行，无需停止"
    exit 0
fi

echo "[$(date '+%F %T')] 正在停止 simx-server ..."
pkill -f "simx-server --listen"

# 最多等 10 秒，每 1 秒看一次是否已退出
for i in $(seq 1 10); do
    if ! pgrep -f "simx-server --listen" > /dev/null 2>&1; then
        echo "[$(date '+%F %T')] simx-server 已停止"
        exit 0
    fi
    sleep 1
done

echo "[$(date '+%F %T')] 10 秒内未正常退出，强制结束"
pkill -9 -f "simx-server --listen"
