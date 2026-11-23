#!/usr/bin/env bash
set -e
cd /home/emc/grid-hype

export HYPERLIQUID_PK="xxx"

nohup ./grid-hype --config config.toml < /dev/null > grid.log 2>&1 &
BOT_PID=$!
echo "grid-hype 已在后台启动 (PID: $BOT_PID)"
echo "查看实时运行日志: tail -f /home/emc/grid-hype/grid.log"
