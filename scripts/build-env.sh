#!/usr/bin/env bash
# 构建环境（O1 修正：不再硬编码 CI 路径，本机/CI 均可 source）：
# 1) 部分 CI 环境中 /home 为只读挂载，cargo 无法写入 ~/.cargo →
#    检测到该布局时把 CARGO_HOME 指向工作区内的本地目录；
# 2) 普通机器（~/.cargo 可写）保持默认，不覆盖用户环境；
# 3) 存储层已迁移纯 Rust redb，无需 CXXFLAGS 注入（RocksDB 时代遗留，仅供参考）。
set -u

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WS_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# CI 布局：/home 只读且存在预置工具链目录 → 使用该布局（保持旧行为）
if [ -d /home/alex/Projects/Defing/.cargo ]; then
  export CARGO_HOME=/home/alex/Projects/Defing/.cargo
  export PATH="/home/alex/.cargo/bin:$PATH"
elif [ -e "${CARGO_HOME:-$HOME/.cargo}" ] && [ ! -w "${CARGO_HOME:-$HOME/.cargo}" ] 2>/dev/null; then
  # ~/.cargo 已存在但不可写（只读挂载等）→ 退回工作区本地目录
  export CARGO_HOME="$WS_ROOT/.cargo-local"
  export PATH="$WS_ROOT/.cargo-local/bin:$PATH"
fi
