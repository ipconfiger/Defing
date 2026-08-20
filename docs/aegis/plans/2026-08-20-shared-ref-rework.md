# 开发计划：共享配置引用关系重构（shared-ref rework）

日期: 2026-08-20
上游: docs/design/shared-ref-rework.md（已审核通过）
执行路线: 单工作区逐 slice 实现，每 slice 后跑对应测试；全部完成后整体验证

## TDD Route

```text
TDD Route:
- Mode: off
- Decision: skipped
- Strict authority: not applicable（用户未要求 strict TDD）
- Test posture: post-change regression —— 既有测试随实现同步更新，新行为（引用物化/只读拒绝/级联/删除阻断）配对新测试
- Reason: 用户流程为「设计→计划→实现」，无 strict TDD 指令；仓库既有测试基线（cargo test --workspace）即回归网
- Verification: cargo test --workspace（基线 172 测试，实现后全绿）+ scripts/api-surface-test.sh + 手动 UI 清单
```

## 0. 目标与基线

- 目标（5 项，见设计文档 §1）：P1 结构编辑器「共享引用」下拉；P2 移除共享库引用绑定、引用内嵌 ItemDef.shared_ref；P3 ItemDef/SharedItem 加 description；P4 共享库去分组扁平化；P5 草稿页分组管理入口。
- 兼容边界：dev 阶段 breaking change（删除 /api/v1/shared/refs、SharedItem.group）；**不做数据迁移**（旧数据目录失效，同 rocksdb→redb 惯例）。
- 基线命令：`cd server && source ../scripts/build-env.sh && cargo test --workspace`（172 测试全绿）。
- 二进制：`server/target/debug/defing`；e2e：`bash scripts/api-surface-test.sh`（自起 dev-single）。

## 1. 文件地图

| 文件 | 动作 | 内容 |
| --- | --- | --- |
| server/crates/dsh-core/src/model.rs | 改 | ItemDef+description/shared_ref；SharedItem 去 group +description；删 RefBinding |
| server/crates/dsh-core/src/command.rs | 改 | 删 RefBind/RefUnbind；增 SharedDelete |
| server/crates/dsh-core/src/keys.rs | 改 | shared_key/shared_draft_key 扁平；删 shared_prefix/ref_key/idx_ref/group_ref_index_key/K_IDX_REF/K_IDX_REFG |
| server/crates/dsh-core/src/validator.rs | 改 | validate_structure 校验 shared_ref 字符集；validate_publish 跳过 shared_ref 项 |
| server/crates/dsh-core/src/state.rs | 改 | 见 §3 任务 3.1-3.12 |
| server/crates/dsh-core/src/lib.rs | 改 | 导出清理（RefBinding 移除） |
| server/crates/dsh-core/tests/state_machine.rs | 改 | 共享/引用测试重写（见任务 3.12） |
| server/crates/dsh-core/tests/model_serde.rs | 改 | SharedItem 构造去 group |
| server/crates/dsh-api/src/lib.rs | 改 | 见 §4 任务 4.1-4.6 |
| server/crates/dsh-api/tests/http_project_admin.rs | 改 | 删 refs 断言；shared 断言更新+delete 用例 |
| server/crates/dsh-api/admin/index.html | 改 | 共享库页/结构/草稿 UI（§5） |
| server/crates/dsh-api/admin/app.js | 改 | 同上 + 数据源（S.sharedItems、draft shared_refs） |
| api/openapi.v1.yaml | 改 | SharedItem/ItemDef schema、删 refs、增 delete、draft shared_refs |
| schema/storage.v1.schema.json | 改 | 同上语义同步 |
| scripts/api-surface-test.sh | 改 | shared/refs 断言更新（§6） |
| docs/README.md | 改 | 共享配置说明 |

## 2. Slice 划分

- S1 核心模型与键（model/command/keys/validator + model_serde）→ cargo test -p dsh-core
- S2 状态机 apply（state.rs + state_machine.rs 重写）→ cargo test -p dsh-core
- S3 API 层（lib.rs + http_project_admin.rs）→ cargo test -p dsh-api
- S4 Admin UI（index.html/app.js）→ 手动 UI 清单 + cargo build
- S5 契约与脚本（openapi/storage schema/api-surface-test/README）→ bash scripts/api-surface-test.sh
- S6 全量验证 + 文档收尾

