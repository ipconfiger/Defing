# 设计文档：分支级共享引用（shared-ref branch-scope）

状态: v1 待审核
日期: 2026-08-21
范围: dsh-core / dsh-api / admin UI / openapi / storage schema / 测试脚本 / 教程文档
关联文档: dev_docs/design/shared-ref-rework.md（本设计取代其引用语义部分）、dev_docs/design-v3.md、dev_docs/design-modules/01-core.md、dev_docs/aegis/plans/2026-08-20-shared-ref-rework.md

---

## 1. 背景与问题

现状（shared-ref rework 已实现，commit 830e801）：共享引用关系内嵌在**项目结构**的配置项定义 `ItemDef.shared_ref: Option<String>` 上。结构是**项目级、全分支共用**的，因此：

> 全部分支对同一配置项只能引用**同一个**共享项 —— 分支无法差异化。

典型失败场景：`db.host` 在 dev 分支想引用 `dev-db-host`、在 prod 分支想引用 `prod-db-host` —— 当前模型做不到，只能拆 key 或复制共享项。

## 2. 概念与架构分析结论（已与用户确认）

### 2.1 用户诊断验证 ✅

「引用哪个共享项」本质是**值类**数据（决定该 key 在某个分支取哪份值）；Defing 核心原则是「结构强一致、仅值按分支不同」。把**选择**下沉到分支、把**声明**留在结构，与产品原则一致。方向正确。

### 2.2 概念问题与决策（用户已拍板）

| # | 决策点 | 结论 | 理由 |
| --- | --- | --- | --- |
| Q1 | 结构页如何声明「引用共享」 | **保留真实类型 + 独立标记** `ItemDef.shared: bool`（不加 `shared` 到 ValueType 枚举） | `ValueType` 与数据面 proto `config.v1.ValueType` 及 `Value.ty` 共用同一枚举；加 `shared` 会污染值类型空间、结构失去类型约束、波及 proto/三语言 SDK 契约。保留真实类型后：结构仍约束（分支下拉只列类型匹配的共享项），proto/Value/渲染零改动 |
| Q2 | 引用选择存在位置 | **仅分支**：`BranchState.shared_bindings: {group → {key → 共享项 key}}`（单一事实来源） | 完全符合「分支决定引用」；不做「结构默认值 + 分支覆盖」（两个事实来源、优先级/级联复杂） |
| Q3 | 存量数据迁移 | **不迁移**，全新数据目录 | 与 shared-ref-rework §2.2 一致（dev 阶段、无存量部署）；serde 忽略旧 `shared_ref` 字段，旧结构反序列化为全本地项 |

## 3. 目标数据模型

### 3.1 ItemDef（结构，model.rs:184）

```rust
pub struct ItemDef {
    pub key: String,
    #[serde(rename = "type")]
    pub ty: ValueType,            // 保留真实类型：分支下拉按此过滤共享项
    #[serde(default)]
    pub required: bool,           // shared=true 时语义上无意义（validate_publish 跳过），UI 置灰
    #[serde(default)]
    pub secret: bool,             // shared=true 时无意义（掩码由实际共享项决定），UI 置灰
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validate: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// 引用共享标记：true = 本项值为共享来源，由各分支在 shared_bindings 中选择引用哪个共享项。
    #[serde(default, skip_serializing_if = "is_false")]
    pub shared: bool,
}
```

- **删除** `shared_ref` 字段。新增 `fn is_false(b: &bool) -> bool { !*b }`（serde 辅助，与 `Vec::is_empty` 模式一致）。
- serde default + skip_serializing_if 保证旧结构数据（无 `shared` 字段）反序列化兼容 → `shared: false`（全部本地项）。

### 3.2 BranchState（分支，model.rs:263）

```rust
pub struct BranchState {
    // ... 既有字段不动 ...
    /// 分支级共享引用绑定：group → key → 共享项 key（仅对结构标记 shared=true 的 item 有意义）。
    #[serde(default)]
    pub shared_bindings: BTreeMap<String, BTreeMap<String, String>>,
    /// 绑定是否有未发布的变更（发布守卫 + 级联判定用）。值草稿用「非空」判定，绑定必须持久化
    /// （见 §4.6），故用脏标记区分「有无待发布变更」。
    #[serde(default)]
    pub bindings_dirty: bool,
}
```

`BranchState::new()` 初始化两个新字段为默认值。

### 3.3 未变动的实体

- `SharedItem`（key/ty/secret/required/value/version/description）—— 不变。
- `Structure` / `StructureDraft` / `GroupDef` / `Value` —— 不变。

