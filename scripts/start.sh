#!/usr/bin/env bash
# SimuXchange 后端启动脚本：早上 8 点由 crontab 调用（也可手动执行）。
#
# 部署（以用户名 user 为例，全部放在用户目录下，无需 root）：
#   1. 把编译好的 simx-server 与本脚本放到同一目录：
#      ~/simx-server/simx-server
#      ~/simx-server/start.sh
#      ~/simx-server/stop.sh
#      并加执行权限：chmod +x ~/simx-server/start.sh ~/simx-server/stop.sh
#   2. 执行 crontab -e 加入定时启停（把 user 换成实际用户名）：
#      0 8 * * *  /home/user/simx-server/start.sh   # 每天 08:00 启动
#      0 22 * * * /home/user/simx-server/stop.sh    # 每天 22:00 停止
#   3. 查看定时任务：crontab -l
#   4. 日志：追加写到 ~/simx-server/server.log（网关配置 gateways.json、
#      报文文件 packets/ 与日志同目录，即 simx-server 所在目录），排查问题用：
#      tail -f ~/simx-server/server.log

# 脚本所在目录（crontab 环境变量少，不依赖当前工作目录）；
# 数据目录 = simx-server 所在目录（gateways.json / packets/ 都在这里）
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
SERVER="$SCRIPT_DIR/simx-server"
LOG="$SCRIPT_DIR/server.log"

# 已在运行则跳过：cron 到点启动时若进程还在（手动启动过等），
# 直接退出避免端口冲突；盘中崩溃不在此处拉起（由用户接受）
if pgrep -f "simx-server --listen" > /dev/null 2>&1; then
    echo "[$(date '+%F %T')] simx-server 已在运行，跳过启动"
    exit 0
fi

nohup "$SERVER" --listen 0.0.0.0:9800 >> "$LOG" 2>&1 &
echo "[$(date '+%F %T')] simx-server 已启动（日志: $LOG）"
