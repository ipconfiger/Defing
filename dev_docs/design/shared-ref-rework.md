# 设计文档：共享配置引用关系重构（shared-ref rework）

状态: v1 待审核
日期: 2026-08-20
范围: dsh-core / dsh-api / admin UI / openapi / storage schema / 测试脚本
关联文档: dev_docs/design-v2.md §4.6-4.7、dev_docs/design-modules/01-core.md、dev_docs/design/storage-redb-migration.md（数据迁移惯例）

---

## 1. 背景与问题清单（用户提出的 5 个问题，逐一落实）

| # | 问题（用户原话要点） | 根因（现状代码定位） | 解决方案（本文档） |
| --- | --- | --- | --- |
| P1 | 配置管理页面添加配置项时，无法让新添加的项引用共享配置项（希望用下拉菜单选择） | 配置管理页的「结构」编辑器 item 行只有 key/type/required/secret，没有引用入口；引用只能去共享库页填表单绑定 | 结构编辑器 item 行新增「共享引用」下拉（选项 = 已发布共享项），选中即建立引用（§4.1、§7.2） |
| P2 | 共享库里多了「引用绑定」，关系做反了；共享配置只管增删改查，应由项目配置决定哪些配置项引用共享项 | `RefBinding`（model.rs:416）由共享库页表单（index.html「引用绑定」卡片）管理，POST /api/v1/shared/refs 全局可绑 | 删除 RefBinding 实体 / RefBind/RefUnbind 命令 / /api/v1/shared/refs 端点 / 共享库页「引用绑定」卡片；引用关系内嵌到项目配置项定义 `ItemDef.shared_ref`（§4、§5、§6、§7.1） |
| P3 | 项目配置项与共享配置项都缺少「描述」字段（助记用，不渲染进配置文件） | `ItemDef`（model.rs:184）与 `SharedItem`（model.rs:403）均无 description 字段 | 两者均新增 `description`；渲染引擎只消费 SnapshotMap（group→key→Value），描述天然不会进配置文件（§4、§8.1） |
| P4 | 共享配置项不需要分组 | `SharedItem.group` 字段 + 存储键 `sh/{group}/{key}` 两层结构 | SharedItem 移除 group，共享库扁平化为 `sh/{key}`（§4.2、§5） |
| P5 | 配置管理中似乎缺分组管理；每个项目的分组应独立 | 组 CRUD 已存在于「结构」页（app.js addStructGroup/delStructGroup/renameStructGroup，每项目独立，结构本身按项目隔离），但入口只此一处，可发现性差 | 保留现有组 CRUD；草稿页组卡片头增加「管理分组」入口跳转结构页；结构页组头补充组信息展示（§7.3） |

## 2. 决策记录

### 2.1 用户确认的决策（2026-08-20 提问确认）

| 决策点 | 结论 |
| --- | --- |
| 共享引用的设置位置 | 结构编辑器 item 行内「共享引用」下拉（项目配置项定义级，全分支生效） |
| 引用项可否被分支草稿覆盖 | **不可**。引用项只读：值完全来自共享项，分支草稿不能为引用项设置本地值 |
| 整组引用（item_key=null 的组级绑定） | **移除**。只保留单项引用（每个配置项独立下拉选择） |
| 分组管理形态 | 提升可发现性（沿用现有组 CRUD，不新增独立视图） |

### 2.2 本文档补充决策（审核时请一并确认）

| 决策点 | 结论 | 理由 |
| --- | --- | --- |
| 数据迁移 | **不做**。旧数据目录（含旧共享分组/旧 refs）直接失效，全新启动 | 与 storage-redb-migration.md 的既有惯例一致（dev 阶段产品、无存量部署）；引用结构内嵌后旧 refs 无法无损转换，迁移成本 >> 收益 |
| 共享项删除 | 新增 DELETE 能力（现 API 无任何删除共享项的端点） | 用户要求共享库「只管增删改查」；删除已发布且被引用的共享项 → 409 并列出引用方（§5.3） |
| 引用项的类型/required/secret | 继承共享项定义；结构编辑器中选中引用后这三项只读显示共享项属性；若本地声明 type 与共享项不一致 → 校验错误 | 引用项值由共享项产出，本地定义无意义；尽早失败避免发布期意外 |
| 反向引用展示 | GET /api/v1/shared 每项内嵌 `refs`（引用它的 project/group/item_key 列表，扫描项目已发布结构） | 共享库页可展示「被哪些项目引用」，删除阻断与级联预览共用同一扫描 |
| 结构草稿中引用已发布共享项的校验时机 | 保存结构草稿（PUT structure-draft）与发布结构（publish）均校验：shared_ref 必须存在且 type 一致 | 草稿保存期尽早反馈（与现 RefBind 在绑定期校验一致性） |
| description 上限 | 200 字节，自由文本（不套用 valid_key_name 字符集，允许中文/空格/标点） | 助记用途；渲染与校验均不受影响 |

