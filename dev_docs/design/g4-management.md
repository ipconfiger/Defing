# 设计文档：G4 灰度管理面 + Admin UI

> 状态：**已完成**（2025-08-16）｜ 基线：main `ae8b475`（G3 已落地，4 管理端点 + 审计已就绪）
> 验收：api-surface 灰度断言组 12 项全过；UI 内嵌页面实测（灰度卡 + 4 功能）；contract 43 paths；workspace 31 套件全绿
> 前置：[gray-release.md](gray-release.md)、[g3-dataplane.md](g3-dataplane.md)（D24-D28 + B1/R1-R3 闭环）
> 一句话：**G3 已把灰度端点做进后端；G4 让它可操作、可见、可测——openapi 契约补全 + Admin UI 灰度操作面板 + api-surface 断言。**

---

## 0. 现状分界线（G3 已就绪 vs G4 缺口）

| 能力 | 状态 | 位置 |
|------|------|------|
| `POST …/gray-publish` / `…/gray-promote` / `…/gray-abort` + `GET …/gray-status` + PublishService 3 写方法 + 审计 action（gray_publish/promote/abort） | ✅ G3 | dsh-api lib.rs / dsh-publish |
| 数据面身份分流 + watch gray 事件 | ✅ G3 | state.rs / grpc.rs / lib.rs / dsh-watch |
| **openapi.v1.yaml 灰度路径 + ConfigSnapshot 新字段** | ⬜ | api/openapi.v1.yaml |
| **Admin UI 灰度操作面板**（状态/规则编辑/一键发布/转正/下量） | ⬜ | admin/index.html + app.js |
| **api-surface 断言组**（灰度端点 + 审计覆盖 + 数据面联动） | ⬜ | scripts/api-surface-test.sh |

---

## 1. 决策（D29-D30）

### D29：UI 灰度面板放"项目详情"分支区（与版本历史同卡）

Admin UI 结构：项目 tab → 项目详情 → 分支选择 → 版本历史卡。灰度是**分支级**能力，UI 放
分支选择下方新增"灰度发布"卡，与版本历史并列：

```
┌─ 灰度发布（分支级）──────────────────────────────┐
│ 状态：● 灰度活跃  gray_seq=1  稳定版 v2          │
│      规则：{match_labels:[{zone:cn-north-1}]}    │
│ 规则编辑（JSON）：[textarea 预填当前规则]          │
│ [载入当前] [灰度发布] [一键转正] [一键下量(回滚)]   │
└───────────────────────────────────────────────┘
```

- **规则编辑**：JSON textarea（`{"match_labels":[...],"ip_cidrs":[...],"percentage":N}`），
  预填 gray-status 返回的当前规则；发布前 JSON.parse 校验；
- **一键转正/下量**：直接调用端点（确认弹窗 + 备注输入，复用现有 askModal 模式）；
- **状态刷新**：分支切换/操作完成后调 gray-status 刷新；
- **安全**：沿用现有 `esc()` 转义 + `data-act` 事件委托（D-CSP 纪律，无 inline handler）；
  规则文本区内容转义后渲染（无 HTML 注入面）。

### D30：api-surface 断言组覆盖"灰度全链路"

在 scripts/api-surface-test.sh 追加一节：
1. 灰度发布（空草稿 → 422/409 校验；有草稿 → 成功，返回 gray_seq=1）；
2. gray-status 断言 gray_active/gray_seq/gray_rule；
3. 数据面联动：带身份头 snapshot → gray-host + gray:true + resolved_version=gray_seq；
4. gray-promote → active 推进 + 事件；gray-abort → 回落；
5. 审计断言：gray_publish/gray_promote/gray_abort 三条 action 落库。

---

## 2. 代码改动清单

| 文件 | 改动 | 验收 |
|------|------|------|
| `api/openapi.v1.yaml` | 新增 4 个灰度路径（gray-publish/promote/abort/status）；`ConfigSnapshot` schema 加 `gray`/`resolved_version`；branch 相关说明 | `check-contracts.sh` 过 |
| `server/crates/dsh-api/admin/index.html` | 项目详情内新增"灰度发布"卡（状态区 + 规则 textarea + 4 按钮） | 浏览器实测 |
| `server/crates/dsh-api/admin/app.js` | `loadGrayStatus`/`saveGrayPublish`/`doGrayPromote`/`doGrayAbort`/`doGrayStatusRefresh`；分支加载时联动刷新灰度状态 | 浏览器实测 |
| `scripts/api-surface-test.sh` | 灰度全链路断言组（D30） | 脚本退出 0 |
| `dev_docs/roadmap-p4.md` / `plan-gray-g4.md` / `g4-management.md` | 状态标记 | 文档 |

**明确不做（本期）**：灰度内容明文预览（gray-snap 内容渲染——G5 或后续）；自动回滚决策；
多级灰度；SDK 三语言适配（G3/G4 同步独立排期）。

---

## 3. 验收标准

- `check-contracts.sh` 全过（openapi 路径/字段补齐）；
- Admin UI 浏览器全流程：发布灰度（规则 JSON）→ 状态刷新显示灰度活跃 → 数据面身份头验证
  灰度内容 → 一键转正 → 状态清空 → 一键下量 → 回落；secret 项不受影响；
- `api-surface-test.sh` 灰度断言组全过（含审计 action 覆盖）；
- `cargo test --workspace` + clippy/fmt 全绿；CI 8/8。

## 4. 风险

| 风险 | 对策 |
|------|------|
| UI JSON 规则输入非法 | 前端 JSON.parse 校验 + 服务端 validate_gray_rule（G2 已有）双保险 |
| UI XSS（规则文本/状态回显） | 沿用 esc() 转义 + data-act 委托（D-CSP） |
| openapi 与实现漂移 | contract 检查 + api-surface 断言组对拍 |
| 审计 action 缺失 | api-surface 断言 gray_publish/promote/abort 落库 |
