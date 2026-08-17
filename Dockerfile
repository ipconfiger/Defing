# Defing 配置文档服务 —— 直接使用已构建的 release 二进制
# 构建前请先执行：cargo build --release -p dsh-cli（产物位于 server/target/release/dsh）
FROM ubuntu:24.04
COPY server/target/release/dsh /usr/local/bin/dsh
