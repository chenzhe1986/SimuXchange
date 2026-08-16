#!/usr/bin/env bash

# 停止 simx-server：先 TERM 优雅退出，最多等 10 秒仍未退出再 KILL；
cd "$(dirname "$0")"
pkill -f "simx-server --listen"
for i in {1..10}; do
    sleep 1
    if ! pgrep -f "simx-server --listen" > /dev/null; then
        break  # 进程已正常退出，不再等待
    fi
done
pkill -9 -f "simx-server --listen" || true

# 无论是否在运行，收尾都会清理 log/ 与 packets/ 下 7 天前的日期目录
# （这两个目录里按 YYYYMMDD 归档的文件夹，如 20260811，只删 7 天前的）。
CUTOFF=$(date -d "7 days ago" +%Y%m%d)
for d in log/*/ packets/*/; do
    [ -d "$d" ] || continue
    name=${d%/}; name=${name##*/}
    [[ "$name" =~ ^[0-9]{8}$ ]] && [ "$name" -lt "$CUTOFF" ] && rm -rf "$d"
done
