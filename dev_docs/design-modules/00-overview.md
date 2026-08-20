# 模块 00 —— 模块总览与开发约定

> 依据：dev_docs/design-v2.md（§1.4 模块清单）、dev_docs/design-v3.md（§5/§8）
> 版本：v1.0 ｜ 状态：开发就绪（评审通过）

## 1. 本文件目的
定义所有模块共享的开发约定、工作区布局、接口矩阵与开发顺序，作为逐模块规格的入口。

## 2. 工作区布局（Cargo workspace，仓库根 server/）

```
server/
  Cargo.toml            # workspace
  crates/
    dsh-core/           # 数据模型 + 状态机 + 校验（仅依赖 serde）
    dsh-storage/        # RocksDB 封装（Storage trait）
    dsh-raft/           # openraft 集成（依赖 core, storage）
    dsh-publish/        # 发布引擎（依赖 core）
    dsh-crypto/         # 加密层（依赖 core 的类型）
    dsh-render/         # 多格式渲染（依赖 core）
    dsh-watch/          # 订阅/扇出（依赖 core）
    dsh-api/            # gRPC + HTTP 服务（依赖全部应用层）
    dsh-observability/  # 指标/日志/审计/健康
    dsh-jobs/           # 后台任务
    dsh-cli/            # 二进制 crate：dsh（组装 + 启动）
    dsh-testkit/        # 测试工具：mock raft、契约服务、golden 数据
  proto/                # config.v1.proto（仓库根，已存在）
```

## 3. 共享开发约定

| 项 | 约定 |
|----|------|
| 错误类型 | `dsh-core::Error`：`{ kind: ErrorKind, message, detail, leader_hint, request_id }`；ErrorKind 与 design-v3 §7 错误码一一对应（含 Internal/Storage/Raft/Crypto 内部变体） |
| 错误映射 | 各层只产生 ErrorKind；gRPC/HTTP 层映射为对外错误码（模块 05） |
| 日志 | `tracing`；结构化字段：request_id、operator、action、project、branch、version；`DSH_LOG` 控制级别，`DSH_LOG_JSON=1` 输出 JSON |
| 审计 | 通过 `observability::AuditSink` trait 写审计（模块 10），业务代码只调用 `audit(action, target, detail)` |
| 配置 | `clap`（CLI）+ `envy`（DSH_* 环境变量）+ 可选 YAML 文件；优先级 env > file > flag（design-v2 §2.6） |
| 测试 | `nextest`；单元就近；契约/集成用 `dsh-testkit` |
| 代码质量 | rustfmt、clippy `-D warnings`、cargo deny（许可+依赖）、RustSec |
| 确定性 | 状态机 apply 禁止墙钟/随机/IO/日志——副作用走返回事件（design-v2 D16） |
| 并发 | 状态机只在 Raft apply 线程写；读经 ReadIndex 后本地读；事件/审计走 channel 异步落库 |

## 4. 模块接口矩阵（关键类型由哪个 crate 提供）

| 类型 | crate | 说明 |
|------|-------|------|
| `Error / ErrorKind` | dsh-core | 统一错误 |
| `Model`（Project/Branch/Structure/Version…） | dsh-core | 数据模型（对应 storage schema） |
| `Validator` | dsh-core | item 校验 + 引用解析 + 循环检测 |
| `Storage` trait | dsh-storage | KV 读写（前缀 + 批量 + 列族） |
| `RaftTypeConfig / RaftStorage / RaftNetwork` | dsh-raft | openraft 集成 |
| `PublishEngine` | dsh-publish | 发布/结构发布/回滚/共享级联/幂等 |
| `Cipher / KeyProvider` | dsh-crypto | 加密/解密/轮换 |
| `Renderer` | dsh-render | IR → YAML/TOML/JSON |
| `SubscriptionTable` | dsh-watch | 订阅/扇出/重放 |
| `ApiServer` | dsh-api | gRPC + HTTP 启动 |
| `Metrics / AuditSink / Health` | dsh-observability | 可观测性 |
| `JobScheduler` | dsh-jobs | 后台任务 |
| `dsh`（二进制） | dsh-cli | 组装、启动、CLI 子命令 |

## 5. 开发顺序（关键路径）

```
dsh-core ──▶ dsh-storage ──▶ dsh-raft ──▶ dsh-publish ──▶ dsh-api ──┐
    │                                                              │
    ├────▶ dsh-crypto ────────────────────────────▶（并入 api）───┤
    ├────▶ dsh-render ────────────────────────────────────────────┤
    ├────▶ dsh-watch ─────────────────────────────────────────────┤
    ├────▶ dsh-observability ─────────────────────────────────────┤
    └────▶ dsh-jobs ──────────────────────────────────────────────┤
                                                                   ▼
                                                    dsh-cli（组装） + dsh-testkit（贯穿）
```
- 并行轨道：A 轨道（core→storage→raft）、B 轨道（crypto、render）、C 轨道（testkit/契约 golden）。
- 集成点：dsh-api 起服务 → dsh-cli 单节点先跑通 CRUD/发布 → 再上集群。
- **里程碑内联模式**：`--dev-single`（单节点无 Raft 直写状态机）用于快速联调，生产走 Raft。

## 6. 模块规格索引

| 文件 | 模块 |
|------|------|
| 01-core.md | 数据模型与状态机核心 |
| 02-storage.md | RocksDB 封装 |
| 03-raft-node.md | openraft 集成 |
| 04-publish.md | 发布引擎 |
| 05-api.md | gRPC + HTTP |
| 06-watch.md | 订阅与推送 |
| 07-crypto.md | 加密层 |
| 08-render.md | 多格式渲染 |
| 09-admin-ui.md | 管理控制台 |
| 10-observability.md | 可观测性 |
| 11-jobs.md | 后台任务 |
| 12-sdk.md | 三语言 SDK |
| 13-testing-ci.md | 测试与 CI |
| 14-dev-plan.md | 开发计划（WBS/验收） |
