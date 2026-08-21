# 开发计划：分支级共享引用（shared-ref branch-scope）

日期: 2026-08-21
上游: dev_docs/design/shared-ref-branch-scope.md（v1 待审核；本计划随设计修订同步更新）
执行路线: 单工作区逐 slice 实现，每 slice 后跑对应测试；全部完成后整体验证
注意: `server/crates/dsh-api/src/lib.rs` 含未提交的 K3s leader 写转发改动（forward_leader_writes + 测试 leader_write_forward.rs）——所有对该文件的编辑均为外科手术式，不触碰该改动。

## TDD Route

```text
TDD Route:
- Mode: off
- Decision: skipped
- Strict authority: not applicable（用户未要求 strict TDD）
- Test posture: post-change regression —— 既有测试随实现同步更新，新行为（分支差异化绑定/未绑定阻断/绑定持久化/级联按分支/删除保护）配对新测试
- Reason: 用户流程为「分析→设计→计划→实现」，无 strict TDD 指令；仓库既有测试基线即回归网
- Verification: cargo test --workspace（基线全绿）+ scripts/check-contracts.sh + scripts/api-surface-test.sh + 手动 UI 清单
```

## 0. 目标与基线

- 目标：结构只声明「引用共享」（ItemDef.shared: bool + 真实 type），分支在草稿页按下拉选择引用的共享项（BranchState.shared_bindings），取代结构级 ItemDef.shared_ref。
- 兼容边界：dev 阶段 breaking change（删 ItemDef.shared_ref 字段）；**不做数据迁移**（serde 忽略旧字段，旧结构反序列化为全本地项）。
- 基线命令：`cd server && source ../scripts/build-env.sh && cargo test --workspace`（全绿，已验证）。
- 二进制：`server/target/debug/defing`；e2e：`bash scripts/api-surface-test.sh`（自起 dev-single）。

## 1. 文件地图

| 文件 | 动作 | 内容 |
| --- | --- | --- |
| server/crates/dsh-core/src/model.rs | 改 | ItemDef.shared_ref→shared: bool（+is_false 辅助）；BranchState +shared_bindings +bindings_dirty |
| server/crates/dsh-core/src/command.rs | 改 | DraftUpdate +shared_bindings: Vec<SharedBinding>（新 struct） |
| server/crates/dsh-core/src/validator.rs | 改 | 删 shared_ref 检查；validate_publish 跳过 shared 项 |
| server/crates/dsh-core/src/state.rs | 改 | 见 §3-§4 任务 |
| server/crates/dsh-core/src/keys.rs | 改 | 注释同步（引用选择在分支状态） |
| server/crates/dsh-core/tests/state_machine.rs | 改 | 既有 shared_ref 测试改写 + 新增（任务 4.7） |
| server/crates/dsh-core/tests/model_serde.rs / project_admin.rs | 改 | `shared_ref: None` → `shared: false` |
| server/crates/dsh-testkit/src/lib.rs | 改 | 构造器 `shared_ref: None` → `shared: false`（3 处） |
| server/crates/dsh-jobs/src/lib.rs | 改 | 同上（4 处） |
| server/crates/dsh-publish/src/lib.rs | 改 | 同上（2 处）+ update_draft 透传 bindings |
| server/crates/dsh-raft/tests/cluster.rs | 改 | 构造器 `shared_ref: None` → `shared: false` |
| server/crates/dsh-api/src/lib.rs | 改 | DraftUpdateReq +shared_bindings；branch_detail shared_refs 含未绑定；refs 加 branch |
| server/crates/dsh-api/tests/http_project_admin.rs | 改 | shared_refs 形状；DraftUpdate bindings；refs 含 branch |
| server/crates/dsh-api/admin/app.js | 改 | 结构编辑器勾选；草稿页绑定下拉；共享库 tooltip |
| server/crates/dsh-api/admin/index.html / styles.css | 改 | 如涉及结构/样式 |
| api/openapi.v1.yaml | 改 | ItemDef/Branch/DraftUpdate/SharedItem.refs |
| schema/storage.v1.schema.json | 改 | ItemDef.shared；BranchState 新字段 |
| scripts/api-surface-test.sh | 改 | shared_refs/结构 JSON/DraftUpdate 断言 |
| README.md、docs/03-structure.md、docs/04-draft.md、docs/06-shared.md | 改 | 文档同步 |
| dev_docs/design/shared-ref-rework.md | 改 | 标注引用语义被取代 |

