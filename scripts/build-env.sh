#!/usr/bin/env bash
# 本机构建环境（重要！）：
# 1) /home 为只读挂载，cargo 无法写入 ~/.cargo → CARGO_HOME 必须指向工作区
# 2) GCC 16.1 与 RocksDB 9.0 不兼容（缺 uint64_t）→ CXXFLAGS 需注入 <cstdint>
export CARGO_HOME=/home/alex/Projects/Defing/.cargo
export CXXFLAGS="-include cstdint"
export PATH="/home/alex/.cargo/bin:$PATH"