## 3. S1+S2：dsh-core

### 任务 3.1 model.rs
- `ItemDef` 增加：
  - `#[serde(default, skip_serializing_if = "Option::is_none")] pub description: Option<String>`
  - `#[serde(default, skip_serializing_if = "Option::is_none")] pub shared_ref: Option<String>`
- `SharedItem`：删 `pub group: String`；增 `#[serde(default, skip_serializing_if = "Option::is_none")] pub description: Option<String>`。
- 删除 `pub struct RefBinding`（model.rs:415-423）。
- limits.rs 增 `pub const MAX_DESC_BYTES: usize = 200;`。
- 验证：`cargo build -p dsh-core`（编译错误列表 = 待改触点清单）。

### 任务 3.2 command.rs
- 删 `RefBind`/`RefUnbind` 变体。
- 增 `SharedDelete { key: String, #[serde(default)] operator: String }`。

### 任务 3.3 keys.rs
- `shared_key(key: &str) -> String` = `format!("{K_SHARED}{key}")`；`shared_draft_key(key)` 同。
- 删 `shared_prefix`、`ref_key`、`idx_ref`、`group_ref_index_key`、`K_IDX_REF`、`K_IDX_REFG`。
- 更新 key_shapes 单测（shared_key 形态断言）。

### 任务 3.4 validator.rs
- validate_structure：item 循环内增 `if let Some(r) = &item.shared_ref { if !valid_key_name(r) { errs.push(...) } }`。
- validate_publish：item 循环首行 `if item.shared_ref.is_some() { continue; }`。

### 任务 3.5 state.rs 共享访问器
- `get_shared(&self, key: &str)`（删 group 参数）；`list_shared_published/list_shared_drafts` 前缀扫描不变，sort_by 去 group 维（仅按 key）。
- `list_refs`、`read_refs_of_project`、`ref_index_key`、`group_ref_index_key` 删除。
- 新增 `pub fn shared_usage(&self, key: &str) -> Result<Vec<(ProjectId, String, String)>, Error>`：遍历 `list_projects()`，读 `get_structure`，收集 `items.shared_ref == key` 的 (project, group, item_key)。

### 任务 3.6 apply_shared_draft_update（state.rs:1979）
- key 校验去 group；`shared_draft_key(&item.key)` 存；SharedItem 构造去 group 字段、带 description。

### 任务 3.7 apply_shared_publish（state.rs:2061）级联改扫描
- 每草稿项：`prev = get_shared(&item.key)`；published 构造去 group + 带 description；存 `shared_key(&item.key)`。
- 级联：删 idx/ref + idx/refg 反查，改为：
```rust
for (project, group, key) in self.shared_usage(&item.key)? {
    self.cascade_to_project(&project, &group, &key, &item.value, comment, request_id, now_ms, &mut events)?;
}
```
- Manual 模式分支保留。

### 任务 3.8 apply_shared_delete（新增）
```rust
fn apply_shared_delete(&mut self, key: &str, _op: &str) -> ApplyOutcome {
    if !validator::valid_key_name(key) { return Err(Error::validation("shared key 非法")); }
    let published = self.get_shared(key)?;
    if published.is_some() {
        let refs = self.shared_usage(key)?;
        if !refs.is_empty() {
            return Err(Error::conflict(format!("shared item {key} 被 {} 处引用: {:?}", refs.len(), refs)));
        }
        self.delete_pending(shared_key(key).as_bytes())?;
    }
    self.delete_pending(shared_draft_key(key).as_bytes())?; // 幂等
    Ok(vec![])
}
```
- 分发 match 增 `Command::SharedDelete { key, operator } => self.apply_shared_delete(key, operator)`；删 RefBind/RefUnbind 分支。

### 任务 3.9 引用校验 helper + 结构 draft/publish
- 新增 `fn check_shared_refs(&self, structure: &Structure) -> Result<(), Error>`：每个 `shared_ref` item：`get_shared(key)` 必须 Some（否则 validation "shared item {key} 未发布"）；`item.ty != shared.ty` → validation "type 与共享项不一致"。
- apply_structure_draft_set：validate_structure 后调 check_shared_refs。
- apply_publish_structure：validate 后调 check_shared_refs；D14 清理段后追加：
```rust
for g in &new_structure.groups { for item in &g.items {
    if item.shared_ref.is_some() {
        if let Some(m) = st.value_draft.get_mut(&g.name) { m.remove(&item.key); }
    }
} }
```