## 2. Slice 划分

- S1 模型与命令（model/command/validator/keys + 各 crate 构造器）→ `cargo test -p dsh-core -p dsh-publish -p dsh-jobs -p dsh-testkit`
- S2 状态机 apply + 测试改写（state.rs + state_machine.rs）→ `cargo test -p dsh-core`
- S3 API（dsh-publish/dsh-api + http_project_admin）→ `cargo test -p dsh-api`
- S4 Admin UI（app.js）→ dev-single 手动验证
- S5 契约与脚本（openapi/schema/api-surface-test）→ `bash scripts/check-contracts.sh` + `bash scripts/api-surface-test.sh`
- S6 文档（README/教程/rework 标注）→ 审读
- 全量：`cargo test --workspace` + dev-single 手动清单

## 3. S1：dsh-core 模型与命令（T1-T3）

### 任务 3.1 model.rs（T1 前半）
- `ItemDef`：删除 `pub shared_ref: Option<String>`（:197-200）；新增：
```rust
    /// 引用共享标记：true = 本项值为共享来源，由各分支在 shared_bindings 中选择引用哪个共享项。
    #[serde(default, skip_serializing_if = "is_false")]
    pub shared: bool,
```
- 文件顶部（ValueType 定义附近）新增辅助：
```rust
fn is_false(b: &bool) -> bool { !*b }
```
- `BranchState`（:263-282）：新增两字段：
```rust
    /// 分支级共享引用绑定：group → key → 共享项 key（仅对结构标记 shared=true 的 item 有意义）。
    #[serde(default)]
    pub shared_bindings: BTreeMap<String, BTreeMap<String, String>>,
    /// 绑定是否有未发布的变更（发布守卫/级联判定；设计 §4.6）。
    #[serde(default)]
    pub bindings_dirty: bool,
```
- `BranchState::new()`（:285-296）补 `shared_bindings: BTreeMap::new(), bindings_dirty: false,`。
- keys.rs:11 注释改为：「共享引用选择在分支状态 BranchState.shared_bindings；结构仅声明 ItemDef.shared 标记」。
- 验证：`cargo build --workspace`（编译错误列表 = 待改触点清单，即下方构造器任务）。

### 任务 3.2 各 crate 构造器（T1 后半）
全部 `ItemDef { ... shared_ref: None, ... }` 字面量改为 `shared: false`（保持字段顺序与 serde 一致）：
- dsh-testkit/src/lib.rs:22,31,40
- dsh-jobs/src/lib.rs:314,513,522,685
- dsh-publish/src/lib.rs:399,408
- dsh-core/tests/model_serde.rs:46,55；project_admin.rs:441,518
- dsh-core/tests/state_machine.rs（多处，见任务 4.7）
- dsh-raft/tests/cluster.rs（grep shared_ref 定位）
- state.rs 内部测试构造 :2706,2803,2899
- 验证：`cargo build --workspace && cargo test --workspace`（除 state_machine.rs 既有 shared_ref 语义测试外应全绿）。

### 任务 3.3 command.rs（T2）
- 新增载荷结构：
```rust
/// 分支级共享引用绑定条目（DraftUpdate 载荷；shared_key 空串 = 解除绑定）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SharedBinding {
    pub group: String,
    pub key: String,
    pub shared_key: String,
}
```
- `Command::DraftUpdate`（:76-90）新增字段：
```rust
    /// 分支级共享引用绑定 upsert/解除（空 shared_key = 解除）；旧日志无此字段 → 空（兼容）。
    #[serde(default)]
    shared_bindings: Vec<SharedBinding>,
```
- 验证：`cargo build -p dsh-core`（分发 match 未用新字段不影响编译）。

