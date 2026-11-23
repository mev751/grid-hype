#!/usr/bin/env bash
set -e
cd /home/emc/grid-hype

export HYPERLIQUID_PK="0x9d851989a00a765ac5c77a2a04db364e0ec3cbfdf165863e47fd2eca87e9d1db"

nohup ./grid-hype --config config.toml < /dev/null > grid.log 2>&1 &
BOT_PID=$!
echo "grid-hype 已在后台启动 (PID: $BOT_PID)"
echo "查看实时运行日志: tail -f /home/emc/grid-hype/grid.log"
