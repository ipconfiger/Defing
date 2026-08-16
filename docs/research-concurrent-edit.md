# 研究：多人并发编辑同一配置项的并发控制分析

> 日期：2025-08-16 ｜ 依据：代码级审计 + 并发语义推演
> 结论先行：**当前值草稿编辑是 last-write-wins（后写覆盖，无冲突检测），存在"静默丢修改"风险**；
> 结构草稿已有乐观锁（base_version 校验）。建议为值草稿引入**草稿版本号（乐观锁）**，对齐结构草稿的既有机制。

---

## 1. 现状（代码证据）

| 编辑路径 | 并发控制 | 行为 |
|----------|----------|------|
| **结构草稿** `StructureDraftSet` | ✅ **有乐观锁** | `base_version != structure.version → 409 Conflict`（state.rs:1102-1105） |
| **值草稿** `DraftUpdate` | ❌ **无** | 直接合并 updates/deletes 到 `value_draft`（state.rs:1181-1240），**后写覆盖前写** |
| **发布** `Publish` | ⚠️ 仅幂等 | `request_id` 去重（state.rs:1226），无版本冲突检测 |
| 分支草稿 | 单份共享 | `BranchState.value_draft` 是分支级单草稿（model.rs:234-242） |

**"多人同时编辑同一项"的当前行为**（典型竞态）：

```
A 读快照（item X = "v1"）   B 读快照（item X = "v1"）
A 提交草稿（X = "A"）       → value_draft[X] = A
                           B 提交草稿（X = "B"）→ value_draft[X] = B（覆盖 A！）
A 发布 → 发布的是 B 的值，A 的修改静默丢失
```

**根因**：值草稿是**分支级共享单份**，DraftUpdate 是**增量合并**（只更新列出的 item），
无"基于哪个版本编辑"的锚定 → 无法检测"你的修改基于过期值"。

## 2. 为什么结构草稿没这个问题（对照）

- 结构草稿提交携带 `base_version`（当前已发布结构版本）；
- apply 校验 `base_version == structure.version`，不匹配 → 409；
- 效果：A 改了结构草稿但未发布，B 提交时 base_version 仍是旧值 → B 409，必须刷新看 A 的草稿再改。

值草稿缺失同样的锚定机制。

## 3. 方案：值草稿乐观锁（对齐结构草稿）

### 3.1 核心设计

```rust
// BranchState 增加草稿版本戳（multisession 后多人并发的自然扩展）
BranchState {
    active_version: u64,          // 已发布版本（现有）
    draft_rev: u64,               // 新增：草稿修订号（每次 DraftUpdate 单调递增）
    value_draft: ...,             // 现有
}
```

```rust
// DraftUpdate 命令加字段（Raft wire 兼容：#[serde(default)]，None = 旧语义不校验）
DraftUpdate {
    project, branch, updates, deletes, operator, ts,
    #[serde(default)] expected_draft_rev: Option<u64>,  // Some(rev) 严格校验；None = 不校验（旧客户端/旧日志）
}
```

> **为何用 `Option<u64>` 而非 `u64`**：实测发现 `u64` 的"0 = 不校验"与"首次编辑 rev=0"冲突——
> 新客户端首次保存带 `0` 会被误判为"不校验"而绕过检测。`Option` 下新客户端显式传 `Some(0)` 也参与校验，
> 旧客户端缺省 `None` 才不校验（兼容 last-write-wins）。

**apply 校验**：
```rust
if let Some(exp) = cmd.expected_draft_rev {
    if exp != st.draft_rev {
        return Err(Conflict("草稿已被他人修改（draft_rev {当前} != expected {exp}），请刷新后重试"));
    }
}
st.draft_rev += 1;   // 无论是否带 expected，提交都推进修订号
```

### 3.2 API 层

- `GET /branches/{b}` 响应增加 `draft_rev`；
- `PUT /branches/{b}/draft` 请求增加可选 `expected_draft_rev`；
- 冲突 → 409 + 错误消息含当前 `draft_rev`（客户端刷新）。

