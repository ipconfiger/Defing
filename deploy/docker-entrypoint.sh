#!/bin/sh
# Defing 容器入口（O2：非 root 运行）
# 容器以 root 启动 → chown 数据目录（named volume 首挂为 root 所有）→ 降权为 defing 运行。
# 若以非 root 启动（外部显式指定 user），直接执行。
set -e

if [ "$(id -u)" = "0" ]; then
  chown -R defing:defing /data 2>/dev/null || true
  exec su-exec defing "$@"
fi

exec "$@"
