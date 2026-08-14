# 未完成工作清单与下一轮优化计划

> 生成时间：M4 完成后核查 ｜ 目的：记录与 design 规格的差异项，作为下一轮优化输入
> 依据：docs/progress.md（M0–M4 验收记录）+ 工作区核查
> 状态：当前无进行中任务；下列为明确记录的未完成/降级项

---

## 1. 当前状态快照

| 里程碑 | 状态 | 验证 |
|--------|------|------|
| M0 契约+脚手架 | ✅ | 契约三方 lint + 29 tests |
| M1 单节点+集群 | ✅ | 49 tests + dev-single/cluster e2e |
| M2 发布引擎+加密+渲染+任务 | ✅ | core/crypto/render/jobs tests + e2e |
| M2.5 watch+可观测+会话+UI | ✅ | M2.5 e2e |
| M3 三语言 SDK | ✅ | 契约对拍全过 |
| M4 混沌+加固+发布 | ✅ | chaos + 6 脚本 + 49 tests |
| M5 会话/审计/快照/集群watch | ✅（R1–R4） | 59 tests + 5 e2e 脚本（含跨节点 409 断言） |
| M6 模块化归位 + gRPC 数据面 | ✅（B1+A1） | 65 tests + gRPC 集成测试 + 5 e2e 脚本 |
| M7 组级引用 + 密钥轮换 | ✅（B3+B6） | 74 tests + 轮换 HTTP e2e + 5 e2e 脚本 |

- cargo test --workspace：59 passed / 0 failed；clippy -D warnings 零警告；fmt 干净
- 读路径基准冒烟：6279 QPS（200 并发，4 万请求全成功）
- 无运行中后台任务、无残留 dsh 进程
- M5 已闭环：B2 会话落 Raft（集群级单会话+leader 转发）、B4 审计持久化、B5 快照持久化、B7 集群 watch 自动化测试（见 docs/progress.md M5）
- M6 已闭环：B1 模块化归位（dsh-api/publish/observability/watch 全填充，cli 仅组装）、A1 gRPC 数据面（tonic ConfigService 挂 :8383，见 docs/progress.md M6）
- M7 已闭环：B3 组级引用（整组绑定共享组 + 物化/级联/索引）、B6 密钥轮换（KeyRing + rotate API + RewrapDeks 任务 + CRY-002，见 docs/progress.md M7）

## 2. 未完成/降级项清单（下一轮候选）

### A. 结构性未完成

| # | 项 | 现状 | 差异点 |
|---|-----|------|--------|
| A1 | gRPC 数据面 | ~~未实现~~ → **已闭环（M6）**：tonic ConfigService（GetConfig/GetItem/Watch/ListMembers）挂 :8383，含鉴权拦截器与集成测试；三语言 SDK 暂仍走 HTTP/SSE（可后续增 gRPC 客户端） | proto/config.v1.proto → dsh-api/build.rs + grpc.rs |
| A2 | CI 全量流水线 | .github/workflows/ci.yml 仅 lint/unit/contract 三阶段 | 集成/raft/sdk/e2e/bench/release 阶段未接入；本机无法实跑 Actions |
| A3 | 正式基准 | 仅冒烟：读 6279 QPS | 设计目标 写 QPS ≥10k、watch ≥10k、发布→SDK ≤1s、内存 ≤128MB、二进制 ≤50MB（release）未校准；写路径为单写者串行 apply |
| A4 | SBOM | 未生成 | M4 发布清单软项（Docker/compose/README 已就绪） |

### B. 设计降级项（本次核查确认）