### 任务 3.4 validator.rs（T3）
- `validate_structure`（:122-129）：删除整块 `if let Some(rk) = &item.shared_ref { ... }`。
- `validate_publish`（:48-50）：`if item.shared_ref.is_some() { continue; }` → `if item.shared { continue; }`。
- 验证：`cargo test -p dsh-core`。

## 4. S2：dsh-core 状态机（T4-T10）

### 任务 4.1 apply_draft_update 绑定应用（T4，state.rs:1517）
- 签名追加参数（:1522 后）：`bindings: &[crate::command::SharedBinding],`
- 分发（:992-1008）：`Command::DraftUpdate { project, branch, updates, deletes, shared_bindings, operator, ts, expected_draft_rev }` → 调用处追加 `shared_bindings,`。
- 值拒绝（:1562）：`if def.shared_ref.is_some()` → `if def.shared`（错误文案不变）。
- 值写入循环后（:1588 `for (g, key) in deletes` 之前）新增绑定应用段：
```rust
        // 分支级共享引用绑定：upsert/解除（空 shared_key = 解除）；def 须存在且 shared=true。
        // 仅在实际变更时置 bindings_dirty（设计 §4.6）。
        let mut bindings_changed = false;
        for b in bindings {
            let def = index
                .get(&b.group)
                .and_then(|m| m.get(&b.key))
                .ok_or_else(|| Error::validation(format!("unknown item {}/{}", b.group, b.key)))?;
            if !def.shared {
                return Err(Error::validation(format!(
                    "item {}/{} 未标记为引用共享，不可绑定共享项",
                    b.group, b.key
                )));
            }
            let cur = st
                .shared_bindings
                .get(&b.group)
                .and_then(|m| m.get(&b.key))
                .map(|s| s.as_str());
            if b.shared_key.is_empty() {
                if cur.is_some() {
                    bindings_changed = true;
                    if let Some(m) = st.shared_bindings.get_mut(&b.group) {
                        m.remove(&b.key);
                        if m.is_empty() {
                            st.shared_bindings.remove(&b.group);
                        }
                    }
                }
                continue;
            }
            if !validator::valid_key_name(&b.shared_key) {
                return Err(Error::validation(format!(
                    "invalid shared key {:?}: only [A-Za-z0-9._-] allowed",
                    b.shared_key
                )));
            }
            let shared = self.get_shared(&b.shared_key)?.ok_or_else(|| {
                Error::validation(format!("shared item {} 未发布", b.shared_key))
            })?;
            if shared.ty != def.ty {
                return Err(Error::validation(format!(
                    "{}/{}: type {:?} 与共享项 {} 的 {:?} 不一致",
                    b.group, b.key, def.ty, b.shared_key, shared.ty
                )));
            }
            if cur != Some(b.shared_key.as_str()) {
                bindings_changed = true;
            }
            st.shared_bindings
                .entry(b.group.clone())
                .or_default()
                .insert(b.key.clone(), b.shared_key.clone());
        }
        if bindings_changed {
            st.bindings_dirty = true;
        }
```
- 验证：`cargo build -p dsh-core`。

### 任务 4.2 materialize_resolved 按分支绑定物化（T5，state.rs:1636-1653）
- 替换 shared_ref 循环为：
```rust
        for g in &structure.groups {
            for item in &g.items {
                if !item.shared {
                    continue;
                }
                let rk = st
                    .shared_bindings
                    .get(&g.name)
                    .and_then(|m| m.get(&item.key));
                let Some(rk) = rk else {
                    errs.push(format!("{}/{}: 未选择引用共享项", g.name, item.key));
                    continue;
                };
                match self.get_shared(rk)? {
                    Some(shared) => {
                        if shared.ty != item.ty {
                            errs.push(format!(
                                "{}/{}: 共享项 {rk} 类型 {:?} 与结构声明 {:?} 不一致",
                                g.name, item.key, shared.ty, item.ty
                            ));
                            continue;
                        }
                        resolved
                            .entry(g.name.clone())
                            .or_default()
                            .insert(item.key.clone(), shared.value.clone());
                    }
                    None => errs.push(format!(
                        "{}/{}: shared item {rk} 缺失（悬空引用）",
                        g.name, item.key
                    )),
                }
            }
        }
```
- 验证：`cargo build -p dsh-core`（物化行为单测在 4.7）。