## 3. 目标数据模型

### 3.1 ItemDef（项目配置项，model.rs:184）

```rust
pub struct ItemDef {
    pub key: String,
    #[serde(rename = "type")]
    pub ty: ValueType,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub secret: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validate: Option<String>,
    /// 新增：助记描述（自由文本 ≤200 字节；不进入渲染输出）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// 新增：引用共享项（共享库扁平化后的共享项 key；None = 本地项）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shared_ref: Option<String>,
}
```

- `shared_ref` 非 None 时：`required`/`secret`/`type` 语义上继承共享项（校验见 §4.3）；`validate` 忽略。
- serde default + skip_serializing_if 保证旧结构数据（无新字段）反序列化兼容。

### 3.2 SharedItem（共享项，model.rs:403）

```rust
pub struct SharedItem {
    /// 移除 group（P4：共享库扁平化）
    pub key: String,
    pub ty: ValueType,
    #[serde(default)]
    pub secret: bool,
    #[serde(default)]
    pub required: bool,
    pub value: Value,
    pub version: u64,
    /// 新增：助记描述（自由文本 ≤200 字节；不进入渲染输出）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}
```

### 3.3 删除的实体

- `RefBinding`（model.rs:416-423）——整实体删除。
- `Command::RefBind` / `Command::RefUnbind`（command.rs:141-155）——整变体删除（数据迁移不做 ⇒ 无旧日志重放问题，与 rocksdb→redb 同惯例：集群同步升级、全新数据目录）。

## 4. 语义变更

### 4.1 引用建立（替代旧 RefBind）

- 引用是 `ItemDef.shared_ref`，随结构草稿保存、随结构发布成为已发布结构的一部分（结构版本不可变，引用随版本固化）。
- 一个项目 item 至多引用一个共享项；一个共享项可被任意多项目/多 item 引用（N:1）。
- 引用只指向**已发布**的共享项（下拉只列已发布项；结构保存/发布双重校验，§4.3）。

### 4.2 物化（materialize_resolved，state.rs:1598）

发布（Publish / GrayPublish / StructurePublish 共用路径）时：

- 遍历已发布结构的 items：
  - `shared_ref = None`：值来自分支草稿（原逻辑）。
  - `shared_ref = Some(key)`：值 = 共享库已发布项 key 的值；**忽略分支草稿中该 item 的值**（只读语义）。
    - 共享项缺失（悬空引用）→ `ERR_PUBLISH_BLOCKED`（detail 列出 group/key/shared_key），Block 策略拒绝发布（Warn 策略记录继续，与 G1/D35 一致）。
- 删除对旧 `read_refs_of_project` 的引用循环（§5.2 存储一并删除）。

### 4.3 校验

| 位置 | 规则 |
| --- | --- |
| validator::validate_structure | 若 `shared_ref` 为 Some：须通过 valid_key_name（与共享项 key 同字符集） |
| validator::validate_publish | **跳过 shared_ref 项**：不参与草稿必填/类型校验（值由共享库物化，草稿中本就不该有值） |
| apply_structure_draft_set（保存草稿） | 每个 shared_ref：共享库已发布存在；本地声明 type == 共享项 type（不一致 → validation 错误） |
| apply_publish_structure（发布结构） | 同上校验（草稿期已校验，双保险防绕过） |
| apply_draft_update（写值草稿） | item 命中已发布结构且 `shared_ref` 非 None → 拒绝本地值：`validation("item {g}/{k} 引用共享项，不可设置本地值")` |
| apply_publish_structure（D14 扩展） | 发布时清理 `shared_ref` 项的既有草稿值（与已删除 item 的清理同批，防只读项残留草稿） |
| promote（值提升） | 源活动版本值写入目标草稿时，跳过 shared_ref 项（列入 skipped，UI 提示），避免触发上述拒绝 |

### 4.4 共享发布级联（apply_shared_publish，state.rs:2061）

- 删除基于 `idx/ref` / `idx/refg` 索引的反查。
- 改为：发布每个共享草稿项（key K）后，扫描**全项目已发布结构**，收集 `items.shared_ref == K` 的 (project, group, item_key)，逐个 `cascade_to_project`（原函数保留，行为不变：版本推进 + SharedCascade 事件）。
- 复杂度 O(项目数 × 结构项数) 每次共享发布——dev 规模可接受；预留优化：结构发布时维护 `idx/shref/{shared_key}/{project}/{group}/{item_key}` 派生索引（本文档不实现，注释标注）。
- Manual 级联模式（--shared-cascade=manual）语义不变：只更共享版本，引用分支下次发布时经物化取新值。