## 4. 语义变更

### 4.1 声明与选择分离

- **声明（结构）**：item 勾选 `shared=true` + 声明真实 `ty`（分支下拉的类型约束）。`required`/`secret` 对 shared 项无意义（置灰）。
- **选择（分支）**：每个分支在草稿页对 shared 项选一个「已发布且类型一致」的共享项（或解除）；选择存入 `shared_bindings`，随草稿保存（同一乐观锁 `draft_rev`）。
- 引用项**只读语义保留**：分支不能为 shared 项写本地值，只能选择引用；值由共享库物化。
- 一个 item 至多绑定一个共享项；一个共享项可被任意多 (项目, 分支, item) 绑定（N:1）。

### 4.2 物化（materialize_resolved，state.rs:1612）

发布（Publish / GrayPublish 共用）时遍历已发布结构的 items：

- `shared = false`：值来自分支草稿（原逻辑）。
- `shared = true`：查 `st.shared_bindings[group][key]`：
  - **无绑定** → 校验错误 `"{group}/{key}: 未选择引用共享项"`（Block 策略拒绝发布，Warn 记录继续，与 G1/D35 一致）。
  - **有绑定 rk** → 取共享库已发布项 `rk` 的值；共享项缺失（悬空引用）→ 校验错误（Block 拒绝，detail 列出 group/key/rk）。
  - **防御性类型复查**：`shared.ty != def.ty`（结构 ty 在绑定后被修改的残留）→ 校验错误，Block 拒绝 / Warn 记录（正常流程不可达，见 §4.7 结构发布已清失配绑定）。

### 4.3 校验

| 位置 | 规则 |
| --- | --- |
| validator::validate_structure | 删除 shared_ref 的 valid_key_name 检查（字段已删）；无新增检查（`shared` 是 bool） |
| validator::validate_publish | `item.shared == true` → **跳过**该 item 的草稿必填/类型校验（值由共享库物化，同现状） |
| apply_draft_update（写值） | item 命中已发布结构且 `shared=true` → 拒绝本地值：`"item {g}/{k} 引用共享项，不可设置本地值"`（同现状，字段换标记） |
| apply_draft_update（写绑定） | 每个绑定 (g, k, rk)：def 须存在且 `shared=true`；`rk` 为空 = 解除；`rk` 非空 → valid_key_name + 共享项已发布存在 + `shared.ty == def.ty`，任一不满足 → validation 错误 |
| apply_publish_structure | 结构草稿保存/发布已无共享键可校验（选择在分支）；标记本身无需校验。**删除 `check_shared_refs`（state.rs:2182，字段已删、无物可验）**，其两处调用点（:1385、:1424）一并移除 |
| apply_branch_create(source) | 见 §4.8 |

### 4.4 级联（apply_shared_publish，state.rs:2029）

- 删除基于结构 shared_ref 的 `shared_usage` 反查（现状实现）。
- 改为**扫描全项目全分支的 `shared_bindings`**，收集绑定 == 本共享项 key 的 (project, branch, group, item_key)，逐个推进**该分支**版本（原 `cascade_to_project` 按项目推进全部分支 → 重构为 `cascade_to_branch(project, branch, group, key, value, ...)`，保留事件/快照/版本推进逻辑，含 `EventType::SharedCascade`）。
- 级联对象无论 `bindings_dirty` 与否都推进（与现状「结构引用即全分支推进」的一致性语义；重复推进无害）。
- Manual 级联模式不变：只更共享版本，引用分支下次发布经物化取新值。
- 复杂度 O(项目数 × 分支数 × 结构项数)/次共享发布 —— dev 规模可接受（现状为 O(项目数 × 结构项数) × 分支数，量级相同）。

### 4.5 删除保护与反向引用

- `shared_usage(key)`（state.rs:2165）改扫全分支 `shared_bindings`，返回 `(ProjectId, BranchName, String, String)`。
- `apply_shared_delete`：被任一分支绑定 → 409，detail 列出 (project, branch, group, item_key)；未绑定 → 删除（幂等，版本快照不受影响）。
- API `GET /api/v1/shared` 的 `refs` 每项增加 `branch` 字段。

### 4.6 发布守卫与绑定持久化（关键实现点）

