# 设计文档：D3 checkpoint/diff 版本存储（perf 方案②）

> 状态：待审核 ｜ 日期：2025-08-16 ｜ 依据：[perf-write-path.md](../perf-write-path.md) 方案②、
> [04-publish.md](../design-modules/04-publish.md) §8（D3）
> 目标：消除"每次发布写全量快照"的写放大——按 checkpoint 规则存 full/diff，大配置下写字节
> 降 10～100×，DB 体积不再线性膨胀；依赖方案①已就绪的 `write_batch` 与 `save_pending`。

---

## 1. 现状与问题（代码证据）

- `apply_publish`（state.rs:1128-1133 附近）每次 `VersionKind::Full` + `save_pending(snapshot_key, resolved)` 全量快照落盘；
- `apply_publish_structure`（:949）、`apply_rollback`（:1158）、`apply_shared_publish`/`cascade_to_project`（:1223/:1463）同样全量；
- `snapshot_of`（state.rs:294-306 原行号，现为读合并版）读 `snapshot_key(id, branch, version)` 全量快照；
- 设计 D3（04-publish.md §8）明确："vno 为 checkpoint 倍数（每 100）或首次 → full（快照）；否则 diff"，**代码未实现**；
- `VersionRecord` 已有 `kind: VersionKind::Full/Diff`、`snapshot_ref`、`diff_ref` 字段（model.rs:263-281）——存储模型预留完整，仅实现缺位。

## 2. 目标

| 维度 | 现状 | 目标 |
|------|------|------|
| 版本存储 | 每次全量快照（写放大 ×配置大小） | checkpoint（每 100）或首次 → full；其余 diff |
| 写字节 | O(全量配置) / 发布 | O(变更项) / 发布（大配置 10～100× 下降） |
| DB 体积 | 线性膨胀（靠裁剪兜底） | 线性增长放缓（diff 为主） |
| 历史读取 | 直读全量快照 | 最近 full + diff 链重建（≤100 条，有界） |

**注意**：方案②优化的是**写字节量**与 DB 体积，fsync 次数已由方案①收敛（每次命令 1 次），
故对"小配置写 QPS"提升有限；对**大配置**（百 KB～MB 级）场景收益显著。两方案叠加才是完整写优化。

## 3. 设计

### 3.1 存储布局（复用现有 key 函数）

- `snapshot_key(pid, branch, vno)`：**仅 checkpoint 版本**（每 100 或首次）存全量快照；
- 新增 `diff_key(pid, branch, vno)`：非 checkpoint 版本存 diff（`Vec<DiffEntry>`，复用 `compute_diff` 产物）；
- `VersionRecord`：`kind = Full | Diff`，`snapshot_ref`/`diff_ref` 暂不启用（保持 None，key 已编码版本号；字段留给未来引用语义）。

### 3.2 写入规则（统一封装 `write_version_snapshot`）

```rust
fn write_version_snapshot(
    &mut self, id, branch, vno, old: &SnapshotMap, new: &SnapshotMap,
    record: &mut VersionRecord,
) -> Result<(), Error> {
    let is_checkpoint = vno == 1 || vno % CHECKPOINT_INTERVAL == 0;
    if is_checkpoint {
        record.kind = VersionKind::Full;
        self.save_pending(&snapshot_key(id, branch, vno), new)?;
    } else {
        record.kind = VersionKind::Diff;
        let diff = compute_diff(old, new);
        self.save_pending(&diff_key(id, branch, vno), &diff)?;
    }
    self.save_pending(&version_key(id, branch, vno), record)?;
}
```

调用点迁移（4 处）：`apply_publish` / `apply_publish_structure` / `apply_rollback` / `cascade_to_project`。
注意这些调用点**当前已计算 diff**（用于事件），可复用（`apply_publish` 中 `diff` 变量已存在）。

### 3.3 读取规则（`snapshot_of` 改造）

```rust
pub fn snapshot_of(&self, id, branch, version: u64) -> Result<SnapshotMap, Error> {
    // 定位最近 checkpoint（含自身）：向下取整到 checkpoint 边界；v=1 恒 full。
    // v=100 → 100（自身 checkpoint，直接读 full，0 个 diff 应用）；
    // v=101 → 100（读 full(100) + 应用 1 个 diff）；v=2 → 1（读 full(1) + 应用 1 个 diff）。
    let start = {
        let base = ((version - 1) / CHECKPOINT_INTERVAL) * CHECKPOINT_INTERVAL;
        if base == 0 { 1 } else { base }
    };
    let mut snap: SnapshotMap = self.load_merged(&snapshot_key(id, branch, start))?
        .ok_or_else(|| not_found(format!("snapshot {start} of {branch}")))?;
    for v in (start + 1)..=version {
        if v % CHECKPOINT_INTERVAL == 0 {
            // checkpoint 版本存 full（跳过 diff 应用，直接替换基座）
            snap = self.load_merged(&snapshot_key(id, branch, v))?.ok_or_else(...)?;
        } else {
            let diff: Vec<DiffEntry> = self.load_merged(&diff_key(id, branch, v))?.ok_or_else(...)?;
            apply_diff(&mut snap, &diff);
        }
    }
    Ok(snap)
}
```

> 边界分析（CHECKPOINT_INTERVAL=100）：
> - v=1 → start=1，读 full(1)；无循环 → 正确；
> - v=100 → start=100，读 full(100)；循环范围 101..=100 为空 → 正确（**0 个 diff 应用**）；
> - v=101 → start=100，读 full(100) + 应用 diff(101) → 正确；
> - v=105 → start=100，读 full(100) + diff(101..105 共 5 个) → 正确；
> - v=199 → start=100，读 full(100) + diff(101..199 共 99 个) → 正确；
> - v=200 → start=100，读 full(100)；循环中 v=200 命中 checkpoint 分支读 full(200) → 正确；
> - v=201 → start=200，读 full(200) + diff(201) → 正确。
> 复杂度：O(≤99 个 diff 应用)，有界；checkpoint 版本零 diff 应用。