### 4.5 引用项在数据面

- 引用在发布时已物化进版本快照（版本自包含、不可变，design-v2 §4.6 语义不变）；SDK 数据面（HTTP/gRPC/SSE）零改动。
- 渲染引擎（dsh-render）只消费 SnapshotMap ⇒ description 不会进入 YAML/TOML/JSON 输出（P3 的"不渲染"要求天然满足）。

## 5. 存储布局变更

### 5.1 键变更（keys.rs）

| 键 | 现状 | 变更后 |
| --- | --- | --- |
| 共享项 | `sh/{group}/{key}` | `sh/{key}` |
| 共享草稿 | `sh-draft/{group}/{key}` | `sh-draft/{key}` |
| 项目引用绑定 | `p/{pid}/refs/{group}[/{key}]` | 删除（引用内嵌结构） |
| 引用反查索引 | `idx/ref/{sg}/{sk}/...`、`idx/refg/{sg}/...` | 删除（级联改为扫描结构，§4.4） |

- `shared_prefix(group)`（组前缀扫描）删除；单个共享项直接 `sh/{key}` 精确键。
- `get_shared(group, key)` → `get_shared(key)`；`list_shared_published/list_shared_drafts` 前缀扫描 `sh/` / `sh-draft/` 不变（后缀去一层）。

### 5.2 state.rs 删除项

- `apply_ref_bind` / `apply_ref_unbind` / `read_refs_of_project` / `ref_index_key` / `group_ref_index_key`。
- 命令分发 match 分支（RefBind/RefUnbind）同步移除。

### 5.3 新增共享项删除（补全 CRUD 的 D）

| 端点 | 语义 |
| --- | --- |
| DELETE /api/v1/shared-draft/{key} | 删除共享草稿（无已发布版本时等同删除该项草稿；幂等） |
| DELETE /api/v1/shared/{key} | 删除已发布共享项（连同草稿）：若被任一项目结构引用 → 409 Conflict，detail 列出 (project, group, item_key)；未被引用 → 删除。已发布版本快照不受影响（版本自包含）。审计 action 新增 `shared_delete`（schema audit action enum 同步） |

- 命令层新增 `Command::SharedDelete { key, operator }`（草稿/已发布由 apply 按存在性处理，幂等）。

## 6. API 契约变更（openapi.v1.yaml）

### 6.1 SharedItem 请求/响应

- `SharedItem`：删除 `group`；新增 `description`（optional string ≤200）。
- `SharedItemReq`：删除 `group`；新增 `description`。POST /api/v1/shared 与 PUT /api/v1/shared-draft 请求体同（现状两端点同语义，保持）。
- GET /api/v1/shared 每项新增 `refs: [{project, group, item_key}]`（反向引用，扫描结构计算；GET /api/v1/shared-draft 不返回 refs——草稿项尚未生效）。

### 6.2 删除

- `/api/v1/shared/refs`（GET/POST/DELETE）、`RefBinding`、`RefBindingReq`、`RefUnbindReq` schema。

### 6.3 新增

- DELETE /api/v1/shared/{key}、DELETE /api/v1/shared-draft/{key}。
- `ItemDef`：新增 `shared_ref`（optional string，引用共享项 key）、`description`（optional string ≤200）。
- GET /api/v1/projects/{p}/branches/{b}/draft 响应新增 `shared_refs: [{group, key, shared_key, version, value}]`（服务端解析引用项的共享值，供草稿页只读展示；secret 值 masked，与共享库列表一致）。

### 6.4 兼容性

- dev 阶段 breaking change：删除端点/字段直接生效；api-surface-test.sh 与 http_project_admin.rs 断言同步更新（§8）。

## 7. Admin UI 变更

### 7.1 共享库页（view-shared）

- 删除「引用绑定」卡片（index.html:374-403、app.js bindRef）。
- 「新增 / 更新共享项」卡片：删除「组」输入（sh-group）；新增「描述」输入；保存请求体去 group。
- 共享项列表（shared-body）：删除「组」列；新增「描述」列与「被引用」列（refs 数量，title 悬浮展示 project/group/item_key）；行内新增删除按钮（草稿项删草稿、已发布项删发布，409 时 toast 展示引用方明细）。
- 发布共享、刷新、草稿/已发布合并展示逻辑不变。

### 7.2 配置管理页 · 结构编辑器（pane-structure）

- item 行（structItemRowHtml）新增两列控件：
  - 「描述」：`<input>`（data-sf="idesc"），≤200 字节。
  - 「共享引用」：`<select>`（data-sf="ishref"），选项 = 「—」（无）+ 已发布共享项 key（label 带描述 title；数据源 GET /api/v1/shared 与现有 defs 级联同源加载）。
