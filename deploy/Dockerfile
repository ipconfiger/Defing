# Defing 配置文档服务 —— 多阶段构建
FROM rust:1.97 AS builder
ENV CXXFLAGS="-include cstdint"
WORKDIR /app
COPY server/ ./server/
WORKDIR /app/server
RUN cargo build --release -p dsh-cli

FROM debian:bookworm-slim
COPY --from=builder /app/server/target/release/dsh /usr/local/bin/dsh
EXPOSE 8383 8384 8385
ENTRYPOINT ["dsh"]
