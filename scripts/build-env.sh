#!/usr/bin/env bash
# 本机构建环境：
# 1) /home 为只读挂载，cargo 无法写入 ~/.cargo → CARGO_HOME 必须指向工作区
# 2) 存储层已迁移纯 Rust redb，无需 CXXFLAGS 注入（ RocksDB 时代的 GCC 16.1
#    与 RocksDB 9.0 uint64_t 兼容问题已随迁移消失；保留说明供历史参考）
export CARGO_HOME=/home/alex/Projects/Defing/.cargo
export PATH="/home/alex/.cargo/bin:$PATH"