- **绑定必须跨发布持久化**：`apply_publish`/`apply_gray_publish` 成功后 `value_draft.clear()`（state.rs:1710、1842），但**不清 `shared_bindings`** —— shared 项没有本地值可清，清掉后下次发布即「未选择」。绑定是常驻分支状态，直到用户改绑或结构移除该项。
- **发布守卫**：现状 `if st.value_draft.is_empty() { NoDraft }`（state.rs:1679、1818）会卡死「只改绑定」的分支。改为：
  `if st.value_draft.is_empty() && !st.bindings_dirty { NoDraft }`。
- `bindings_dirty`：apply_draft_update 实际改动绑定 → true；publish/gray_publish 成功 → false；结构发布不触碰。

### 4.7 结构发布清理（apply_publish_structure，state.rs:1394）

每分支循环内（保留既有清理）：

- **草稿值清理**：既有循环（state.rs:1488-1497）条件从 `item.shared_ref.is_some()` 改为 `item.shared`（覆盖 local→shared 翻转时清除旧本地值）。
- **绑定清理（新增）**：构造新结构的 shared 项集合 `{(group, key, ty)}`；`st.shared_bindings.retain(...)` 仅保留仍在结构中、仍 `shared=true`、且绑定共享项类型与新结构 `ty` 一致的条目（删除 item / shared→local 翻转 / **ty 变更致失配** → 绑定丢弃，分支需重新选择）。
- `bindings_dirty` 不触碰。

### 4.8 分支创建(source) 与灰度

- `apply_branch_create`（state.rs:1279）：复制源分支活动快照进 value_draft 时，**跳过结构标记 `shared=true` 的 item**（避免把物化后的共享值变成引用项的「本地草稿」，否则下次草稿保存被拒）；同时**继承源分支的 `shared_bindings`**（clone），`bindings_dirty = false`（继承即生效，非新变更）。
- 灰度（GrayPublish/GrayPromote/GrayAbort）与稳定发布共用 `materialize_resolved`，自动一致；只改绑定的灰度发布同样走新守卫。

### 4.9 数据面 / 渲染 / SDK

- 引用在发布时已物化进版本快照（版本自包含、不可变）—— SDK 数据面（HTTP/gRPC/SSE）**零改动**。
- 渲染引擎只消费 SnapshotMap —— description 不进入输出（不变）。
- secret 掩码语义：物化值携带共享项自身的 secret 属性 → 同一 key 在不同分支可呈现不同掩码（分支级引用的自然结果，接受）。

## 5. 存储布局变更

| 键 | 现状 | 变更后 |
| --- | --- | --- |
| `p/{pid}/b/{branch}`（BranchState） | 无绑定字段 | + `shared_bindings`、+ `bindings_dirty`（serde default，旧数据兼容） |
| 结构（Structure/StructureDraft） | ItemDef.shared_ref | ItemDef.shared（bool） |

无键布局变化（绑定内嵌分支状态，不新增存储键）。

## 6. API 契约变更（openapi.v1.yaml）

1. **ItemDef**：删除 `shared_ref`；新增 `shared: bool`（default false）；补上缺失的 `description`（≤200 字节，上轮 rework §6.3 承诺未同步 —— 顺带修复）。
2. **Branch（branch detail 响应）**：`shared_refs` 语义改为「结构 shared 项 × 本分支绑定」解析：
   - 已绑定 → `{ group, key, shared_key: "rk", version: n, value: {...掩码...} }`
   - 未绑定 → `{ group, key, shared_key: "", version: null, value: null }`（UI 据此渲染未选择状态）
   - 附注：`shared_refs` 现包含**全部** shared 项（含未绑定），供草稿页渲染下拉。
3. **DraftUpdate 请求**：新增 `shared_bindings: [{ group, key, shared_key }]`（`shared_key` 空串 = 解除绑定）。
4. **SharedItem.refs**：每项新增 `branch` 字段。
5. **StructureDraft / Structure**（GroupDef → ItemDef）随 1 同步。
6. 兼容性：dev 阶段 breaking change，删除/新增字段直接生效；api-surface-test.sh 与 http_project_admin.rs 断言同步更新（§8）。

## 7. Admin UI 变更

### 7.1 结构编辑器（app.js structItemRowHtml，:1199-1217）

- 「共享引用」下拉 → 替换为「引用共享」**勾选框**（data-sf="ishared"）。
- 勾选后：`required`/`secret` 控件置灰（title：由各分支所选的共享项决定）；`ty` 保持可编辑（分支下拉的类型约束）；行内提示「分支草稿页将按此类型显示共享项下拉」。
- `collectStructDraft` / `serializeGroups` / `validateGroups` / JSON 模式（struct-draft textarea）同步支持 `shared`（去掉 shared_ref 收集与 NAME_RE 校验，:1284）。
- 组头信息（:623-624）：改为「x 项 · y 引用共享」（判定条件 `it.shared`）。

