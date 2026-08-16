# 设计文档：G5 百分比放量 + 可观测 + 自动回滚钩子

> 状态：**已完成**（2025-08-16）｜ 基线：main `03b62db`（G4 已落地）
> 验收：metrics 6 项指标 e2e 断言全过；自动回滚集成测试（正/负例）通过；集群 3 节点同桶实测；workspace 31 套件全绿
> 前置：[gray-release.md](gray-release.md)（D17-D23）、[g3-dataplane.md](g3-dataplane.md)（D24-D28）、[g4-management.md](g4-management.md)（D29-D30）
> 一句话：**补灰度最后一公里——分桶算法文档化 + 灰度/HTTP 指标 + 可选自动回滚钩子 + 跨节点一致性验证。**

---

## 0. 现状分界线

| 能力 | 状态 |
|------|------|
| 百分比分桶纯函数 `fnv1a_hash(instance_id) % 100 < pct` | ✅ G2（rule_matches + T2 动态阈值测试） |
| 灰度命令/数据面/管理面/watch | ✅ G2-G4 |
| **算法文档化**（FNV-1a 常量、取模语义、跨节点确定性论证） | ⬜ |
| **灰度指标**（dsh_gray_active / gray 命令计数） | ⬜（metrics_text 无灰度项） |
| **HTTP 状态指标**（requests/5xx，自动回滚信号源） | ⬜ |
| **自动回滚钩子**（leader-only，对接错误率指标） | ⬜ |
| **跨节点一致性验证**（同一规则同一桶） | ⬜（纯函数确定性已论证，无集群测试） |

---

## 1. 决策（D31-D34）

### D31：灰度指标集（observability，Prometheus 文本）

在 `metrics_text` 追加（全部为节点可见、无隐私信息）：

```
dsh_gray_active            gauge  活跃灰度分支数（扫描各项目各分支 gray_seq>0）
dsh_gray_publish_total     counter 灰度发布累计（审计 action=gray_publish 计数）
dsh_gray_promote_total     counter 灰度转正累计（审计 action=gray_promote）
dsh_gray_abort_total       counter 灰度下量累计（审计 action=gray_abort）
dsh_http_requests_total    counter 进程内 HTTP 请求总数（API middleware 自增）
dsh_http_5xx_total         counter 进程内 HTTP 5xx 响应数（自动回滚信号源）
```

- 灰度计数取自**审计**（状态机数据，集群一致——任一节点指标一致）；
- HTTP 计数为**进程内 AtomicU64**（非状态机数据，节点本地语义正确——指标本来就是节点视图）；
- 计数器用模块级 static + 访问/重置函数（不扩 ApiState 构造签名，测试可重置）。

### D32：HTTP 计数中间件（API 层）

`build_router` 末尾加 `router.layer(axum::middleware::from_fn(count_http))`：
响应后 `status().is_server_error()` → `HTTP_5XX += 1`；每请求 `HTTP_REQUESTS += 1`。
`/metrics` 自身请求也被计入（Prometheus 抓取频率固定，比例语义稳定）。

### D33：自动回滚钩子（jobs，可选，默认禁用）

**信号源抽象**（业务错误率由外部系统决定，Defing 只做"框架 + 本地探针"）：

```rust
pub trait GrayHealthProbe: Send + Sync {
    /// 当前错误率（0.0-1.0）；None = 本轮无法获取（跳过）
    fn error_rate(&self) -> Option<f64>;
}
/// 内置探针：节点本地 /metrics 的 5xx 比例（对接 dsh_http_* 计数）
pub struct LocalHttp5xxProbe;
```

**调度**（异步任务，不塞进同步 Job trait——abort 走 raft 写路径需要 async）：

```
spawn_gray_auto_rollback(sm, raft, events_tx, audit, probe, threshold, interval, is_leader)
  └─ 循环（interval，默认 60s）：
     1. 非 leader → 跳过
     2. probe.error_rate() → None 跳过；≤ threshold 跳过
     3. 扫描活跃灰度分支（gray_seq>0）
     4. 逐个 dsh_raft::write_command(GrayAbort{comment:"auto-rollback: rate>thr"})
        （统一写路径：dev-single 直 apply+broadcast；集群 client_write 复制）
     5. 审计 action="gray_auto_abort" + warn 日志
```

