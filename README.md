# Defing —— 分布式配置文档服务

单二进制分布式配置服务：**Rust 主服务（Raft 集群 + 内嵌 Admin UI + 三语言 SDK）**。
配置按 项目 → 分支（dev/test/prod + 自定义）→ 分组 → item 组织；
修改走"草稿 → 版本 → 发布 → 通知"闭环；结构项目级强一致，仅值按分支不同。

## 快速开始

### 单节点联调（--dev-single）

```bash
server/target/debug/dsh --dev-single --admin-password admin123 --http-addr 127.0.0.1:8384
# 管理面:  http://127.0.0.1:8384  （/admin 内嵌控制台，/metrics，/healthz）
# 数据面:  GET  /v1/projects/{p}/branches/{b}/snapshot  （SDK 拉配置，纯值+版本号）
#          SSE  /v1/projects/{p}/branches/{b}/watch      （订阅发布事件）
```

### 集群（3 节点）

```bash
dsh --node-id 1 --bootstrap --http-addr 127.0.0.1:8384 --raft-addr 127.0.0.1:8385 --data-dir ./n1 --admin-password admin123 --join-token demo --raft-token demo
dsh --node-id 2 --join http://127.0.0.1:8384 --http-addr 127.0.0.1:8386 --raft-addr 127.0.0.1:8387 --data-dir ./n2 --admin-password admin123 --join-token demo --raft-token demo
dsh --node-id 3 --join http://127.0.0.1:8384 --http-addr 127.0.0.1:8388 --raft-addr 127.0.0.1:8389 --data-dir ./n3 --admin-password admin123 --join-token demo --raft-token demo
# 提升为 voter：
# POST /api/v1/cluster/promote {"node_id": 2} / {"node_id": 3}（需管理员 Bearer）
# 重启自动恢复：同 data-dir 直接启动（无需 --bootstrap/--join）
```
> 安全（F3）：集群模式强制要求 `--join-token`（join 端点鉴权）与 `--raft-token`（raft RPC 鉴权），
> 集群内所有节点须传相同值；生产环境请使用强随机值。

### SDK（TS / Go / Python）

```ts
import { ConfigClient } from './sdk/ts/src/index.ts';
const c = new ConfigClient([{ grpc: '127.0.0.1:8383', http: 'http://127.0.0.1:8384' }]);
const snap = await c.get('my-app', 'dev');          // 读活动版本（gRPC 数据面）
c.watch('my-app', 'dev', (e) => console.log(e));    // 订阅发布事件（gRPC 流，断线 after_version 续传）
await c.listMembers();                              // 集群成员（端点池刷新）
```
Go：`sdk/go`（`configclient.NewGrpc(addr, token)` / `New(endpoints)` HTTP 降级）；
Python：`sdk/python`（`ConfigClient([{'grpc': ..., 'http': ...}])`）。
端点带 `grpc` 地址时优先走 gRPC 数据面（:8383），纯字符串端点自动降级 HTTP/SSE；
gRPC 契约测试：`bash scripts/sdk-grpc-contract-test.sh`（依赖：npm install、pip install grpcio、go mod tidy）。

## 核心能力

- **集群**：Raft 强一致、join/promote、leader 击杀容错、节点重启自动恢复
- **配置模型**：项目→分支→分组→item；结构强一致（仅值按分支）
- **发布闭环**：草稿 → 版本（不可变）→ 发布 → 通知；回滚；共享配置项与级联
- **安全**：secret 项 AES-256-GCM 信封加密（主密钥 env/文件）、单管理员会话、审计、CSP、
  join/raft 集群令牌（--join-token/--raft-token 集群模式强制）、HTTP 数据面令牌（--data-plane-token）
- **多格式**：YAML / TOML / JSON 渲染
- **可观测**：/healthz、/readyz、/metrics（Prometheus）、审计 API
- **Admin UI**：内嵌 /admin（项目/配置/watch）

## 构建与测试

```bash
cd server
source ../scripts/build-env.sh   # CARGO_HOME + CXXFLAGS（本机 /home 只读环境）
cargo build --workspace
cargo test --workspace           # 49 测试（core/storage/raft/crypto/render/jobs）
# 端到端：
bash ../scripts/dev-single-demo.sh   # 单节点全流程（含 watch）
bash ../scripts/cluster-demo.sh      # 3 进程集群 kill 容错
bash ../scripts/chaos-test.sh        # leader 击杀/重启追赶混沌
bash ../scripts/sdk-contract-test.sh # 三语言 SDK 契约
bash ../scripts/check-contracts.sh   # proto/openapi/schema lint
```

## 文档

- 需求演进：[docs/proposl.md](docs/proposl.md) → [docs/proposal-v4.md](docs/proposal-v4.md)
- 可行性分析：[docs/feasibility-report.md](docs/feasibility-report.md)
- 详细设计：[docs/design-v3.md](docs/design-v3.md)、[docs/design-modules/](docs/design-modules/)（15 份模块规格）
- 进度记录：[docs/progress.md](docs/progress.md)

## 许可

Apache-2.0