### 7.2 草稿页（app.js renderDraftEditor，:598-651）

- 删除 `draftRefs` 未发布引用 hack（:612-614，:627-634）—— 新模型下结构草稿的 shared 标记未发布即不生效，草稿页只按已发布结构渲染。
- 对已发布结构 `it.shared === true` 的 item 渲染**绑定行**（替代原只读 sharedRefRowHtml）：
  - key + 「引用共享」徽标；ty 列显示 `it.type`；
  - `<select class="draft-shared-bind" data-g data-k>`：选项 = `S.sharedItems` 过滤 `s.type === it.type`（secret 项带锁图标/徽标），值为当前绑定（来自 `b.shared_refs` 的 `shared_key`，未绑定显示「— 请选择 —」）；
  - 下方展示物化值（来自 shared_refs 的 value）或「未选择共享项」占位。
- 保存草稿：收集 `.draft-shared-bind`（全部 shared 行，含空选择 = 解除）→ `shared_bindings` 载荷；值变更/绑定变更均置 `S.draftDirty`。
- `loadShared` 刷新 `S.sharedItems` 后重渲染下拉（与现状同源）。

### 7.3 共享库页（view-shared）

- 「被引用」列 tooltip 增加 branch：`project/branch/group/item_key`；计数不变（绑定数）。

## 8. 测试与脚本同步

| 项 | 变更 |
| --- | --- |
| dsh-core/tests/state_machine.rs | 既有 shared_ref 测试改写：结构 shared=true + 分支绑定（:2808、:2904、:2762 project_delete 等）；新增：①**分支差异化绑定**（dev→A、prod→B，发布快照各自取值——核心场景）②未绑定发布阻断（Block/Warn）③绑定类型不一致拒绝 ④绑定未发布共享项拒绝 ⑤只改绑定可发布（守卫）⑥绑定跨发布持久化 ⑦结构发布清理绑定（删 item / shared↔local 翻转）⑧共享发布级联只推绑定分支 ⑨共享删除被分支绑定 409（detail 含 branch）⑩BranchCreate(source) 跳过 shared 值 + 继承绑定 ⑪shared 项写本地值被拒 |
| dsh-core/tests/model_serde.rs / project_admin.rs | `shared_ref: None` → `shared: false`（构造器） |
| dsh-api/tests/http_project_admin.rs | branch detail shared_refs 形状（含未绑定项）；DraftUpdate shared_bindings；SharedItem.refs 含 branch |
| scripts/api-surface-test.sh | ItemDef/结构 JSON 去 shared_ref 加 shared；DraftUpdate 加 shared_bindings 断言；shared_refs 形状 |
| 手动验证清单 | dev-single 全流程：建共享项 A/B（同类型）→ 结构勾「引用共享」→ dev 绑 A / prod 绑 B → 各自发布 → 快照值不同 → 共享项 A 更新发布级联仅 dev → 删除被引用共享项 409 |

## 9. 文档同步

- api/openapi.v1.yaml（§6）
- schema/storage.v1.schema.json（ItemDef.shared；BranchState.shared_bindings/bindings_dirty）
- README.md「核心能力」共享配置描述（结构声明 + 分支选择）
- 教程 docs/03-structure.md（§3.3 共享引用改「分支选择」）、04-draft.md（引用行 → 下拉行）、06-shared.md（级联语义 + 被引用计数）
- dev_docs/design/shared-ref-rework.md 标注「引用语义被本设计取代」；design-v2.md §4.6-4.7 / design-modules/01-core.md 引用表同步

## 10. 风险与边界

| 风险 | 处置 |
| --- | --- |
| 悬空绑定（绑定指向已删共享项） | 删除被绑定共享项 409 ⇒ 正常流程不可达；异常残留 → 发布阻断并给出明细 |
| 未绑定 shared 项 | 发布阻断（Block，明细列出）/ Warn 记录继续；UI 下拉「请选择」提示 |
| 结构 ty 变更致既有绑定失配 | 结构发布时丢弃失配绑定（§4.7），分支重新选择；物化期类型复查兜底（§4.2） |
| 每分支重复选择（UX 摩擦） | 接受（Q2 决策）；BranchCreate(source) 继承绑定缓解；下拉按类型过滤减少误选 |
| 级联扫描成本 | O(项目×分支×结构项)/次，dev 规模可接受；预留派生索引优化点（注释标注，同 rework） |
| secret 分支差异化 | 接受（分支级引用自然结果）；UI 下拉带 secret 徽标 |
| 旧数据目录 | 不迁移（Q3）；升级指南注明旧 shared_ref 数据被忽略 |
| 只改绑定的分支被 NoDraft 卡死 | bindings_dirty 守卫（§4.6），测试覆盖 |
| 发布后绑定被清导致下次悬空 | 绑定常驻（§4.6），测试覆盖 |
| 灰度 | 与稳定发布共用物化/守卫路径，一致 |