- 选中共享引用后：该行 type/required/secret 控件置灰只读，显示共享项继承的 type/required/secret（行内提示）；取消引用恢复可编辑。
- collectStructDraft / serializeGroups / validateGroups / JSON 模式（struct-draft textarea）同步支持 description 与 shared_ref。
- 组头（struct-ghead）补充信息：组内「x 项 · y 引用共享」。

### 7.3 配置管理页 · 草稿页（pane-draft，P5 可发现性）

> 修订（2026-08-20 用户反馈）：草稿页范式由「添加配置项」改为**结构驱动的全量编辑**——key 由结构定义故保持下拉/只读，草稿一次性展示已发布结构的全部组与配置项，直接改值保存（空值 = 删除该草稿值）。原「添加配置项」卡片及其级联代码已移除。

- 组卡片头新增「管理分组」按钮（data-act="manageGroups"，跳转结构 pane 并定位该组，沿用现有组 CRUD）。
- 草稿页按已发布结构（GET /structure）全量渲染：每个本地项显示 key/type/required/secret/描述 + 可编辑值控件（有草稿值则回填，secret 留空不修改）；shared_ref 项以只读行展示：徽标「引用共享项 {key} · v{version}」+ 共享值（secret masked）。
- 保存草稿：有值 → upsert；原草稿有值但清空 → delete；乐观锁 expected_draft_rev 不变。
- 分支「查看配置」预览不变（发布快照已含物化值）。

### 7.4 状态与数据源

- S 增加 `sharedItems: []`（已发布共享项，结构编辑器下拉 + 草稿页只读展示共用；随 loadProject/loadShared 刷新）。

## 8. 测试与脚本同步

| 项 | 变更 |
| --- | --- |
| dsh-core/tests/state_machine.rs | 现有 ref bind/unbind 测试删除；新增：结构 shared_ref 物化（值=共享值）、引用项草稿写入被拒、结构发布清理引用项草稿值、共享发布级联（扫描结构命中）、悬空引用发布阻断、type 不一致校验失败、共享项删除（被引用 409 / 未引用成功） |
| dsh-api/tests/http_project_admin.rs | 移除 /api/v1/shared/refs 断言（:265-271, :279-283）；shared CRUD 断言去 group、加 description、加 delete 用例 |
| scripts/api-surface-test.sh | :67-84 更新：shared 请求体去 group、断言去 group；refs 断言（:81-84）删除；补 delete 断言 |
| scripts/seed-demo.sh | 检查 shared 相关片段同步（grep 确认） |
| 手动验证清单 | dev-single 全流程：建共享项→结构引用→发布→草稿页只读展示→共享发布级联→删除阻断 |

## 9. 文档同步

- api/openapi.v1.yaml（§6）
- schema/storage.v1.schema.json（SharedItem 去 group 加 description；ItemDef 加 shared_ref/description；删 RefBinding；SharedDraft 去 group）
- README.md「核心能力」共享配置描述更新
- dev_docs/design-v2.md §4.6-4.7、dev_docs/design-modules/01-core.md 引用/命令表同步（如审核通过后执行）

## 10. 风险与边界

| 风险 | 处置 |
| --- | --- |
| 悬空引用（shared_ref 指向不存在的共享项） | 结构保存/发布双校验 + 删除共享项时被引用 409 ⇒ 正常流程不可达；异常残留时发布阻断并给出明细 |
| 旧数据目录失效 | 已确认不做迁移（§2.2），升级指南注明 |
| 级联扫描成本 | O(项目数 × 结构项数)/次共享发布，dev 规模可接受；预留派生索引优化点（§4.4） |
| secret 共享项 UI 展示 | 沿用 masked + reveal 审计路径 |
| 老结构 JSON（无新字段） | serde default 兼容；旧结构无 shared_ref ⇒ 全部本地项，行为不变 |
| promote 触碰引用项 | 跳过并列入 skipped（§4.3），UI 提示 |
| 灰度发布 | 与稳定发布共用 materialize_resolved，引用物化行为一致（§4.2） |

## 11. 变更文件清单（预估）

- server/crates/dsh-core/src/{model,command,keys,state,validator,lib}.rs
- server/crates/dsh-core/tests/state_machine.rs
- server/crates/dsh-api/src/lib.rs（shared 处理器、路由、SharedItemReq、shared_item_json、draft 响应扩展、delete 处理器）
- server/crates/dsh-api/tests/http_project_admin.rs
- server/crates/dsh-api/admin/{index.html,app.js}
- api/openapi.v1.yaml、schema/storage.v1.schema.json
- scripts/api-surface-test.sh、（如涉及）scripts/seed-demo.sh
- dev_docs/README.md 等文档