### 任务 4.3 发布守卫与脏标记（T6，state.rs:1679/1710/1818/1842）
- `apply_publish`：`if st.value_draft.is_empty()` → `if st.value_draft.is_empty() && !st.bindings_dirty`；`st.value_draft.clear();` 后追加 `st.bindings_dirty = false;`。
- `apply_gray_publish`：同上两处（:1818/:1842）。
- 验证：`cargo build -p dsh-core`。

### 任务 4.4 结构发布清理 + 删 check_shared_refs（T7，state.rs:1394）
- 引用项草稿值清理（:1491）：`if item.shared_ref.is_some()` → `if item.shared`。
- 该清理块后新增绑定清理：
```rust
            // 分支级绑定清理：仅保留仍在结构中、仍 shared=true、且绑定共享项类型与新结构 ty 一致的条目
            let new_shared: std::collections::HashMap<(String, String), ValueType> =
                new_structure
                    .groups
                    .iter()
                    .flat_map(|g| {
                        g.items
                            .iter()
                            .filter(|i| i.shared)
                            .map(|i| ((g.name.clone(), i.key.clone()), i.ty))
                    })
                    .collect();
            st.shared_bindings.retain(|g, m| {
                m.retain(|k, rk| {
                    match new_shared.get(&(g.clone(), k.clone())) {
                        None => false, // item 已删除或不再 shared
                        Some(ty) => self
                            .get_shared(rk)
                            .ok()
                            .flatten()
                            .map(|s| s.ty == *ty)
                            .unwrap_or(false), // 类型失配或共享项缺失 → 丢弃
                    }
                });
                !m.is_empty()
            });
```
- 删除 `fn check_shared_refs`（:2182-2203）及其两处调用（:1384-1385、:1423-1424）。
- 验证：`cargo build -p dsh-core`。

### 任务 4.5 分支创建 source（T8，state.rs:1304-1332）
- 替换源快照复制段：
```rust
        if let Some(src) = source {
            let src_state = self
                .get_branch_state(id, src)?
                .ok_or_else(|| Error::validation(format!("source branch {src} not found")))?;
            if src_state.active_version == 0 {
                return Err(Error::validation(format!(
                    "source branch {src} has no published version"
                )));
            }
            let snap = self.snapshot_of(id, src, src_state.active_version)?;
            // 跳过结构标记 shared=true 的 item（避免物化值变成引用项本地草稿）；继承源分支绑定
            let shared_items: std::collections::HashSet<(String, String)> = structure
                .groups
                .iter()
                .flat_map(|g| {
                    g.items
                        .iter()
                        .filter(|i| i.shared)
                        .map(|i| (g.name.clone(), i.key.clone()))
                })
                .collect();
            state.value_draft = snap
                .into_iter()
                .map(|(g, items)| {
                    let m = items
                        .into_iter()
                        .filter(|(k, _)| !shared_items.contains(&(g.clone(), k.clone())))
                        .map(|(k, v)| {
                            (
                                k,
                                DraftValue {
                                    value: v,
                                    updated_at: now_ms,
                                },
                            )
                        })
                        .collect();
                    (g, m)
                })
                .filter(|(_, m)| !m.is_empty())
                .collect();
            state.shared_bindings = src_state.shared_bindings.clone();
            state.bindings_dirty = false;
        }
```
- 验证：`cargo build -p dsh-core`。