### 任务 3.10 materialize_resolved（state.rs:1598）只读物化
- 删除 read_refs_of_project 循环；validate_publish 之后新增：
```rust
for g in &structure.groups { for item in &g.items {
    if let Some(rk) = &item.shared_ref {
        match self.get_shared(rk)? {
            Some(shared) => { resolved.entry(g.name.clone()).or_default().insert(item.key.clone(), shared.value.clone()); }
            None => errs.push(format!("{}/{}: shared item {} 缺失（悬空引用）", g.name, item.key, rk)),
        }
    }
} }
```
（errs 并入既有 policy 检查：Block 拒绝 / Warn 记录，与 G1/D35 一致。）

### 任务 3.11 apply_draft_update 只读拒绝（state.rs:1510）
- 值校验前：`if def.shared_ref.is_some() { return Err(Error::validation(format!("item {}/{} 引用共享项，不可设置本地值", u.group, u.key))); }`

### 任务 3.12 测试：state_machine.rs 与 model_serde.rs
- model_serde.rs：SharedItem 构造删 group（:118 附近）。
- state_machine.rs：
  - `publish_shared(s, group, key, ...)` helper 签名 → `publish_shared(s, key, ...)`；全部调用点去 group 参数。
  - 重写/删除引用相关测试：`shared_cascade_updates_referencing_branches`（改：结构 item 带 shared_ref → 共享发布级联命中）、`ref_requires_published_shared`（改：结构草稿引用未发布共享项被拒）、`group_ref_*`（:937,:960,:1029,:1068 删除——整组引用已移除）。
  - 新增：结构 shared_ref 物化；引用项写草稿被拒；结构发布清理引用项草稿值；悬空引用发布阻断；type 不一致结构保存被拒；SharedDelete（被引用 409 / 未引用成功）；shared_usage 反向映射。
  - `rewrap_deks` 共享行键断言更新为 `sh/{key}`。
  - state.rs 内 `#[cfg(test)]` 的 `shared_item(group, key)` helper（:2745）签名去 group。
- 验证：`cargo test -p dsh-core` 全绿。

## 4. S3：dsh-api

### 任务 4.1 SharedItemReq / shared_item_json（lib.rs:1443-1474）
- SharedItemReq：删 `group`；增 `#[serde(default)] description: Option<String>`。
- shared_item_json：去 group；增 description；增 `refs`（调 `sm.shared_usage(key)` 映射 `[{project, group, item_key}]`）。
- list_shared / list_shared_drafts 保持（草稿列表不返回 refs）。

### 任务 4.2 write_shared_draft（lib.rs:1477）
- 去 group；SharedItem 构造去 group + description；审计 detail 去 group；描述校验 `description.len() <= MAX_DESC_BYTES`。

### 任务 4.3 共享删除处理器
- `delete_shared`（Path(key)）→ `Command::SharedDelete`，审计 action `shared_delete`，204。
- `delete_shared_draft`（Path(key)）→ 同走 SharedDelete（幂等），204。
- 删 `list_refs`/`ref_bind`/`ref_unbind` 处理器与路由行 `/api/v1/shared/refs`。
- 路由：`/api/v1/shared/{key}`、`/api/v1/shared-draft/{key}` 增 delete（精确路径，与 `/api/v1/shared` 不冲突）。

### 任务 4.4 branch_detail 增 shared_refs
- 响应增 `shared_refs`：遍历结构 items.shared_ref → `get_shared` → `{group, key, shared_key, version, value: masked_shared_value(shared)}`；缺失项跳过。

### 任务 4.5 promote 跳过引用项（lib.rs:1344）
- handler 取 dst structure 构建 `(group,key)` 引用集；主循环 `if ref_set.contains(&(g,k)) { skipped.push(key); continue; }`。

### 任务 4.6 http_project_admin.rs
- 删 refs 端点断言（:265-271, :279-283）；shared CRUD 去 group 加 description；新增 delete 用例（未引用 204 / 被引用 409）。
- 验证：`cargo test -p dsh-api`。

