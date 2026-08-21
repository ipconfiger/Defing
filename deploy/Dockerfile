# Defing 配置文档服务 —— 多阶段构建
FROM rust:1.97 AS builder
ENV CXXFLAGS="-include cstdint"
# 部署版本标记：docker build --build-arg DEFING_GIT_COMMIT=$(git rev-parse --short HEAD)
ARG DEFING_GIT_COMMIT
# dsh-api/build.rs（tonic-prost-build）编译期需要系统 protoc（D-DKR 修复：此前缺失导致构建失败）
RUN apt-get update && apt-get install -y --no-install-recommends protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY server/ ./server/
WORKDIR /app/server
RUN cargo build --release -p dsh-cli

FROM debian:bookworm-slim
# O2：非 root 运行——su-exec 供 entrypoint 降权；创建 defing 用户与 /data
RUN apt-get update && apt-get install -y --no-install-recommends su-exec ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --uid 10001 --create-home defing \
    && mkdir -p /data && chown defing:defing /data
COPY --from=builder /app/server/target/release/defing /usr/local/bin/defing
COPY docker-entrypoint.sh /docker-entrypoint.sh
RUN chmod +x /docker-entrypoint.sh
# 容器默认 root 启动以 chown 挂载卷，entrypoint 内降权为 defing（最终进程非 root）
EXPOSE 8383 8384 8385
ENTRYPOINT ["/docker-entrypoint.sh"]