### 任务 4.6 级联/删除/反向引用（T9，state.rs:2029-2203）
- `shared_usage`（:2165-2180）改扫分支绑定，返回 4 元组：
```rust
    /// 反向引用：扫描全项目全分支 shared_bindings，收集绑定 == key 的 (project, branch, group, item_key)。
    pub fn shared_usage(
        &self,
        key: &str,
    ) -> Result<Vec<(ProjectId, BranchName, String, String)>, Error> {
        let mut out = Vec::new();
        for p in self.list_projects()? {
            for b in self.list_branches(&p.id)? {
                if let Some(st) = self.get_branch_state(&p.id, &b)? {
                    for (g, m) in &st.shared_bindings {
                        for (k, rk) in m {
                            if rk == key {
                                out.push((p.id.clone(), b.clone(), g.clone(), k.clone()));
                            }
                        }
                    }
                }
            }
        }
        Ok(out)
    }
```
- `apply_shared_publish`（:2063-2075）级联循环改为逐分支：
```rust
            for (project, branch, group, key) in self.shared_usage(&item.key)? {
                self.cascade_to_branch(
                    &project,
                    &branch,
                    &group,
                    &key,
                    &item.value,
                    comment,
                    request_id,
                    now_ms,
                    &mut events,
                )?;
            }
```
- `cascade_to_project`（:2080-2123）重构为 `cascade_to_branch`（删外层分支循环；`branch` 改为参数 `&BranchName`；`self.get_branch_state(project, branch)`；`self.snapshot_of(project, branch, ...)`；`branch_state_key(project, branch)`；`PublishEvent { project: project.clone(), branch: branch.clone(), ... }`；其余逻辑逐字保留，含 `EventType::SharedCascade`）。
- `apply_shared_delete`（:2141）：冲突 detail 解构从 3 元组改 4 元组并带 branch：
```rust
                let detail = refs
                    .iter()
                    .map(|(p, b, g, k)| format!("{}/{}/{}/{}", p.as_str(), b.as_str(), g, k))
                    .collect::<Vec<_>>()
                    .join(", ");
```
- 验证：`cargo build -p dsh-core`。

### 任务 4.7 测试改写与新增（T10，tests/state_machine.rs）
- 既有 shared_ref 用例改写（grep `shared_ref` 定位）：
  - :2808 附近结构构造 → `shared: true` + 分支绑定共享项后再断言；:2904 → `shared: false`；
  - project_delete_removes_structure_shared_refs（:2762）→ 改名/语义：删除项目连带清其分支绑定（断言改为项目删除后 shared_usage 无该项目）。
- 新增用例（沿用现有 helper：mk_project/mk_shared/publish 辅助）：
  1. `branch_scoped_binding_differs`（核心场景）：共享项 A、B（同 type=String，值不同）→ 结构 item `shared: true` → dev 绑 A、prod 绑 B → 各自发布 → 断言快照值不同（dev=A 值、prod=B 值）。
  2. `shared_item_unbound_blocks_publish`：shared 项无绑定 → Publish Block → ERR_PUBLISH_BLOCKED（detail 含「未选择」）；Warn → 成功且快照无该项。
  3. `binding_type_mismatch_rejected`：绑定 type 与结构 ty 不一致 → DraftUpdate validation 错误。
  4. `binding_missing_shared_item_rejected`：绑定未发布共享项 → 错误。
  5. `binding_only_publish_allowed`：值草稿空、仅改绑定 → 发布成功；再改回原绑定但未再变更 → 再次发布 NoDraft。
  6. `bindings_persist_after_publish`：绑定 → 发布 → 再发布值变更（值草稿更新）→ 物化仍取绑定共享值；断言 bindings_dirty=false。
  7. `structure_publish_cleans_bindings`：删 item / shared→local 翻转 / ty 变更 → 各分支绑定被清（assert shared_bindings 空）。
  8. `shared_publish_cascades_only_bound_branches`：dev 绑 K、prod 未绑 → 共享 K 发布（Auto）→ dev 版本推进、prod 版本不变。
  9. `shared_delete_blocked_when_bound`：分支绑 K → SharedDelete → 409（detail 含 branch）；解除后删除成功。
  10. `branch_create_source_skips_shared_and_inherits_bindings`：source 分支有绑定 + 值草稿 → 新分支：绑定继承、shared item 无本地草稿值。
  11. `draft_value_write_to_shared_item_rejected`：shared 项写本地值 → validation 错误。
- 验证：`cargo test -p dsh-core`（全绿）。

## 5. S3：dsh-api（T11）