## 5. S4：Admin UI（index.html / app.js）

### 任务 5.1 共享库页
- index.html：删「引用绑定」卡片（:374-403）；删 sh-group 输入，增 sh-desc 输入；列表 thead 改「key/类型/状态/值/描述/被引用/标记/操作」。
- app.js：loadShared 行渲染去 group、加 description/refs/删除按钮（草稿→DELETE /api/v1/shared-draft/{key}；发布→DELETE /api/v1/shared/{key}，409 toast 引用明细）；saveShared 去 group 加 description；删 actions.bindRef。

### 任务 5.2 结构编辑器
- S 增 `sharedItems: []`（loadShared 填充）。
- structItemRowHtml：增「描述」input（data-sf="idesc"）与「共享引用」select（data-sf="ishref"，选项=「—」+sharedItems，label `{key}（{type}）`，title 带 description）。
- 选中 ishref 后 type/required/secret 控件 disabled 并显示共享项属性；取消恢复。
- collectStructDraft/serializeGroups/validateGroups/JSON 模式同步 description/shared_ref。
- 组头（struct-ghead）补「x 项 · y 引用共享」badge。

### 任务 5.3 草稿页
- 组卡片头增「管理分组」按钮（data-act="manageGroups" → switchPane('structure')）。
- renderDraftGroups：shared_ref 项渲染只读行（徽标「引用共享项 {key} · v{version}」+ 值，值源 draft 响应 shared_refs；secret masked）。
- populateKeySel / fallback 路径：命中 shared_ref 项 → 提示「该项引用共享项，不可添加本地值」。

### 任务 5.4 构建验证
- `source ../scripts/build-env.sh && cargo build`；dev-single 手动走 UI 清单（§7）。

## 6. S5：契约与脚本

### 任务 6.1 openapi.v1.yaml
- SharedItem 删 group 增 description/refs；SharedItemReq 同步；ItemDef 增 shared_ref/description。
- 删 RefBinding/RefBindingReq/RefUnbindReq 与 /api/v1/shared/refs 路径。
- 增 DELETE /api/v1/shared/{key}、DELETE /api/v1/shared-draft/{key}；draft 响应增 shared_refs。

### 任务 6.2 schema/storage.v1.schema.json
- SharedItem/SharedDraft 去 group 加 description；ItemDef 增两字段；删 RefBinding；audit action enum 增 "shared_delete"。

### 任务 6.3 scripts/api-surface-test.sh（:67-84）
- shared 请求体去 group；断言去 group 维；删 refs 三行断言；增 description、DELETE 两个端点（含被引用 409 分支）断言。
- 验证：`bash scripts/api-surface-test.sh` 全绿。

### 任务 6.4 README.md
- 「核心能力」共享配置描述更新（去分组、引用在项目结构、描述字段）。

## 7. 验证策略（整体）

1. `cd server && source ../scripts/build-env.sh && cargo test --workspace` → 全绿。
2. `bash scripts/api-surface-test.sh` → 全绿。
3. 手动 UI（dev-single）：共享项含描述建/列/发布；结构 item 选共享引用（type/required/secret 只读）→ 发布结构；草稿页引用项只读徽标+值、「管理分组」跳转；共享发布级联版本推进；删除被引用共享项 409 列引用方、解除后删除成功。

## 8. 风险与处置

| 风险 | 处置 |
| --- | --- |
| 编译断链（字段/签名批量变更） | 任务 3.1 先 build 收集全量触点再改 |
| axum 路由 /api/v1/shared/{key} 与 /api/v1/shared 冲突 | 精确路径不冲突；若报重叠则改 /api/v1/shared/item/{key} |
| shared_refs 未随共享发布刷新 | UI 在发布/刷新动作后重拉 GET /shared 与 draft |
| 灰度路径回归 | 复用 materialize_resolved，测试覆盖 gray publish 引用物化 |
| promote 引用项跳过 | 4.5 实现 + 手动验证 skipped 提示 |

## 9. 出口标准

- 设计文档 §1 五问题全部落实；§10 风险处置生效。
- cargo test --workspace、api-surface-test.sh 全绿；手动 UI 清单逐项通过。
- openapi / storage schema / README 与实现一致。