## 11. 开发任务清单

> 顺序即依赖序；每项完成后 `cargo test` 通过再进入下一项。最终统一做文档同步与全量验证。

| # | 任务 | 主要文件 | 验证 |
| --- | --- | --- | --- |
| T1 | ItemDef：`shared_ref` → `shared: bool`（+is_false 辅助）；BranchState：+`shared_bindings` +`bindings_dirty`（+new()）；keys.rs 注释同步；修全部构造器（testkit/jobs/publish/model_serde/project_admin/cluster） | model.rs、keys.rs、dsh-testkit、dsh-jobs、dsh-publish、tests | cargo build + cargo test（编译期全量修正） |
| T2 | Command：DraftUpdate + `shared_bindings: Vec<SharedBinding>`（serde default，旧日志兼容） | command.rs | cargo build |
| T3 | validator：validate_structure 删 shared_ref 检查；validate_publish 跳过 shared 项 | validator.rs | cargo test |
| T4 | state：apply_draft_update —— shared 项拒本地值 + 绑定应用（存在性/类型校验/解除/脏标记） | state.rs | 新增绑定校验单测 |
| T5 | state：materialize_resolved 按分支绑定物化（未绑定/悬空 → 错误） | state.rs | 物化单测 |
| T6 | state：apply_publish / apply_gray_publish 守卫改造（value_draft ∥ bindings_dirty）+ 成功后清 bindings_dirty | state.rs | 只改绑定发布单测 |
| T7 | state：apply_publish_structure —— 草稿值清理条件换 shared + 绑定清理 retain（含 ty 失配丢弃）+ 删除 check_shared_refs 及两处调用 | state.rs | 结构翻转清理单测 |
| T8 | state：apply_branch_create(source) 跳过 shared 值 + 继承绑定 | state.rs | 分支创建单测 |
| T9 | state：绑定反查扫描 + cascade_to_branch 重构 + apply_shared_publish / apply_shared_delete / shared_usage 改造 | state.rs | 级联/删除单测 |
| T10 | state_machine.rs 全部既有 shared_ref 测试改写 + 新增测试（§8 清单） | tests/state_machine.rs | cargo test |
| T11 | API：DraftUpdateReq + update_draft 透传；branch_detail shared_refs 含未绑定项；list_shared refs 加 branch；delete 409 detail 加 branch | dsh-api/src/lib.rs | cargo test + curl |
| T12 | 契约：openapi.v1.yaml（ItemDef/shared_refs/DraftUpdate/refs）+ storage.v1.schema.json + api-surface-test.sh | api/openapi.v1.yaml、schema、scripts | bash scripts/check-contracts.sh + api-surface-test.sh |
| T13 | Admin UI：结构编辑器勾选改造 + 草稿页绑定下拉行 + 共享库 tooltip | admin/app.js、index.html、styles.css | dev-single 手动验证 |
| T14 | 文档同步（§9）+ README + 教程三章 | docs/、README.md、dev_docs | 全文审读 |
| T15 | 全量验证：cargo test --workspace + dev-single 全流程 + cluster-demo | — | 见 §8 手动清单 |

## 12. 变更文件清单（预估）

- server/crates/dsh-core/src/{model,command,validator,state,keys}.rs
- server/crates/dsh-core/src/lib.rs（如需导出 is_false）
- server/crates/dsh-core/tests/{state_machine,model_serde,project_admin}.rs
- server/crates/dsh-testkit/src/lib.rs、server/crates/dsh-jobs/src/lib.rs、server/crates/dsh-publish/src/lib.rs
- server/crates/dsh-raft/tests/cluster.rs（如引用 shared_ref 构造）
- server/crates/dsh-api/src/lib.rs、server/crates/dsh-api/tests/http_project_admin.rs
- server/crates/dsh-api/admin/{app.js,index.html,styles.css}
- api/openapi.v1.yaml、schema/storage.v1.schema.json
- scripts/api-surface-test.sh
- README.md、docs/{03-structure,04-draft,06-shared}.md、dev_docs/design/shared-ref-rework.md（标注取代）