### 任务 5.1 dsh-publish update_draft 透传（lib.rs:123）
- 签名追加 `bindings: Vec<crate::command::SharedBinding>,`；`Command::DraftUpdate { ... updates, deletes, shared_bindings: bindings, operator, ts, expected_draft_rev }`。
- `encrypt_secret_updates` 只处理 updates，绑定无需加密。

### 任务 5.2 DraftUpdateReq / update_draft handler（dsh-api lib.rs:299-310、924）
- 新增：
```rust
#[derive(Deserialize)]
struct SharedBindingReq {
    group: String,
    key: String,
    shared_key: String,
}
```
- `DraftUpdateReq` 追加 `#[serde(default)] shared_bindings: Vec<SharedBindingReq>,`。
- handler `update_draft`：将 req.shared_bindings map 为 `dsh_core::command::SharedBinding` 传入 `app.publish.update_draft(..., bindings, ...)`；审计 detail 加 `"bindings": req.shared_bindings.len()`。

### 任务 5.3 branch_detail shared_refs（dsh-api lib.rs:1301-1319）
- 替换为：
```rust
    // 引用项展示：结构 shared 项 × 本分支绑定解析值（secret 掩码）；未绑定项 shared_key 为空串
    let mut shared_refs = Vec::new();
    if let Some(structure) = sm.get_structure(&id).map_err(ApiError::from)? {
        for g in &structure.groups {
            for item in &g.items {
                if !item.shared {
                    continue;
                }
                let rk = st
                    .shared_bindings
                    .get(&g.name)
                    .and_then(|m| m.get(&item.key));
                match rk {
                    Some(rk) => {
                        if let Some(shared) = sm.get_shared(rk).map_err(ApiError::from)? {
                            shared_refs.push(serde_json::json!({
                                "group": g.name,
                                "key": item.key,
                                "shared_key": rk,
                                "version": shared.version,
                                "value": masked_shared_value(&shared),
                            }));
                        }
                    }
                    None => shared_refs.push(serde_json::json!({
                        "group": g.name,
                        "key": item.key,
                        "shared_key": "",
                        "version": null,
                        "value": null,
                    })),
                }
            }
        }
    }
```
（注意：`st` 为分支状态，已在函数前部取得。）

### 任务 5.4 list_shared refs + delete detail（dsh-api lib.rs:1593-1621、1722-1735）
- `shared_item_json`（:1593-1621）：refs 参数类型 `Option<&[(ProjectId, String, String)]>` → `Option<&[(ProjectId, BranchName, String, String)]>`；映射解构与字段：
```rust
            r.iter()
                .map(|(p, b, g, k)| serde_json::json!({
                    "project": p.as_str(),
                    "branch": b.as_str(),
                    "group": g,
                    "item_key": k,
                }))
                .collect::<Vec<_>>()
```
- `list_shared`（:1730）：`shared_usage` 返回 4 元组后无需改动调用，类型随函数签名自动更新。
- `delete_shared`/`delete_shared_draft`：detail 已含 branch（state.rs 侧改动，§4.6）；API 层无需改动。

### 任务 5.5 http_project_admin.rs
- branch detail shared_refs 断言更新：未绑定 shared 项 → `shared_key: ""`；已绑定 → 含 version/value（掩码）。
- 新增 DraftUpdate 带 shared_bindings 的用例（绑定/解除/类型错误 400）。
- SharedItem.refs 断言含 branch。
- 验证：`cargo test -p dsh-api`。

## 6. S4：Admin UI（T13，admin/app.js）