### 3.3 语义

| 场景 | 行为 |
|------|------|
| A、B 同时编辑同一项，B 后提交 | B 若带旧 expected_rev → **409**（A 先提交推进了 rev），B 刷新后看到 A 的值再改 |
| A 改 X、B 改 Y（不同项） | 若都带 expected_rev：A 提交 rev+1，B 提交时 expected 旧 → **409 误报**（B 被迫刷新）——**粒度粗** |
| 无人并发 | 带最新 expected 总是匹配，零影响 |
| 旧客户端（无 expected=None） | 不校验，last-write-wins（兼容） |

**粒度权衡**：草稿级 rev 是"粗粒度"（不同 item 的并发编辑也冲突）。
可选细化：
- **item 级 rev**：`value_draft[X].rev`，只校验同 item 的并发——精确但复杂度高（每 item 记 rev）；
- **折中**：分支级 rev + 客户端"编辑前读、提交带 rev"，冲突时提示刷新（对配置中心场景通常足够——配置编辑频率低、冲突可接受人工协调）。

## 4. 影响面

| 项 | 改动 |
|----|------|
| model.rs | BranchState 加 draft_rev（serde default 0，旧数据兼容） |
| command.rs | DraftUpdate 加 expected_draft_rev（serde default 0） |
| state.rs | apply_draft_update 校验 + 推进 rev |
| dsh-api | draft GET/PUT 增加字段 |
| Admin UI | 草稿编辑器带 rev + 409 提示刷新 |
| 测试 | 并发编辑冲突用例 |

## 5. 与写性能的关系

- **不影响写性能**：校验是状态机内一次整数比较（apply 内），无额外 IO/fsync；
- Raft 串行 apply 保证"校验-推进"原子（同一 apply 内 read-modify-write，无 TOCTOU）。

## 6. 明确不做（本期）

- 悲观锁（编辑期间锁分支/项目）：复杂度高，配置场景不必要；
- item 级 rev：粒度优化，可后续按需叠加；
- 自动合并（如 git 式 merge）：超出配置中心职责。

## 7. 结论与建议

1. **现状风险真实**：多人同时编辑同一项 → 后写覆盖、静默丢修改（无冲突提示）；
2. **建议实施草稿乐观锁**（分支级 draft_rev，1-2 天）：对齐结构草稿既有机制，低风险高价值；
3. 冲突粒度粗（不同 item 也冲突）可接受——配置编辑低频，409+刷新人工协调是行业惯例（Apollo 同样依赖发布确认流程）；
4. 不改变写性能（纯状态机内比较），不破坏 Raft 确定性。

## 8. 实施状态（2025-08-16）：已完成

**核心**：
- `BranchState.draft_rev`（serde default 0 兼容旧数据）；`DraftUpdate.expected_draft_rev: Option<u64>`
  （`Some(rev)` 严格校验、`None` 不校验——消除"0 双义性"：新客户端首次传 `Some(0)` 也校验）；
- `apply_draft_update`：`Some(exp) != draft_rev` → Conflict 409；提交后 `draft_rev += 1`；
- **API**：`GET /branches/{b}` 返回 `draft_rev`；`PUT /draft` 接受 `expected_draft_rev`（可选）；
- **Admin UI**：保存带 `expected_draft_rev`，409 → 提示"草稿已被他人修改"并**自动拉取最新草稿**供继续修改。

**实测**（两人并发编辑）：
```
A 保存(rev=0) → 200
B 保存(rev=0 过期) → 409 "草稿已被他人修改（draft_rev 1 != expected 0），请刷新后重试"
B 拉取最新 → 看到 A 的值 + rev=1
B 保存(rev=1) → 200
旧客户端（无 expected）→ 200（兼容，last-write-wins）
```

**验收**：cargo test 31 套件全绿（新增乐观锁冲突/兼容用例）、clippy/fmt 零告警、
e2e（dev-single/api-surface）全过、写性能不变（纯状态机内整数比较）、Raft wire 兼容
（旧日志 None 不校验）。
