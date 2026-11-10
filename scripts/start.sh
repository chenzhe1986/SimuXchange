#!/usr/bin/env bash

# 启动 simx-server：脚本所在目录即数据目录（gateways.json / packets/ / server.log 都在这里）。
# 用法：./start.sh [musl|glibc]，默认 musl。
#   musl  = 静态链接，不依赖系统库，任何 x86_64 Linux 都能跑（不需要 Oracle 查询时用它）
#   glibc = 需要「同步持仓数据」（Oracle）时用，要求系统 glibc >= 2.17
# 已在运行（任意版本）则跳过，cron 到点重复触发不冲突。
SERVER_FLAVOR="${1:-musl}"
case "$SERVER_FLAVOR" in
    musl|glibc) SERVER_BIN="./simx-server-$SERVER_FLAVOR" ;;
    *)
        echo "用法: $0 [musl|glibc]" >&2
        exit 2
        ;;
esac

# 「同步持仓数据」的 Oracle Instant Client（按服务器实际路径修改）：
# 仅 glibc 版能加载它；musl 版不支持 Oracle 查询。目录不存在时自动跳过。
ORACLE_CLIENT_DIR="${ORACLE_CLIENT_DIR:-/opt/zzb/oracle_client_install/oracle_sqlplus}"
if [ -d "$ORACLE_CLIENT_DIR" ]; then
    export LD_LIBRARY_PATH="$ORACLE_CLIENT_DIR${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
fi

cd "$(dirname "$0")" || exit 1

# 已在运行则跳过（兼容旧版无后缀的 simx-server、musl、glibc 三个名字）
if pgrep -f "simx-server.*--listen" >/dev/null; then
    exit 0
fi

if [ ! -x "$SERVER_BIN" ]; then
    echo "找不到可执行文件 $SERVER_BIN（请确认已 chmod +x）" >&2
    exit 1
fi

nohup "$SERVER_BIN" --listen 0.0.0.0:9800 --auto-start >> server.log 2>&1 &