### 任务 6.1 结构编辑器（structItemRowHtml :1199-1217、collectStructDraft :1312、serializeGroups :794、validateGroups :1284、组头 :623）
- `structItemRowHtml`：删共享引用 `<select data-sf="ishref">` 与 refBadge；新增勾选框：
```js
<label class="check" title="勾选后该项值由共享库物化，各分支在草稿页选择引用的共享项">
  <input type="checkbox" data-sf="ishared" data-act="structShared" ${it.shared ? 'checked' : ''}>引用共享
</label>
```
- 勾选联动：`data-act="structShared"` 处理函数——勾选后该行 required/secret 控件 disabled + 行内提示「类型约束：分支下拉仅显示同类型共享项」；取消恢复。
- `collectStructDraft`/`serializeGroups`：`shared_ref` 收集 → `shared: !!sharedChk.checked`（JSON 模式同样支持 `shared` 布尔）。
- `validateGroups`：删除 shared_ref 的 NAME_RE 校验（:1284）。
- 组头徽章（:623-624）：`it.shared_ref || draftRefs[...]` → `it.shared`；计数文案不变。

### 任务 6.2 草稿页（renderDraftEditor :598-651、saveDraft :836-875）
- 删除 `draftRefs`（:612-614）及其使用（:627-634）。
- `g.items.map` 分支：`it.shared` → 渲染**绑定行**（新增函数 `sharedBindRowHtml(g, it, b)`）：
```js
function sharedBindRowHtml(g, it, refs) {
  const ref = refs && refs[g.name + '/' + it.key];
  const opts = '<option value="">— 请选择 —</option>' + (S.sharedItems || [])
    .filter((s) => s.type === it.type)
    .map((s) => `<option value="${esc(s.key)}"${s.key === (ref && ref.shared_key) ? ' selected' : ''} title="${esc(s.description || '')}">${esc(s.key)}${s.secret ? ' 🔒' : ''}${s.description ? ' · ' + esc(s.description) : ''}</option>`).join('');
  const valTxt = ref && ref.value ? fmtVal(ref.value) : '<span class="muted">未选择共享项</span>';
  return `<div class="grow ref-grow">
    <div class="gkey"><span class="mono">${esc(it.key)}</span><span class="badge acc ref-badge">引用共享</span>${it.description ? `<div class="hint small">${esc(it.description)}</div>` : ''}</div>
    <div class="gtype"><span class="ty">${esc(it.type || '')}</span></div>
    <div class="gctl"><select class="sel draft-shared-bind" data-g="${esc(g.name)}" data-k="${esc(it.key)}">${opts}</select><div class="hint small" style="margin-top:2px">${valTxt}</div></div>
    <div class="gdel"><span class="hint">${ref && ref.version ? 'v' + ref.version : ''}</span></div>
  </div>`;
}
```
- `renderDraftEditor`：`S.sharedRefs` 保留（含未绑定项 `shared_key: ""`）；shared 行调用 `sharedBindRowHtml`。
- `saveDraft`：新增收集：
```js
  const shared_bindings = [];
  for (const sel of $$('#pane-draft .draft-shared-bind')) {
    shared_bindings.push({ group: sel.dataset.g, key: sel.dataset.k, shared_key: sel.value });
  }
```
  载荷加 `shared_bindings`（`updates`/`deletes` 同理；绑定变更也置 dirty —— 事件委托 :1968-1975 的 class 判定加 `draft-shared-bind`）。
- 绑定变更后重渲染（loadBranch 已刷新 shared_refs → 物化值展示更新）。

### 任务 6.3 共享库页（view-shared）
- 「被引用」tooltip：`${r.project}/${r.branch}/${r.group}/${r.item_key}`（数据源 shared_item_json refs 已含 branch）。

### 任务 6.4 构建验证
- `cd server && source ../scripts/build-env.sh && cargo build --workspace`；
- dev-single 手动清单（§8 验证策略）。

## 7. S5：契约与脚本（T12）

### 任务 7.1 openapi.v1.yaml
- `ItemDef`（:961-969）：追加
```yaml
        description: { type: string, maxLength: 200, description: 助记描述（≤200 字节，不渲染） }
        shared: { type: boolean, default: false, description: 引用共享标记：true 时值由共享库物化，各分支在草稿页选择引用的共享项 }
```
- `Branch.shared_refs`（:932-942）：description 改为「结构 shared 项 × 本分支绑定解析值（secret 已掩码）；未绑定项 shared_key 为空串」；`shared_key` 描述注明空串=未绑定；`version`/`value` 允许 null（`type: [integer, "null"]` / `anyOf`）。
- `PUT /projects/{p}/branches/{b}/draft` 请求体：追加 `shared_bindings: [{ group, key, shared_key }]`（shared_key 空串=解除）。
- `SharedItem.refs` items（:1167-1172）：追加 `branch: { type: string }`。
- 删除 openapi 中任何 ItemDef.shared_ref 残留（grep 确认无）。