| # | 项 | 现状 | 期望（design） | 相关代码 |
|---|-----|------|---------------|----------|
| B1 | 四个 crate 仍为 M0 占位 | ~~placeholder stub~~ → **已闭环（M6）**：dsh-api（HTTP+Admin UI+错误映射）、dsh-publish（PublishService+加密）、dsh-observability（AuditLog+指标）、dsh-watch（WatchHub+SSE）全部填充；cli 仅组装（1469→~300 行） | 按 design-modules/05/04/10/06 模块化拆分 | server/crates/dsh-{api,publish,observability,watch}/ |
| B2 | 单管理员会话非集群级 | ~~每节点内存 Mutex~~ → **已闭环（M5）**：会话入状态机 `sess/admin`（token_hash），跨节点唯一，login 非 leader 自动转发 | 会话状态存 Raft 状态机、集群范围唯一（I7） | dsh-core/state.rs + dsh-cli/main.rs |
| B3 | 组级共享引用不支持 | ~~item_key=None 被拒~~ → **已闭环（M7）**：整组绑定共享组（bind 校验/物化/级联 idx/refg/解绑） | 支持组级引用（design-v2 R6/AC6.2） | server/crates/dsh-core/src/state.rs |
| B4 | 审计未持久化 | ~~内存环形缓冲~~ → **已闭环（M5）**：`Command::AuditAppend` 落库 `audit/{seq}`，查询带 action/since/limit，`AuditRetention` 保留策略 | 落库 audit/{seq}，可查询 | dsh-core/state.rs + dsh-cli/main.rs + dsh-jobs |
| B5 | 快照不跨重启持久化 | ~~内存态~~ → **已闭环（M5）**：build/install 落盘 snapshots 列族，重启读盘恢复 | 快照文件化/checkpoint 持久化 | server/crates/dsh-raft/src/store.rs |
| B6 | 主密钥轮换未实现 | ~~无轮换~~ → **已闭环（M7）**：KeyRing + rotate API + --rotate-master-key CLI + RewrapDeks 任务 + 环文件持久化（CRY-002） | 轮换流程（design-v2 7.5） | dsh-crypto + dsh-api + dsh-jobs + dsh-cli |
| B7 | 集群 watch 无自动化测试 | ~~仅手工验证~~ → **已闭环（M5）**：cluster.rs 新增 follower 订阅事件断言 | 自动化契约用例 | server/crates/dsh-raft/tests/cluster.rs |

## 3. 建议优先级（下一轮候选顺序）

| 优先级 | 项 | 理由 | 预计工作量 |
|--------|-----|------|-----------|
| ~~P0~~ | ~~B2 会话落 Raft~~ | ✅ M5 完成（含 leader 转发 + 错误传播修复） | — |
| ~~P0~~ | ~~B4 审计持久化~~ | ✅ M5 完成（落库 + 查询参数 + 保留策略） | — |
| ~~P1~~ | ~~A1 gRPC 数据面~~ | ✅ M6 完成（含鉴权 + 集成测试） | — |
| ~~P1~~ | ~~B1 模块化归位~~ | ✅ M6 完成（4 crate 全部填充） | — |
| ~~P2~~ | ~~B3 组级引用~~ | ✅ M7 完成（整组绑定/物化/级联/解绑） | — |
| ~~P2~~ | ~~B6 密钥轮换~~ | ✅ M7 完成（KeyRing/rotate API/重包任务/CRY-002） | — |
| ~~P2~~ | ~~B5 快照持久化~~ | ✅ M5 完成 | — |
| ~~P3~~ | ~~B7 集群 watch 测试~~ | ✅ M5 完成 | — |
| P3 | A2 CI 全量 / A3 正式基准 / A4 SBOM | 需要对应 CI/压测环境 | 视环境 |

## 4. 下一轮优化入口（每项照例：实现 → 测试 → 验证）

1. 会话落 Raft：新增 Command::SessionLogin/Logout/Heartbeat（core）+ state machine 会话记录；login 走 raft.client_write；auth_middleware 校验状态机会话（跨节点唯一）。
   - 注意：login 是"无会话"状态下的调用，需豁免鉴权（同 /api/v1/cluster/join）。
   - 验证：第二个登录 409（跨节点）；kill leader 后会话保持（日志复制）。
2. 审计持久化：AuditSink 落库 audit/{seq}（core 键布局已有 K_AUDIT）；GET /api/v1/audit 改读库；保留策略（可配条数/天数）。
3. gRPC 数据面：tonic 实现 proto ConfigService（GetConfig/GetItem/Watch/ListMembers）；dsh-cli 挂 gRPC server；三 SDK 增 gRPC 客户端（或保留 HTTP 作为降级通道）。
4. 模块化归位：把 cli 中的 handler/会话/指标迁入 dsh-api，发布逻辑迁入 dsh-publish，审计/指标迁入 dsh-observability；cli 仅组装。
5. 组级引用：RefBinding item_key=None 语义（整组共享）实现 + 级联。
6. 密钥轮换：dsh admin rotate-master-key + DEK 重包后台任务（jobs）。
7. 快照持久化：build_snapshot 落盘 + get_current_snapshot 读取；安装后持久化。

## 5. 环境备忘（下一轮构建必须）

- /home 只读挂载 → 构建前 source scripts/build-env.sh（CARGO_HOME=/home/alex/Projects/Defing/.cargo、CXXFLAGS="-include cstdint"）
- Go 构建缓存 → GOCACHE=/tmp/dsh-gocache
- 端到端脚本：scripts/{dev-single-demo,cluster-demo,chaos-test,sdk-contract-test,check-contracts}.sh
- 端口约定：dev-single 8384；cluster 演示 860x/870x；混沌 861x/871x
- 契约文件：proto/config.v1.proto、api/openapi.v1.yaml、schema/storage.v1.schema.json