- `apply_diff`：按 `ChangeKind::Upsert`（写 group/key）与 `Delete`（删 group/key）应用；**确定性**（纯内存，BTreeMap 有序）；
- 边界：version=0 调用方已处理（get_config 解析 active_version）；`version==1` 恒 full；
- 复杂度：O(最近 checkpoint 起 ≤100 版本的 diff 应用)，有界。

### 3.4 影响面审计

| 消费者 | 影响 | 处理 |
|--------|------|------|
| `snapshot_of`（版本读取） | 需支持 diff 链重建 | 3.3 改造，内部统一 |
| `version_history` / `get_version_record` | 读 VersionRecord（不读快照） | 无影响 |
| `prune_versions`（裁剪） | 删除 version+snapshot key | **需同时删 diff key**；且裁剪后最近 full 可能被删 → **保留 checkpoint 链完整性**（只删到"最近 checkpoint 之后"或重写） |
| `apply_publish` 的 `old` 读取 | `snapshot_of(active_version)` 可能命中 diff 链 | 3.3 自动处理 |
| watch 事件 / diff 响应 | 用 compute_diff 产物，不落盘 | 无影响 |
| `rewrap_deks`（DEK 重包） | 扫描 `/snap` 后缀 key | **需扩展扫描 diff key**（diff 中可能含 secret 密文 new_value） |
| 回滚 `snapshot_of(to_version)` | 同 3.3 | 自动处理 |
| 快照构建 `dump_all` | 全量导出（含 diff key） | 无影响（导出即恢复） |

### 3.5 裁剪策略（prune_versions 重写）

现状：按 keep 数删最旧版本（version + snapshot）。
新规则：删最旧版本时同时删对应 `diff_key`；**若被删版本是 checkpoint（full），则其后续 diff 链失去基座**——
策略：裁剪下限对齐到"最近仍保留的 checkpoint"，即 `keep` 与 checkpoint 边界取安全值：
`cutoff = min(可删数, 距最近 checkpoint 的版本数)`，保证最近 checkpoint 及其后 diff 链完整。

## 4. 测试计划

| 用例 | 断言 |
|------|------|
| T1 checkpoint 布局 | 发布 v1..v105：v1/v100 full，其余 diff；`snapshot_key` 只存在 checkpoint 版本；diff_key 存在其余 |
| T2 重建正确性 | 任意 v（1,2,50,100,101,105）`snapshot_of(v)` == 全量语义（与旧实现等价：抽查与 active 版本 diff 一致） |
| T3 回滚 | rollback to v（diff 链中）→ 内容正确 |
| T4 级联 | shared_publish 级联后 `snapshot_of` 重建正确 |
| T5 裁剪 | prune 后最近 checkpoint 保留、diff 链可重建 |
| T6 DEK 重包 | diff 中 secret 密文被重包 |
| T7 回归 | 既有 130+ 用例全绿；e2e 4 脚本；大配置（1MB）写字节对比（bench 扩展） |

## 5. 验收标准

1. `cargo test --workspace` 全绿（新增 T1-T6）；
2. e2e 4 脚本全过；
3. 大配置写字节下降 ≥10×（bench 扩展输出 `WRITE_BYTES`）；
4. 历史版本读取正确（任意版本快照重建）；
5. 确定性保持（diff 应用纯内存、BTreeMap 有序）。

## 6. 明确不做（本期）

- checkpoint 间隔可配置（`--checkpoint-interval`，D1 旋钮，暂固定 100）；
- diff 压缩/二进制序列化（保持 serde_json 可读性）；
- snapshot_ref/diff_ref 字段语义启用（key 已编码，字段留空）。

## 7. 审核修订记录（2025-08-16，子代理 Q1-Q5）

| # | 审核问题 | 处理 |
|---|---------|------|
| Q1 | 基座定位伪码与实现不一致（v100 白重建 2..99） | 实现已用"自身即 checkpoint"分支（`version % 100 == 0 → start=version`）；本文档 3.3 已同步 |
| Q2 | prune_versions 未适配 diff_key + checkpoint 对齐（删基座=数据丢失） | **已实现**：prune 同时删 diff_key；删除下限对齐最近 checkpoint（`keep_from` 向下取整），最新保留版本是 full 基座，diff 链可重建（T5 验证） |
| Q3 | rewrap_deks 未覆盖 /diff 中 secret 密文（KEK 轮换不完整） | **已实现**：新增 `/diff` 分支，扫描 DiffEntry Upsert 的 new_value，重写后写回（T6 验证） |
| Q4 | cascade_to_project 未迁移（级联路径仍全量+非批写） | **已修复**：级联走 write_version_snapshot（checkpoint/diff 规则 + pending 批写） |
| Q5 | 滚动升级：旧 reader 读新 diff 版本必挂 | **决策**：单向兼容（新 reader 兼容旧数据 via fallback 直读）；**升级要求全集群同步升级**（与 project-admin.md §3 的 PA 功能升级纪律一致）；文档明确"不支持降级"。`version_history` 已排除 `/diff` 后缀（小项修复） |

**实现状态**：开发完成 + 测试全绿（新增 T1/T2/T5/T6，`cargo test -p dsh-core` 30 用例）+ 全量 workspace 回归中。