### 任务 7.2 schema/storage.v1.schema.json
- `ItemDef`（:46-49）：`shared_ref` → `shared: { type: boolean, default: false, description: 引用共享标记（值由共享库物化，各分支选择引用项） }`。
- `BranchState`（:223-254）：追加
```json
        "shared_bindings": {
          "type": "object",
          "additionalProperties": {
            "type": "object",
            "additionalProperties": { "type": "string" },
            "description": "key → 共享项 key"
          },
          "description": "分支级共享引用绑定：group → key → 共享项 key"
        },
        "bindings_dirty": { "type": "boolean", "default": false }
```

### 任务 7.3 scripts/api-surface-test.sh
- 结构 JSON 断言：去 `shared_ref`，加 `shared`（`"shared": true` 场景）。
- draft 请求断言：加 `shared_bindings` 示例。
- branch detail `shared_refs` 断言：含未绑定项（shared_key 空串）。
- 验证：`bash scripts/check-contracts.sh && bash scripts/api-surface-test.sh`。

## 8. S6：文档（T14）

- README「核心能力」共享配置段：结构声明「引用共享」+ 各分支独立选择；级联语义更新。
- docs/03-structure.md §3.3：结构页「引用共享」勾选 + 类型约束；草稿页下拉选择（原"只读"描述更新）。
- docs/04-draft.md：引用共享行改为「下拉选择 + 物化值展示」。
- docs/06-shared.md：级联改为「仅推进绑定该共享项的分支」；被引用计数 = 绑定数。
- dev_docs/design/shared-ref-rework.md 头部标注：「引用语义部分已被 shared-ref-branch-scope.md 取代（结构声明 + 分支选择）」。
- design-v2.md §4.6-4.7 / design-modules/01-core.md 引用/命令表同步。

## 9. 验证策略（T15，整体）

1. `cd server && source ../scripts/build-env.sh && cargo test --workspace`（全绿）。
2. `bash scripts/check-contracts.sh`（proto/openapi/schema lint）。
3. `bash scripts/api-surface-test.sh`（dev-single 自动全流程）。
4. dev-single 手动清单：
   - 建共享项 A、B（同 type String，值不同）→ 结构页勾「引用共享」→ 发布结构；
   - dev 草稿页下拉绑 A、prod 绑 B → 各自发布 → 两分支「查看配置」值不同（核心验收）；
   - 共享项 A 更新发布（Auto）→ 仅 dev 版本推进；
   - 删除被绑定共享项 → 409 明细含分支；
   - 新建 shared 项未绑定 → 发布阻断提示「未选择」；
   - 结构 type 从 String 改 Int → 发布结构 → 分支绑定被清、需重新选择。

## 10. 风险与处置

| 风险 | 处置 |
| --- | --- |
| lib.rs 共存改动 | 外科手术式编辑；T11 完成前后各跑一次 `cargo test -p dsh-api` |
| 旧日志重放 | 命令新字段全部 `#[serde(default)]`；apply 路径不读取缺失字段 |
| 绑定脏标记漏清 | 测试 5/6 覆盖守卫与持久化；发布/灰度两条路径都清 |
| 级联范围错误 | 测试 8 精确断言「仅绑定分支推进」 |
| UI 下拉数据源为空 | 结构 shared 项无共享项可绑 → 下拉「— 请选择 —」；服务端校验兜底 |

## 11. 出口标准

- 设计文档 `shared-ref-branch-scope.md` 经用户审核通过（含后续修订）；
- 本计划全部 slice 完成，`cargo test --workspace` / check-contracts / api-surface-test 全绿；
- 手动清单核心场景（分支差异化绑定）验证通过；
- 文档（README/教程/design-v2/01-core）同步无残留旧 shared_ref 语义。