- **阈值**：CLI `--gray-rollback-threshold <pct>`（默认 0 = 禁用）；`--gray-rollback-interval <秒>`（默认 60，测试可调小）；
- **防抖**：abort 后该分支 gray_seq=0，不再触发；审计可追溯；
- **确定性**：job 是后台任务（非 apply），读墙钟/网络无 D16 约束；abort 命令本身经状态机确定性 apply。

### D34：跨节点一致性（验证而非新实现）

百分比分桶确定性 = 纯函数（`fnv1a_hash`）+ 状态机数据（规则 Raft 复制）。新增集群测试：
同一 percentage 规则经 Raft 写入 3 节点，各节点对同一组 instance_id 的 `resolve_version`
结果逐位一致（同桶）。测试在 `dsh-raft/tests/cluster.rs` 追加。

---

## 2. 代码改动清单

| 文件 | 改动 | 验收 |
|------|------|------|
| `dsh-observability/src/lib.rs` | HTTP 计数 statics + accessor/reset；metrics_text 加 6 指标（gray 扫描 + 审计计数 + HTTP 计数）；测试 | 编译 + 单测 |
| `dsh-api/src/lib.rs` | `count_http` middleware + `build_router` layer | 编译 + 集成测试 |
| `dsh-jobs/src/lib.rs` | `GrayHealthProbe` + `LocalHttp5xxProbe` + `spawn_gray_auto_rollback`（raft 写路径 + 审计）+ 测试 | 编译 + 单测 |
| `dsh-jobs/Cargo.toml` | 加 dsh-raft / dsh-observability 依赖 | 编译 |
| `dsh-cli/src/main.rs` | `--gray-rollback-threshold` / `--gray-rollback-interval` 参数 + 装配 spawn | 编译 |
| `dsh-raft/tests/cluster.rs` | 跨节点百分比一致性测试 | 全绿 |
| `scripts/gray-obs-demo.sh` | e2e：/metrics 含 6 项灰度指标断言 + 自动回滚触发（阈值 1% + 制造 5xx） | 退出 0 |
| `docs/gray-release.md` | 补"百分比分桶算法"一节（FNV-1a 常量/取模语义/确定性论证） | 文档 |
| `docs/roadmap-p4.md` / `plan-gray-g5.md` / `g5-observability.md` | 状态标记 | 文档 |

**明确不做（本期）**：业务级错误率采集（Prometheus 远程查询对接——留给上层平台；钩子已抽象）；灰度分流请求计数（`dsh_gray_resolved_total`——需数据面埋点，G5 后可按需加）；自动回滚的"只回滚坏分支"（本期全分支；按分支分流留待远程指标对接时）。

---

## 3. 验收标准

- metrics 输出含 6 项新指标，gray 计数与审计一致（e2e 断言）；
- 自动回滚：dev-single 带阈值 1% + interval 2s，制造 5xx → 活跃灰度分支被自动 abort + 审计 gray_auto_abort；
- 集群测试：3 节点同一规则同桶（fnv1a 确定性验证）；
- `cargo test --workspace` + clippy/fmt + contract 全绿；CI 8/8。

## 4. 风险

| 风险 | 对策 |
|------|------|
| 自动回滚误伤（错误率阈值误判） | 默认禁用（threshold=0）；审计留痕；只有显式开启才生效 |
| HTTP 计数不精确（middleware 覆盖不全） | from_fn 包整个 router；metrics 自身计入，比例稳定 |
| gray 审计计数随保留策略裁剪 | 指标反映"当前审计窗口内"计数，语义注明；Prometheus counter 会回落属正常 |
| 集群测试耗时 | 复用现有 3 节点 bootstrap 框架，只加一个测试用例 |
